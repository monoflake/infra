//! What an operator can do to an app: redeploy, roll back, start, stop and restart. See
//! spec/architecture/host.md, "What an operator can do to an app".

use super::Error;
use super::Outcome;
use super::tell::tell_telemetry;
use super::version::{close, run_version, settle};
use crate::Host;
use crate::store::{self, Action, Deployed, Source, Stage};
use deploy::Version;
use deploy::manifest::Manifest;
use std::path::PathBuf;
use std::sync::Arc;

/// The platform's own four: restarted and never stopped, since each stopped takes the way in or the
/// way back with it. See spec/architecture/host.md, "What an operator can do to an app".
pub const PLATFORM: [&str; 4] = ["host", "keeper", "caddy", "tunnel"];

/// What a restart must not wait for: host answering the request, and Caddy carrying it. The caller
/// is told first and the restart follows.
pub(super) const ON_THE_WAY: [&str; 2] = ["host", "caddy"];

/// How long a restart on the way waits, so the answer saying it was asked has left.
pub(super) const ANSWERED: std::time::Duration = std::time::Duration::from_millis(500);

/// host as it runs now, read back from its container's label, in the shape of any app. keeper
/// keeps no record and host none of itself, so it has no previous version here and is never held.
pub async fn itself(host: &Host) -> Result<Option<Deployed>, Error> {
	let fallback = Manifest::parse(include_str!("../../service.toml"))?;
	let Some(version) = host.engine.current("host", &fallback).await? else { return Ok(None) };
	let deployed_at = host.engine.created("host").await?.unwrap_or_default();
	Ok(Some(Deployed {
		manifest: version.manifest,
		image: version.image,
		previous: None,
		deployed_at,
		held: false,
	}))
}

/// An app the panel may act on: one host runs, and not host itself, which cannot stop or replace
/// the program answering the request. See spec/architecture/host.md, "What an operator can
/// do to an app".
pub(super) fn actionable(host: &Host, name: &str) -> Result<Deployed, Error> {
	if name == "host" {
		return Err(Error::Itself);
	}
	host.store.app(name)?.ok_or_else(|| Error::NoSuchApp(name.into()))
}

/// Run the current version again: a deploy of what already runs, which picks up a changed
/// environment.
pub async fn redeploy(host: &Arc<Host>, name: &str) -> Result<Outcome, Error> {
	let _one = host.deploying.lock().await;
	let app = actionable(host, name)?;
	let source = Source::panel();
	let id = host.store.record(
		name,
		Action::Redeploy,
		&source,
		Some(&app.image),
		store::Outcome::Running,
		Some(Stage::Starting),
	)?;
	let result = async {
		let current = Version { manifest: app.manifest.clone(), image: app.image.clone() };
		let snapshot = run_version(host, &current, Some(&current), None).await?;
		host.store.put_app(&Deployed { deployed_at: jiff::Timestamp::now().to_string(), ..app })?;
		host.store.hold(name, false)?;
		Ok(((), snapshot))
	}
	.await;
	close(host, id, &result);
	result?;
	let image = host.store.app(name)?.map(|app| app.image).unwrap_or_default();
	settle(host, name.into(), image).await
}

/// Whether a rollback with data can be offered: the snapshot from before the running version was
/// deployed is still among the ones kept.
pub fn restorable(host: &Host, app: &Deployed) -> Result<Option<PathBuf>, Error> {
	let recorded = host.store.snapshot_before(&app.manifest.name, &app.image)?;
	Ok(recorded.map(PathBuf::from).filter(|path| path.exists()))
}

/// Run the previous version instead of the current one. With `with_data`, the app's directory is
/// also put back as it was before the current version was deployed, and everything written since is
/// lost.
pub async fn rollback(host: &Arc<Host>, name: &str, with_data: bool) -> Result<Outcome, Error> {
	let _one = host.deploying.lock().await;
	let app = actionable(host, name)?;
	let previous = app.previous.clone().ok_or_else(|| Error::NoPrevious(name.into()))?;
	let restore = if with_data {
		Some(restorable(host, &app)?.ok_or_else(|| Error::NoSnapshot(name.into()))?)
	} else {
		None
	};
	let action = if with_data { Action::RollbackWithData } else { Action::Rollback };
	let source = Source::panel();
	let (image, running) = (Some(previous.image.as_str()), store::Outcome::Running);
	let id = host.store.record(name, action, &source, image, running, Some(Stage::Starting))?;
	let result = async {
		let current = Version { manifest: app.manifest.clone(), image: app.image.clone() };
		let snapshot = run_version(host, &previous, Some(&current), restore.as_deref()).await?;
		host.store.put_app(&Deployed {
			manifest: previous.manifest.clone(),
			image: previous.image.clone(),
			previous: Some(current),
			deployed_at: jiff::Timestamp::now().to_string(),
			held: false,
		})?;
		host.store.hold(name, false)?;
		Ok(((), snapshot))
	}
	.await;
	close(host, id, &result);
	result?;
	settle(host, name.into(), previous.image).await
}

