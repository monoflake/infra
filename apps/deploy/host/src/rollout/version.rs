//! Deploying and closing one version: the step every action that runs a version shares, the event
//! it is recorded in, and the collection after it. See spec/architecture/host.md.

use super::Error;
use super::Outcome;
use super::admit::{RESOLVER, admit, admitted};
use super::route::{attach, route};
use super::shape::{bound, shape_of};
use super::tell::{tell_cron, tell_telemetry};
use crate::sidecars::{self, drive};
use crate::store::{Action, Deployed, Source, Stage};
use crate::{Host, store};
use deploy::manifest::Manifest;
use deploy::replace::{self, Beside, replace_beside};
use deploy::sidecar::Driver;
use deploy::{Shape, Version};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The one step every action that runs a version shares: replace what runs with `next`, restoring
/// `restore` first, and answer with the snapshot taken before `next` started. A driver has no
/// container and so no snapshot: running it is moving every sidecar of its kind onto it.
pub(super) async fn run_version(
	host: &Host,
	next: &Version,
	current: Option<&Version>,
	restore: Option<&Path>,
) -> Result<Option<PathBuf>, Error> {
	if let Some(kind) = host.config.grants.driver_of(&next.manifest) {
		drive(host, kind, next, current).await?;
		return Ok(None);
	}
	let mut shape = shape_of(host, &next.manifest)?;
	// CoreDNS will not start without its file, so it is written before the container is -- into
	// the subvolume host makes for it, never a directory the write would make, which could not be
	// snapshotted.
	if next.manifest.name == RESOLVER {
		host.volumes.ensure(RESOLVER).await.map_err(replace::Error::from)?;
		crate::resolver::apply(&host.config.resolver, &[]).await?;
	}
	let members = [host.config.own_container.as_str(), host.config.caddy.container.as_str()];
	let driver = sidecars::driver(host, Driver::Objects)?;
	let beside_next = sidecars::sidecars_for(host, &next.manifest).await?;
	let beside_current = match current {
		Some(current) => sidecars::sidecars_for(host, &current.manifest).await?,
		None => Vec::new(),
	};
	// cron and apt declare no `[objects]`, so binding always no-ops for them; only the shapes that
	// could carry a sidecar's address need the match at all.
	if let Some(driver) = &driver
		&& let Shape::Sandboxed { env }
		| Shape::Platform { env }
		| Shape::Observer { env }
		| Shape::Edge { env }
		| Shape::Tunnel { env, .. }
		| Shape::Peer { env } = &mut shape
	{
		bound(env, sidecars::binding(&next.manifest, &driver.manifest));
	}
	// The tunnel has no network of its own for host to ask its health on; host stands on the edge.
	if matches!(shape, Shape::Tunnel { .. }) {
		host.engine.join(deploy::engine::EDGE_NETWORK, &members[..1], false).await?;
	}
	let beside = Beside { next: &beside_next, current: &beside_current };
	let snapshot =
		replace_beside(&host.engine, &host.volumes, &members, &shape, next, current, restore, beside)
			.await?;
	// A peer reaches host on host's own network.
	let name = next.manifest.name.as_str();
	if admitted(&shape) {
		host
			.engine
			.join(&deploy::engine::network_of(&host.config.own_container), &[name], false)
			.await?;
	}
	// A new Caddy is a new container, on none of the apps' networks yet.
	if next.manifest.name == host.config.caddy.container
		&& let Err(error) = attach(host).await
	{
		eprintln!("host: attaching the new Caddy: {error}");
	}
	Ok(Some(snapshot))
}

/// Close event `id` with how `result` went, keeping the snapshot a success took.
pub(super) fn close<T>(host: &Host, id: i64, result: &Result<(T, Option<PathBuf>), Error>) {
	let closed = match result {
		Ok((_, snapshot)) => {
			let snapshot = snapshot.as_ref().map(|snapshot| snapshot.display().to_string());
			host.store.finish(id, store::Outcome::Succeeded, snapshot.as_deref(), None)
		}
		Err(error) => host.store.finish(id, store::Outcome::Failed, None, Some(&error.to_string())),
	};
	if let Err(error) = closed {
		eprintln!("host: recording event {id}: {error}");
	}
}

/// Run `step` as event `id`'s `stage`: moved on to it first, and closed failed at it if it fails.
/// A record that cannot be written is logged rather than failing the deploy, as `close` does.
pub(super) async fn staged<T, E: std::fmt::Display>(
	store: &store::Store,
	id: i64,
	stage: Stage,
	step: impl Future<Output = Result<T, E>>,
) -> Result<T, E> {
	if let Err(error) = store.advance(id, stage, None) {
		eprintln!("host: recording event {id}: {error}");
	}
	step.await.inspect_err(|error| {
		let detail = error.to_string();
		if let Err(error) = store.finish(id, store::Outcome::Failed, None, Some(&detail)) {
			eprintln!("host: recording event {id}: {error}");
		}
	})
}

/// Close event `id` as passed over, saying why.
pub(super) fn skip(host: &Host, id: i64, why: &str) {
	if let Err(error) = host.store.finish(id, store::Outcome::Skipped, None, Some(why)) {
		eprintln!("host: recording event {id}: {error}");
	}
}

