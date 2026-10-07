//! What host tells the services that read its state: telemetry's `services.json` and cron's
//! schedule table. See platform's spec/architecture/telemetry.md and spec/architecture/cron.md.

use super::panel::redeploy;
use crate::Host;
use crate::grants::Role;
use std::sync::Arc;

/// Write telemetry's `services.json` from the state as it now is: at start, and after a deploy, a
/// redeploy, a rollback, a stop or a start. Logged and skipped rather than failing the caller, as
/// `tell_cron` is. See platform's spec/architecture/telemetry.md, "`services.json`, what host
/// tells".
pub async fn tell_telemetry(host: &Host) {
	let Some(reporter) = host.config.grants.holder(Role::Reporter) else { return };
	let directory = host.volumes.data(reporter);
	if let Err(error) = crate::telemetry::write(&host.store, &directory).await {
		eprintln!("host: writing telemetry's services: {error}");
	}
}

/// Write `cron`'s schedule table from the state as it now is: what a deploy, a redeploy or a
/// rollback leaves behind, or what is already there at start. Logged and skipped rather than
/// failing the caller -- as `node::tell` is for the meter -- and skipped outright when `cron` has
/// no directory yet. See platform's spec/architecture/cron.md, "host gives `cron` the table".
///
/// A mount change redeploys `cron` on a spawned background task, never inline -- this runs under
/// `host.deploying`, already held by whoever called it, and taking that lock again would deadlock.
pub async fn tell_cron(host: &Arc<Host>) {
	let apps = match host.store.apps() {
		Ok(apps) => apps,
		Err(error) => {
			eprintln!("host: reading apps for cron's schedule table: {error}");
			return;
		}
	};
	let Some(scheduler) = host.config.grants.holder(Role::Scheduler) else { return };
	let directory = host.volumes.data(scheduler);
	if let Err(error) = crate::cron::write(&apps, host.config.slot, &directory).await {
		eprintln!("host: writing cron's schedule table: {error}");
		return;
	}
	if host.store.app(scheduler).ok().flatten().is_none() {
		return;
	}
	let desired = crate::cron::socket_services(&apps);
	let mounted = match host.engine.socket_mounts(scheduler).await {
		Ok(mounted) => mounted,
		Err(error) => {
			eprintln!("host: reading cron's own mounts: {error}");
			return;
		}
	};
	if !crate::cron::mounts_changed(&desired, &mounted) {
		return;
	}
	let recently = {
		let redeployed_at = host.cron_mount_redeployed_at.lock().unwrap();
		redeployed_at.is_some_and(|at| at.elapsed() < CRON_MOUNT_REDEPLOY_COOLDOWN)
	};
	if recently {
		eprintln!(
			"host: cron's socket services still differ (had {}, want {}) after a recent redeploy for \
			 them; not redeploying again so soon",
			mounted.join(", "),
			desired.join(", ")
		);
		return;
	}
	eprintln!(
		"host: cron's socket services changed (had {}, now {}); redeploying it for its mounts",
		mounted.join(", "),
		desired.join(", ")
	);
	*host.cron_mount_redeployed_at.lock().unwrap() = Some(std::time::Instant::now());
	tokio::spawn(redeploy_cron(host.clone()));
}

/// How long `tell_cron` waits after redeploying `cron` for its mounts before it will do so again,
/// even if the read-back still disagrees -- the guard against the loop a mount-prefix bug once
/// caused, where the redeploy never made the read-back agree and `tell_cron` ran it every time.
const CRON_MOUNT_REDEPLOY_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(300);

/// `redeploy(host, "cron")`, boxed. `tell_cron` runs inside `settle`, which `redeploy` itself ends
/// with, so a plain `async move { redeploy(...).await }` here would make this function's future
/// embed `redeploy`'s, which embeds `settle`'s, which embeds this function's again -- a type with
/// no fixed size. Naming the return type erases it at this one edge, so the cycle closes through a
/// trait object instead of an infinitely nested one.
fn redeploy_cron(
	host: Arc<Host>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
	Box::pin(async move {
		let Some(scheduler) = host.config.grants.holder(Role::Scheduler).map(str::to_owned) else {
			return;
		};
		if let Err(error) = redeploy(&host, &scheduler).await {
			eprintln!("host: redeploying cron for its mounts: {error}");
		}
	})
}