/// Start, stop or restart the container as it is. A stop holds the app stopped; a start or a
/// restart ends the hold. The platform's own are restarted and nothing else.
pub async fn act(host: &Arc<Host>, name: &str, action: Action) -> Result<(), Error> {
	permitted(name, action)?;
	let driver = host.store.app(name)?.and_then(|app| host.config.grants.driver_of(&app.manifest));
	if driver.is_some() {
		return Err(Error::Driver(name.into()));
	}
	if ON_THE_WAY.contains(&name) {
		return restart_later(host, name).await;
	}
	let _one = host.deploying.lock().await;
	let app = actionable(host, name)?;
	let source = Source::panel();
	let running = store::Outcome::Running;
	let id = host.store.record(name, action, &source, Some(&app.image), running, None)?;
	// Sidecars are up before their app and down after it. See platform's
	// spec/architecture/objects.md.
	let sidecars = app.manifest.sidecars();
	let done = async {
		for sidecar in &sidecars {
			match action {
				Action::Start => host.engine.start(sidecar).await?,
				Action::Restart => host.engine.restart(sidecar).await?,
				_ => {}
			}
		}
		match action {
			Action::Start => host.engine.start(name).await?,
			Action::Stop => host.engine.stop(name).await?,
			Action::Restart => host.engine.restart(name).await?,
			_ => return Err(Error::NotAnAct),
		}
		if action == Action::Stop {
			for sidecar in &sidecars {
				host.engine.stop(sidecar).await?;
			}
		}
		Ok(())
	}
	.await;
	if matches!(done, Err(Error::NotAnAct)) {
		return Err(Error::NotAnAct);
	}
	let (outcome, detail) = match &done {
		Ok(()) => (store::Outcome::Succeeded, None),
		Err(error) => (store::Outcome::Failed, Some(error.to_string())),
	};
	host.store.finish(id, outcome, None, detail.as_deref())?;
	done?;
	host.store.hold(name, action == Action::Stop)?;
	tell_telemetry(host).await;
	Ok(())
}

/// Whether the panel may do this to the container at all, before anything is asked of Docker.
pub(super) fn permitted(name: &str, action: Action) -> Result<(), Error> {
	if PLATFORM.contains(&name) && action != Action::Restart {
		return Err(Error::Platform(name.into()));
	}
	Ok(())
}

/// A restart of what carries the request: recorded, answered, and only then done. host's own is
/// recorded as done when asked, since nothing of it is left to finish the record once it restarts.
pub(super) async fn restart_later(host: &Arc<Host>, name: &str) -> Result<(), Error> {
	let image = match name {
		"host" => itself(host).await?.map(|app| app.image),
		_ => Some(actionable(host, name)?.image),
	};
	let source = Source::panel();
	let outcome = if name == "host" { store::Outcome::Succeeded } else { store::Outcome::Running };
	let id = host.store.record(name, Action::Restart, &source, image.as_deref(), outcome, None)?;
	let host = host.clone();
	let name = name.to_owned();
	tokio::spawn(async move {
		tokio::time::sleep(ANSWERED).await;
		let _one = host.deploying.lock().await;
		let done = host.engine.restart(&name).await;
		if let Err(error) = &done {
			eprintln!("host: restarting {name}: {error}");
		}
		if name != "host" {
			let (outcome, detail) = match &done {
				Ok(()) => (store::Outcome::Succeeded, None),
				Err(error) => (store::Outcome::Failed, Some(error.to_string())),
			};
			let _ = host.store.finish(id, outcome, None, detail.as_deref());
		}
	});
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::permitted;
	use crate::rollout::Error;
	use crate::store::Action;

	#[test]
	fn the_platforms_own_are_restarted_and_never_stopped_or_started() {
		for name in ["host", "keeper", "caddy", "tunnel"] {
			assert!(permitted(name, Action::Restart).is_ok(), "{name}");
			for action in [Action::Stop, Action::Start] {
				assert!(matches!(permitted(name, action), Err(Error::Platform(_))), "{name}");
			}
		}
		for action in [Action::Stop, Action::Start, Action::Restart] {
			assert!(permitted("geo", action).is_ok());
			assert!(permitted("meter", action).is_ok());
		}
	}
}
