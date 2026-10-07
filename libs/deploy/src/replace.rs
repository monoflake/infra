//! Replacing one app's container with a new version, which both of the platform's programs do:
//! host for every app and keeper, keeper for host. Stop, snapshot, start, check, and on a failed
//! check put back both the directory and the version before. See spec/architecture/host.md, "One
//! version runs, and a failed deploy puts the last one back".

use crate::Manifest;
use crate::engine::{self, Engine, Shape, Version};
use crate::sidecar::{Driver, Sidecar};
use crate::volume::{self, Volumes};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a new version has to answer its health check when its declaration does not say.
const DEFAULT_DEADLINE: Duration = Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
pub enum Error {
	#[error(transparent)]
	Engine(#[from] engine::Error),
	#[error(transparent)]
	Volume(#[from] volume::Error),
	/// The new version did not become healthy, and the one before it is running again on the
	/// data as it was.
	#[error("{reason}; the previous version is back, with its data as it was\n{logs}")]
	Unhealthy { reason: String, logs: String },
	/// Nothing ran before, so there was nothing to put back.
	#[error("{reason}; there was no previous version to put back\n{logs}")]
	FirstFailed { reason: String, logs: String },
	/// A sidecar did not become healthy on its own, outside any app's deploy.
	#[error("{0}")]
	Sidecar(String),
	/// The new version, started beside the running one, did not take over; the running one was
	/// never touched and is still routed.
	#[error("{reason}; the running version was left as it was\n{logs}")]
	Beside { reason: String, logs: String },
	/// The new version took the route, and putting it in the old one's place failed after.
	#[error("{0}")]
	Switched(String),
}

/// The sidecars each side of a replacement runs beside its app, one per driver it declares.
#[derive(Debug, Clone, Copy, Default)]
pub struct Beside<'a> {
	pub next: &'a [Sidecar],
	pub current: &'a [Sidecar],
}

/// Run `next` in place of `current`, on the app's own network with `members` attached to it, and
/// answer with the snapshot taken of the app's directory before `next` started. With `restore`,
/// the directory is first put back as that snapshot held it -- a rollback with its data -- and a
/// failed check still puts back the directory as it was just before.
pub async fn replace(
	engine: &Engine,
	volumes: &Volumes,
	members: &[&str],
	shape: &Shape,
	next: &Version,
	current: Option<&Version>,
	restore: Option<&Path>,
) -> Result<PathBuf, Error> {
	replace_beside(engine, volumes, members, shape, next, current, restore, Beside::default()).await
}

/// The same, with the app's sidecars stopped before the snapshot, so the snapshot holds their files
/// at rest, and started after any restore and before the app, so the app never meets an endpoint
/// that is not there yet. See platform's spec/architecture/objects.md, "A sidecar per app, over the
/// app's own directory", and platform's spec/architecture/databases.md, "Declared by the app, run
/// beside it".
#[allow(clippy::too_many_arguments)]
pub async fn replace_beside(
	engine: &Engine,
	volumes: &Volumes,
	members: &[&str],
	shape: &Shape,
	next: &Version,
	current: Option<&Version>,
	restore: Option<&Path>,
	beside: Beside<'_>,
) -> Result<PathBuf, Error> {
	putting_back(engine, volumes, members, shape, None, next, current, restore, beside).await
}

/// `replace`, putting the version before back in `back`, the shape it ran in, should the new one
/// fail: keeper recreating host on a changed environment, whose old one is the way back. See
/// spec/architecture/host.md, "host never updates itself; keeper updates host".
pub async fn replace_back(
	engine: &Engine,
	volumes: &Volumes,
	members: &[&str],
	shape: &Shape,
	back: &Shape,
	next: &Version,
	current: &Version,
) -> Result<PathBuf, Error> {
	let beside = Beside::default();
	putting_back(engine, volumes, members, shape, Some(back), next, Some(current), None, beside).await
}

#[allow(clippy::too_many_arguments)]
async fn putting_back(
	engine: &Engine,
	volumes: &Volumes,
	members: &[&str],
	shape: &Shape,
	back: Option<&Shape>,
	next: &Version,
	current: Option<&Version>,
	restore: Option<&Path>,
	beside: Beside<'_>,
) -> Result<PathBuf, Error> {
	let name = next.manifest.name.as_str();
	// Every kind, declared or not: a sidecar the next version no longer declares goes too.
	let sidecars: Vec<String> = Driver::ALL.iter().map(|driver| driver.sidecar_of(name)).collect();
	if shape.networked() {
		engine.network(name, members).await?;
	}
	volumes.ensure(name).await?;

	engine.archive(name, &volumes.logs(name)).await?;
	engine.remove(name).await?;
	for sidecar in &sidecars {
		engine.archive(sidecar, &volumes.logs(sidecar)).await?;
		engine.remove(sidecar).await?;
	}
	let snapshot = volumes.snapshot(name).await?;
	if let Some(restore) = restore {
		volumes.restore(name, restore).await?;
	}
	if let Some((uid, gid)) = engine.user_of(&next.image).await? {
		volumes.hand_over(name, uid, gid).await?;
	}
	let mut beside_next = Ok(());
	for sidecar in beside.next {
		beside_next = start_sidecar(engine, sidecar).await;
		if beside_next.is_err() {
			break;
		}
	}
	let checked = match beside_next {
		Err(reason) => Err(reason),
		Ok(()) => match engine.run(next, shape, &volumes.data(name)).await {
			Ok(()) => healthy(engine, next, shape, &volumes.data(name)).await,
			Err(error) => Err(error.to_string()),
		},
	};
	let Err(reason) = checked else {
		volumes.prune(name).await?;
		return Ok(snapshot);
	};

	let logs = engine.tail(name).await;
	engine.archive(name, &volumes.logs(name)).await?;
	engine.remove(name).await?;
	for sidecar in &sidecars {
		engine.archive(sidecar, &volumes.logs(sidecar)).await?;
		engine.remove(sidecar).await?;
	}
	volumes.restore(name, &snapshot).await?;
	let Some(current) = current else {
		return Err(Error::FirstFailed { reason, logs });
	};
	for before in beside.current {
		if let Err(reason) = start_sidecar(engine, before).await {
			eprintln!("deploy: putting back {}: {reason}", before.name);
		}
	}
	engine.run(current, back.unwrap_or(shape), &volumes.data(name)).await?;
	Err(Error::Unhealthy { reason, logs })
}

/// Run `sidecar` in place of whatever ran under its name, alone rather than as part of its app's
/// deploy: the driver moving every sidecar to a new image. Its log is archived first.
pub async fn sidecar(engine: &Engine, volumes: &Volumes, sidecar: &Sidecar) -> Result<(), Error> {
	engine.archive(&sidecar.name, &volumes.logs(&sidecar.name)).await?;
	start_sidecar(engine, sidecar).await.map_err(Error::Sidecar)
}

/// Make its directories, give them to the user it runs as, start it and wait for it to answer. A
/// failure is a reason, carrying the last lines it wrote.
async fn start_sidecar(engine: &Engine, sidecar: &Sidecar) -> Result<(), String> {
	let started = async {
		let owner = match sidecar.user {
			Some(user) => Some(user),
			None => engine.user_of(&sidecar.image).await?,
		};
		for directory in std::iter::once(sidecar.source.clone())
			.chain(sidecar.directories.iter().map(|bucket| sidecar.source.join(bucket)))
		{
			make(&directory, owner).await?;
		}
		engine.run_sidecar(sidecar).await?;
		Ok::<_, Error>(())
	};
	if let Err(error) = started.await {
		return Err(format!("{}: {error}", sidecar.name));
	}
	let started = tokio::time::Instant::now();
	let mut last = String::from("no answer yet");
	while started.elapsed() < SIDECAR_DEADLINE {
		if !engine.running(&sidecar.name).await.map_err(|e| e.to_string())? {
			last = "it exited".into();
			break;
		}
		match sidecar.ask().await {
			Ok(()) => return Ok(()),
			Err(answer) => last = answer,
		}
		tokio::time::sleep(Duration::from_secs(1)).await;
	}
	let logs = engine.tail(&sidecar.name).await;
	Err(format!("{} is not healthy: {last}\n{logs}", sidecar.name))
}

/// How long a sidecar has to answer before its app is started without it, which is never.
const SIDECAR_DEADLINE: Duration = Duration::from_secs(30);

/// A directory a sidecar mounts or serves, made when missing and given -- itself, not what is in it
/// -- to `owner`, as an app's own directory is.
async fn make(directory: &Path, owner: Option<(u32, u32)>) -> Result<(), Error> {
	use std::os::unix::fs::MetadataExt;
	let failed = |source| volume::Error::Io { path: directory.to_path_buf(), source };
	tokio::fs::create_dir_all(directory).await.map_err(failed)?;
	let Some((uid, gid)) = owner else { return Ok(()) };
	let metadata = tokio::fs::metadata(directory).await.map_err(failed)?;
	if (metadata.uid(), metadata.gid()) != (uid, gid) {
		std::os::unix::fs::chown(directory, Some(uid), Some(gid)).map_err(failed)?;
	}
	Ok(())
}

/// Where a health check asks an app: on its socket, or at its port.
#[derive(Debug, Clone, PartialEq)]
pub enum Asked {
	/// `host:port`, as host or keeper dials it.
	At(String),
	/// The socket file, as the machine sees it.
	Socket(PathBuf),
}

impl Asked {
	/// The status `path` is answered with, and the body.
	pub async fn get(
		&self,
		path: &str,
		within: Duration,
	) -> Result<(u16, String), crate::http::Error> {
		match self {
			Asked::At(address) => crate::http::get_within(address, path, within).await,
			Asked::Socket(socket) => crate::http::get_unix_within(socket, path, within).await,
		}
	}
}

/// Where `manifest`'s health is asked of the container `container`: its socket in `data`, its
/// directory as the machine sees it, or its port -- on the app's own network when it has one,
/// since the checker may share several with it; see `engine::on_own_network`. None for a
/// declaration with no container.
pub fn asked_at(
	manifest: &Manifest,
	container: &str,
	networked: bool,
	data: &Path,
) -> Option<Asked> {
	let declared = manifest.container.as_ref()?;
	if let Some(socket) = &declared.socket {
		return Some(Asked::Socket(data.join(socket)));
	}
	let port = declared.port.unwrap_or_default();
	Some(Asked::At(if networked {
		format!("{container}.{}:{port}", engine::network_of(&manifest.name))
	} else {
		format!("{container}:{port}")
	}))
}

/// Poll the declared path of the app's container until it answers 2xx, the container exits, or
/// the deadline passes.
async fn healthy(
	engine: &Engine,
	version: &Version,
	shape: &Shape,
	data: &Path,
) -> Result<(), String> {
	let name = version.manifest.name.as_str();
	let Some(asked) = asked_at(&version.manifest, name, shape.networked(), data) else {
		return Err("the declaration has no container to check".into());
	};
	healthy_at(engine, version, name, &asked).await
}

/// `healthy`, of the container `container` runs `version` in, asked at `asked`: a version beside
/// its predecessor, asked at its address, since a name it alone has would not resolve through
/// musl, which takes no underscore in a host name.
pub async fn healthy_at(
	engine: &Engine,
	version: &Version,
	container: &str,
	asked: &Asked,
) -> Result<(), String> {
	let Some(declared) = &version.manifest.container else {
		return Err("the declaration has no container to check".into());
	};
	let deadline = declared.health_timeout.map_or(DEFAULT_DEADLINE, Duration::from_secs);
	let started = tokio::time::Instant::now();
	let mut last = String::from("no answer yet");
	while started.elapsed() < deadline {
		if !engine.running(container).await.map_err(|e| e.to_string())? {
			return Err("the container exited during its health check".into());
		}
		match asked.get(&declared.health, crate::http::ATTEMPT).await {
			Ok((status, _)) if (200..300).contains(&status) => return Ok(()),
			Ok((status, _)) => last = format!("{} answered {status}", declared.health),
			Err(error) => last = error.to_string(),
		}
		tokio::time::sleep(Duration::from_secs(1)).await;
	}
	Err(format!("not healthy within {} seconds: {last}", deadline.as_secs()))
}

#[cfg(test)]
mod tests {
	use super::{Asked, asked_at};
	use crate::Manifest;
	use std::path::Path;

	#[test]
	fn a_networked_app_is_asked_on_its_own_network_and_the_rest_by_name() {
		let manifest = |name: &str, answer: &str| {
			let text = format!(
				"version = 1\nname = \"{name}\"\nplacements = [\"rdu\"]\n[container]\n{answer}\n\
				 health = \"/health\"\n"
			);
			Manifest::parse(&text).unwrap()
		};
		let data = Path::new("/data/apps/x/data");
		// keeper checking host shares app-keeper and app-host with it, and host binds on app-host.
		let host = manifest("host", "port = 11011");
		assert_eq!(asked_at(&host, "host", true, data), Some(Asked::At("host.app-host:11011".into())));
		let caddy = manifest("caddy", "port = 2019");
		assert_eq!(asked_at(&caddy, "caddy", false, data), Some(Asked::At("caddy:2019".into())));
		let meter = manifest("meter", "socket = \"meter.sock\"");
		let socket = asked_at(&meter, "meter", false, data);
		assert_eq!(socket, Some(Asked::Socket(data.join("meter.sock"))));
	}
}