/// Caddy follows the state, and images no version needs go.
pub(super) async fn settle(
	host: &Arc<Host>,
	name: String,
	image: String,
) -> Result<Outcome, Error> {
	let routed = route(host).await.map_err(|error| error.to_string());
	collect(host).await?;
	tell_cron(host).await;
	tell_telemetry(host).await;
	Ok(Outcome { name, image, routed })
}

/// Deploy a new version, as an upload or a CI run brings one, into event `id`, which it closes.
/// Explicit, so it ends a hold.
pub(super) async fn deploy(
	host: &Arc<Host>,
	id: i64,
	manifest: Manifest,
	image: String,
) -> Result<Outcome, Error> {
	let name = manifest.name.clone();
	if let Err(error) = host.store.advance(id, Stage::Starting, Some(&image)) {
		eprintln!("host: recording event {id}: {error}");
	}
	let result = async {
		let current = host
			.store
			.app(&name)?
			.map(|current| Version { manifest: current.manifest, image: current.image });
		let next = Version { manifest, image };
		let snapshot = run_version(host, &next, current.as_ref(), None).await?;
		host.store.put_app(&Deployed {
			manifest: next.manifest,
			image: next.image.clone(),
			previous: current,
			deployed_at: jiff::Timestamp::now().to_string(),
			held: false,
		})?;
		host.store.hold(&name, false)?;
		Ok((next.image, snapshot))
	}
	.await;
	close(host, id, &result);
	let (image, _) = result?;
	settle(host, name, image).await
}

/// Load an image archive and deploy it, as an upload brings one. The archive is gone afterwards
/// whatever happened, so a refused one does not wait on disk for the next.
pub async fn from_archive(
	host: &Arc<Host>,
	name: &str,
	manifest: Manifest,
	archive: &Path,
	source: &Source,
) -> Result<Outcome, Error> {
	let admitting = Some(Stage::Admitting);
	let running = store::Outcome::Running;
	match host.store.record(name, Action::Deploy, source, None, running, admitting) {
		Ok(id) => archived(host, id, name, manifest, archive).await,
		Err(error) => {
			let _ = tokio::fs::remove_file(archive).await;
			Err(error.into())
		}
	}
}

/// `from_archive` into event `id`, already open: moved on as the archive is admitted, loaded and
/// deployed, and closed at the stage that fails.
pub(super) async fn archived(
	host: &Arc<Host>,
	id: i64,
	name: &str,
	manifest: Manifest,
	archive: &Path,
) -> Result<Outcome, Error> {
	let store = &host.store;
	let deployed = async {
		staged(store, id, Stage::Admitting, async { admit(host, name, &manifest) }).await?;
		// One deploy at a time on a node: two would snapshot, stop and route over each other.
		let _one = host.deploying.lock().await;
		let image = staged(store, id, Stage::Loading, async {
			let file = deploy::uncached::read(archive).await.map_err(Error::Archive)?;
			let loaded = host.engine.load(name, file).await;
			loaded.map_err(Error::Load)
		})
		.await?;
		deploy(host, id, manifest, image).await
	}
	.await;
	let _ = tokio::fs::remove_file(archive).await;
	deployed
}

/// Keep what every app runs and what each would go back to; the rest of their images go. host's
/// own images are keeper's to collect, so they are not among the names asked about here.
pub(super) async fn collect(host: &Host) -> Result<(), Error> {
	let apps = host.store.apps()?;
	let names: Vec<&str> = apps.iter().map(|app| app.manifest.name.as_str()).collect();
	let mut keep = HashSet::new();
	for app in &apps {
		keep.insert(app.image.clone());
		keep.extend(app.previous.as_ref().map(|previous| previous.image.clone()));
	}
	host.engine.collect(&names, &keep).await?;
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::staged;
	use crate::rollout::Error;
	use crate::store::{Action, Outcome, Source, Stage, Store};

	#[tokio::test]
	async fn a_load_that_fails_closes_its_event_failed_at_loading() {
		let directory = tempfile::tempdir().unwrap();
		let store = Store::open(directory.path()).unwrap();
		let (running, admitting) = (Outcome::Running, Some(Stage::Admitting));
		let id = store.record("geo", Action::Deploy, &Source::upload(), None, running, admitting);
		let id = id.unwrap();
		staged(&store, id, Stage::Admitting, async { Ok::<_, Error>(()) }).await.unwrap();
		let [event] = store.events(None, None, 50).unwrap().try_into().unwrap();
		assert_eq!((event.outcome, event.stage), (Outcome::Running, admitting));

		let missing = std::io::Error::from(std::io::ErrorKind::NotFound);
		let loading = async { Err::<String, _>(Error::Archive(missing)) };
		assert!(staged(&store, id, Stage::Loading, loading).await.is_err());
		let [event] = store.events(None, None, 50).unwrap().try_into().unwrap();
		assert_eq!((event.outcome, event.stage), (Outcome::Failed, Some(Stage::Loading)));
		assert!(event.detail.is_some_and(|detail| detail.starts_with("the archive:")));
		assert!(event.finished_at.is_some());
	}
}
