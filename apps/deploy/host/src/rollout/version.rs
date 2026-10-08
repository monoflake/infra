//! Deploying and closing one version: the step every action that runs a version shares, the event
//! it is recorded in, and the collection after it. See spec/architecture/host.md.

use super::Error;
use super::Outcome;
use super::admit::{RESOLVER, admit, admitted, runnable};
use super::beside::{self, Plan};
use super::route::{attach, proxy_everywhere, proxy_into, route};
use super::shape::{bound, shape_of};
use super::tell::{tell_cron, tell_telemetry};
use crate::grants::Role;
use crate::sidecars::{self, drive};
use crate::store::{Action, Deployed, Source, Stage};
use crate::{Host, store};
use deploy::manifest::{Manifest, Rollout};
use deploy::replace::{self, Beside, replace_beside};
use deploy::sidecar::{Driver, Sidecar};
use deploy::{Shape, Version};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The one step every action that runs a version shares, as event `id`: replace what runs with
/// `next`, restoring `restore` first, and answer with the snapshot taken before `next` started. A
/// driver has no container and so no snapshot: running it is moving every sidecar of its kind onto
/// it. Nor has an app rolled out beside what runs, which keeps nothing to snapshot.
pub(super) async fn run_version(
	host: &Host,
	id: i64,
	next: &Version,
	current: Option<&Version>,
	restore: Option<&Path>,
) -> Result<Option<PathBuf>, Error> {
	// A redeploy or a rollback is admitted by nothing else, and the node may have stopped emulating.
	runnable(host, &next.manifest)?;
	if let Some(kind) = host.config.grants.driver_of(&next.manifest) {
		drive(host, kind, next, current).await?;
		return Ok(None);
	}
	// CoreDNS will not start without its file, so it is written before the container is -- into
	// the subvolume host makes for it, never a directory the write would make, which could not be
	// snapshotted.
	if next.manifest.name == RESOLVER {
		host.volumes.ensure(RESOLVER).await.map_err(replace::Error::from)?;
		crate::resolver::apply(&host.config.resolver, &[]).await?;
	}
	let members = [host.config.own_container.as_str(), host.config.caddy.container.as_str()];
	let (shape, beside_next, beside_current) = prepared(host, next, current).await?;
	// The tunnel has no network of its own for host to ask its health on; host stands on the edge.
	if matches!(shape, Shape::Tunnel { .. }) {
		host.engine.join(deploy::engine::EDGE_NETWORK, &members[..1], false).await?;
	}
	// The node's proxy is on an app's network from before the app first starts.
	if shape.networked() {
		host.engine.network(&next.manifest.name, &members).await?;
		proxy_into(host, &next.manifest.name).await;
	}
	// A rollback with data puts a directory back, which only a replacement does.
	if next.manifest.rollout == Rollout::Beside && restore.is_none() {
		let available = match current {
			Some(_) => beside::available().await,
			None => None,
		};
		match beside::plan(next, current, available) {
			Plan::Beside => {
				beside::roll(host, id, next, &shape, &members).await?;
				return Ok(None);
			}
			Plan::Replace(Some(why)) => {
				if let Err(error) = host.store.note(id, &why) {
					eprintln!("host: recording event {id}: {error}");
				}
			}
			Plan::Replace(None) => {}
		}
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

/// The shape `next` runs in, with the sidecars it and `current` run beside them. The sidecars
/// come first: making one writes the secrets its app is handed -- object storage's keys, a
/// database's URL -- into the app's `secret.env`, which the shape's environment is then read from,
/// so a first deploy starts with them.
pub(super) async fn prepared(
	host: &Host,
	next: &Version,
	current: Option<&Version>,
) -> Result<(Shape, Vec<Sidecar>, Vec<Sidecar>), Error> {
	let beside_next = sidecars::sidecars_for(host, &next.manifest).await?;
	let beside_current = match current {
		Some(current) => sidecars::sidecars_for(host, &current.manifest).await?,
		None => Vec::new(),
	};
	let mut shape = shape_of(host, &next.manifest)?;
	let driver = sidecars::driver(host, Driver::Objects)?;
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
	Ok((shape, beside_next, beside_current))
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
	// A new proxy is a new container, on none of the apps' networks yet, and is in the state only
	// now that its deploy is recorded.
	if host.config.grants.holder(Role::Proxy) == Some(name.as_str()) {
		proxy_everywhere(host).await;
	}
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
		let snapshot = run_version(host, id, &next, current.as_ref(), None).await?;
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
	scope: Option<&str>,
) -> Result<Outcome, Error> {
	let admitting = Some(Stage::Admitting);
	let running = store::Outcome::Running;
	match host.store.record(name, Action::Deploy, source, None, running, admitting) {
		Ok(id) => {
			// The scope an upload names, else the one the app has, else infra's for its own names.
			let resolved = match scope {
				Some(scope) => Ok(scope.to_owned()),
				None => uploaded_scope(host, name),
			};
			let scope = match staged(&host.store, id, Stage::Admitting, async { resolved }).await {
				Ok(scope) => scope,
				Err(error) => {
					let _ = tokio::fs::remove_file(archive).await;
					return Err(error);
				}
			};
			archived(host, id, name, manifest, archive, None, &scope).await
		}
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
	run: Option<u64>,
	scope: &str,
) -> Result<Outcome, Error> {
	let store = &host.store;
	let deployed = async {
		staged(store, id, Stage::Admitting, async { admit(host, name, &manifest, scope) }).await?;
		// One deploy at a time on a node: two would snapshot, stop and route over each other.
		let _one = host.deploying.lock().await;
		// The image it already runs, under the same declaration: nothing to load or restart.
		let path = archive.to_path_buf();
		let ids = tokio::task::spawn_blocking(move || deploy::archive::identities(&path));
		let ids = ids.await.ok().and_then(Result::ok).unwrap_or_default();
		let current = store.app(name)?;
		let running = host.engine.running(name).await.unwrap_or(false);
		if let Some(current) = current.filter(|app| unchanged(&ids, app, &manifest, running)) {
			skip(host, id, &unchanged_why(run));
			return Ok(Outcome { name: name.to_owned(), image: current.image, routed: Ok(()) });
		}
		let image = staged(store, id, Stage::Loading, async {
			let file = deploy::uncached::read(archive).await.map_err(Error::Archive)?;
			let loaded = host.engine.load(name, file).await;
			loaded.map_err(Error::Load)
		})
		.await?;
		let deployed = deploy(host, id, manifest, image).await?;
		// The first deploy of an app makes it its scope's.
		store.claim(name, scope)?;
		Ok(deployed)
	}
	.await;
	let _ = tokio::fs::remove_file(archive).await;
	deployed
}

/// The scope an upload naming none deploys `name` into: the one it is recorded as, or infra's for
/// infra's own names. A new app other than those is refused, its scope unsaid.
fn uploaded_scope(host: &Host, name: &str) -> Result<String, Error> {
	if let Some(recorded) = host.store.scope_of(name)? {
		return Ok(recorded);
	}
	if deploy::manifest::OWN.contains(&name) {
		return Ok(deploy::github::INFRA_SCOPE.to_owned());
	}
	Err(Error::NoScope(name.to_owned()))
}

/// Whether `app` runs the image an archive holds, by one of its `ids`, under `manifest` as it is:
/// then deploying it would restart it for nothing. A stopped, crashed or held app is deployed.
pub(super) fn unchanged(
	ids: &[String],
	app: &Deployed,
	manifest: &Manifest,
	running: bool,
) -> bool {
	running && !app.held && ids.contains(&app.image) && app.manifest == *manifest
}

/// Why a deploy of an unchanged image was passed over: from `run`, or from an upload.
pub(super) fn unchanged_why(run: Option<u64>) -> String {
	match run {
		Some(run) => format!("{}run {run} built the image this node already runs", store::UNCHANGED),
		None => format!("{}the upload is the image this node already runs", store::UNCHANGED),
	}
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
	use super::{runnable, staged};
	use crate::rollout::Error;
	use crate::store::{Action, Outcome, Source, Stage, Store};

	#[tokio::test]
	async fn an_app_asking_for_arm64_is_refused_where_nothing_runs_it_before_anything_loads() {
		let directory = tempfile::tempdir().unwrap();
		// The testing host is an x86 node given no EMULATE.
		let host = crate::testing(directory.path());
		let manifest = deploy::Manifest::parse(
			"version = 1\nname = \"database\"\narch = \"arm64\"\nplacements = [\"rdu\"]\n\
			 [container]\nport = 25432\nhealth = \"/health\"\n",
		)
		.unwrap();
		let archive = directory.path().join("never-read.tar");
		let refused = super::from_archive(
			&host,
			"database",
			manifest,
			&archive,
			&Source::upload(),
			Some("platform"),
		)
		.await;
		let Err(error @ Error::Unrunnable { .. }) = refused else { panic!("{refused:?}") };
		assert!(error.to_string().contains("EMULATE=arm64"), "{error}");
		let [event] = host.store.events(None, None, 50).unwrap().try_into().unwrap();
		assert_eq!((event.outcome, event.stage), (Outcome::Failed, Some(Stage::Admitting)));
		assert_eq!(event.detail, Some(error.to_string()));
		assert!(host.store.app("database").unwrap().is_none());
	}

	#[tokio::test]
	async fn an_identity_is_refused_ungranted_or_when_another_app_holds_its_mac() {
		use crate::store::Deployed;
		let qq = |mac: &str| {
			deploy::Manifest::parse(&format!(
				"version = 1\nname = \"qq\"\nplacements = [\"rdu\"]\n[container]\nport = 23000\n\
				 health = \"/health\"\nhostname = \"qq\"\nmac_address = \"{mac}\"\n"
			))
			.unwrap()
		};
		let directory = tempfile::tempdir().unwrap();
		let archive = directory.path().join("never-read.tar");
		let ungranted = crate::testing(directory.path());
		let refused = super::from_archive(
			&ungranted,
			"qq",
			qq("02:00:00:00:00:01"),
			&archive,
			&Source::upload(),
			Some("canmi"),
		)
		.await;
		assert!(matches!(refused, Err(Error::Refused(_))), "{refused:?}");

		let directory = tempfile::tempdir().unwrap();
		let host = crate::testing_with(directory.path(), |config| {
			config.grants = crate::grants::Grants::parse("qq:identity").unwrap();
		});
		let mut other = qq("02:00:00:00:00:01");
		other.name = "phone".into();
		other.container.as_mut().unwrap().port = Some(23001);
		let held = Deployed {
			manifest: other,
			image: "sha256:a".into(),
			previous: None,
			deployed_at: String::new(),
			held: false,
		};
		host.store.put_app(&held).unwrap();
		let taken = super::from_archive(
			&host,
			"qq",
			qq("02:00:00:00:00:01"),
			&archive,
			&Source::upload(),
			Some("platform"),
		)
		.await;
		let Err(error @ Error::MacTaken { .. }) = taken else { panic!("{taken:?}") };
		assert_eq!(
			error.to_string(),
			"MAC address 02:00:00:00:00:01 is already `phone`'s on this node"
		);
		let [event] = host.store.events(None, None, 50).unwrap().try_into().unwrap();
		assert_eq!((event.outcome, event.stage), (Outcome::Failed, Some(Stage::Admitting)));
	}

	#[tokio::test]
	async fn a_deploy_from_another_scope_than_the_apps_is_refused_before_anything_stops() {
		use crate::store::Deployed;
		let geo = || {
			deploy::Manifest::parse(
				"version = 1\nname = \"geo\"\nplacements = [\"rdu\"]\n[container]\nport = 23440\n\
				 health = \"/health\"\n",
			)
			.unwrap()
		};
		let directory = tempfile::tempdir().unwrap();
		let host = crate::testing(directory.path());
		let archive = directory.path().join("never-read.tar");
		let deployed = Deployed {
			manifest: geo(),
			image: "sha256:a".into(),
			previous: None,
			deployed_at: String::new(),
			held: false,
		};
		host.store.put_app(&deployed).unwrap();
		host.store.claim("geo", "platform").unwrap();
		let upload = Source::upload();
		let refused = super::from_archive(&host, "geo", geo(), &archive, &upload, Some("canmi")).await;
		let Err(error @ Error::OutOfScope { .. }) = refused else { panic!("{refused:?}") };
		assert_eq!(error.to_string(), "`geo` is `platform`'s, and `canmi` may not deploy it");
		let [event] = host.store.events(None, None, 50).unwrap().try_into().unwrap();
		assert_eq!((event.outcome, event.stage), (Outcome::Failed, Some(Stage::Admitting)));

		// Infra's own names from no other scope, even unrecorded.
		let mut caddy = geo();
		caddy.name = "caddy".into();
		let refused =
			super::from_archive(&host, "caddy", caddy, &archive, &upload, Some("platform")).await;
		assert!(
			matches!(refused, Err(Error::OutOfScope { ref holder, .. }) if holder == "infra"),
			"{refused:?}"
		);

		// An upload naming no scope of a new app is refused; of a recorded one it takes the app's.
		let mut fresh = geo();
		fresh.name = "fresh".into();
		let unsaid = super::from_archive(&host, "fresh", fresh, &archive, &upload, None).await;
		assert!(matches!(unsaid, Err(Error::NoScope(_))), "{unsaid:?}");
		assert_eq!(super::uploaded_scope(&host, "geo").unwrap(), "platform");
		assert_eq!(super::uploaded_scope(&host, "keeper").unwrap(), "infra");
	}

	#[tokio::test]
	async fn a_declaration_from_another_scope_than_the_apps_is_refused_too() {
		use crate::rollout::redeclare::redeclare;
		use crate::store::Deployed;
		let manifest = deploy::Manifest::parse(
			"version = 1\nname = \"geo\"\nplacements = [\"rdu\"]\n[container]\nport = 23440\n\
			 health = \"/health\"\n",
		)
		.unwrap();
		let directory = tempfile::tempdir().unwrap();
		let host = crate::testing(directory.path());
		let deployed = Deployed {
			manifest: manifest.clone(),
			image: "sha256:a".into(),
			previous: None,
			deployed_at: String::new(),
			held: false,
		};
		host.store.put_app(&deployed).unwrap();
		host.store.claim("geo", "platform").unwrap();
		let source = Source::run(42, None).of("canmi21/cue");
		let id = host.store.record(
			"geo",
			Action::Deploy,
			&source,
			None,
			Outcome::Running,
			Some(Stage::Admitting),
		);
		let refused = redeclare(&host, id.unwrap(), manifest, "canmi").await;
		assert!(matches!(refused, Err(Error::OutOfScope { .. })));
	}

	#[test]
	fn a_node_runs_arm64_natively_or_emulated_and_its_own_always() {
		let directory = tempfile::tempdir().unwrap();
		let host = crate::testing(directory.path());
		let pinned = |arch: Option<&str>| {
			let mut manifest = deploy::Manifest::parse(
				"version = 1\nname = \"database\"\nplacements = [\"rdu\"]\n[container]\n\
				 port = 25432\nhealth = \"/health\"\n",
			)
			.unwrap();
			manifest.arch = arch.map(str::to_owned);
			manifest
		};
		assert!(matches!(runnable(&host, &pinned(Some("arm64"))), Err(Error::Unrunnable { .. })));
		assert!(runnable(&host, &pinned(None)).is_ok());
		let mut emulating = host.config.clone();
		emulating.emulate = vec!["arm64".into()];
		assert!(emulating.runs("arm64"));
		emulating.native = Some("arm64");
		emulating.emulate = vec![];
		assert!(emulating.runs("arm64") && !emulating.runs("amd64"));
	}

	#[tokio::test]
	async fn a_first_deploy_starts_with_its_object_keys_and_its_database_url() {
		use crate::store::Deployed;
		let directory = tempfile::tempdir().unwrap();
		let host = crate::testing_with(directory.path(), |config| {
			config.grants = crate::grants::Grants::parse("objects:objects postgres:postgres").unwrap();
		});
		for fixture in [
			include_str!("../../../../../libs/deploy/fixtures/objects.toml"),
			include_str!("../../../../../libs/deploy/fixtures/postgres.toml"),
		] {
			let manifest = deploy::Manifest::parse(fixture).unwrap();
			let image = format!("sha256:{}", manifest.name);
			let deployed =
				Deployed { manifest, image, previous: None, deployed_at: String::new(), held: false };
			host.store.put_app(&deployed).unwrap();
		}
		let manifest = deploy::Manifest::parse(
			"version = 1\nname = \"store\"\nplacements = [\"rdu\"]\n[container]\nport = 23000\n\
			 health = \"/health\"\n[objects]\nbuckets = [\"photos\"]\n[postgres]\n",
		)
		.unwrap();
		// The directory a deploy's subvolume would be; nothing is in it yet.
		std::fs::create_dir_all(host.volumes.root("store")).unwrap();
		let next = deploy::Version { manifest, image: "sha256:store".into() };
		let (shape, beside, _) = super::prepared(&host, &next, None).await.unwrap();
		let deploy::Shape::Sandboxed { env } = shape else { panic!("{shape:?}") };
		let named = |name: &str| {
			env.iter().find_map(|line| line.strip_prefix(&format!("{name}="))).map(str::to_owned)
		};
		let key = named("S3_ACCESS_KEY_ID").expect("the access key");
		let secret = named("S3_SECRET_ACCESS_KEY").expect("the secret key");
		assert_eq!((key.len(), secret.len()), (20, 40));
		assert!(named("DATABASE_URL").is_some_and(|url| url.starts_with("postgresql://store:")));
		assert!(
			named("S3_ENDPOINT").is_some_and(|endpoint| endpoint.ends_with("//store-objects:17070"))
		);
		// The sidecar's root account is the app's own.
		let objects = beside.iter().find(|sidecar| sidecar.name == "store-objects").unwrap();
		assert!(objects.env.contains(&format!("ROOT_ACCESS_KEY_ID={key}")));
	}

	#[test]
	fn an_app_already_running_the_image_under_the_same_declaration_is_not_deployed_again() {
		use super::{unchanged, unchanged_why};
		use crate::store::Deployed;
		let manifest = deploy::Manifest::parse(
			"version = 1\nname = \"geo\"\nplacements = [\"rdu\"]\n[container]\nport = 23440\n\
			 health = \"/health\"\n",
		)
		.unwrap();
		let app = Deployed {
			manifest: manifest.clone(),
			image: "sha256:aa".into(),
			previous: None,
			deployed_at: String::new(),
			held: false,
		};
		// The manifest's digest and the config's, either of which Docker may take as the id.
		let ids = ["sha256:aa".to_owned(), "sha256:cc".to_owned()];
		assert!(unchanged(&ids, &app, &manifest, true));
		assert!(!unchanged(&["sha256:bb".to_owned()], &app, &manifest, true));
		assert!(!unchanged(&[], &app, &manifest, true));
		// A changed declaration over the same image is deployed, as is a stopped or held app.
		let mut declared = manifest.clone();
		declared.container.as_mut().unwrap().memory_mb = Some(128);
		assert!(!unchanged(&ids, &app, &declared, true));
		assert!(!unchanged(&ids, &app, &manifest, false));
		assert!(!unchanged(&ids, &Deployed { held: true, ..app.clone() }, &manifest, true));
		assert_eq!(unchanged_why(Some(42)), "unchanged: run 42 built the image this node already runs");
		assert_eq!(unchanged_why(None), "unchanged: the upload is the image this node already runs");
	}

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
