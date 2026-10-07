//! Taking an app off the node by hand: its container, its sidecars and its network go, it leaves
//! the state and so Caddy, and its history stays. Its directory stays too unless asked otherwise.
//! See spec/architecture/host.md.

use super::Error;
use super::tell::{tell_cron, tell_telemetry};
use crate::Host;
use crate::store::{self, Action, Source};
use std::sync::Arc;

/// What is never removed: host and keeper, which deploy each other, and Caddy and the tunnel, the
/// way in. Each removed would take with it the way to put it back.
const KEPT: [&str; 4] = ["host", "keeper", "caddy", "tunnel"];

/// What a removal did, for the answer.
#[derive(Debug, serde::Serialize)]
pub struct Removed {
	pub name: String,
	/// Whether the app's directory and its snapshots went too.
	pub data_dropped: bool,
	/// Whether Caddy took the configuration without it.
	pub routed: Result<(), String>,
}

pub(super) fn removable(name: &str) -> Result<(), Error> {
	if KEPT.contains(&name) {
		return Err(Error::Kept(name.into()));
	}
	Ok(())
}

/// Remove `name` from this node. With `drop_data`, its directory and its snapshots are deleted as
/// well, and nothing it wrote is left to deploy it again onto.
pub async fn remove(host: &Arc<Host>, name: &str, drop_data: bool) -> Result<Removed, Error> {
	removable(name)?;
	let _one = host.deploying.lock().await;
	let app = host.store.app(name)?.ok_or_else(|| Error::NoSuchApp(name.into()))?;
	let running = store::Outcome::Running;
	let id =
		host.store.record(name, Action::Remove, &Source::panel(), Some(&app.image), running, None)?;
	let done = async {
		host.engine.remove(name).await?;
		for sidecar in app.manifest.sidecars() {
			host.engine.remove(&sidecar).await?;
		}
		host.engine.forget_network(name).await?;
		host.store.forget_app(name)?;
		if drop_data {
			host.volumes.drop_app(name).await.map_err(deploy::replace::Error::from)?;
		}
		Ok::<_, Error>(())
	}
	.await;
	let (outcome, detail) = match &done {
		Ok(()) => (store::Outcome::Succeeded, None),
		Err(error) => (store::Outcome::Failed, Some(error.to_string())),
	};
	host.store.finish(id, outcome, None, detail.as_deref())?;
	done?;
	let routed = super::route(host).await.map_err(|error| error.to_string());
	tell_cron(host).await;
	tell_telemetry(host).await;
	Ok(Removed { name: name.into(), data_dropped: drop_data, routed })
}

#[cfg(test)]
mod tests {
	use super::removable;
	use crate::rollout::Error;

	#[test]
	fn the_way_in_and_the_way_back_are_never_removed() {
		for name in ["host", "keeper", "caddy", "tunnel"] {
			assert!(matches!(removable(name), Err(Error::Kept(_))), "{name}");
		}
		for name in ["panel", "geo", "meter", "resolver"] {
			assert!(removable(name).is_ok(), "{name}");
		}
	}
}
