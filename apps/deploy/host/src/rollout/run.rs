//! Taking a CI run: deploy what it built for this node, Caddy first. See spec/architecture/host.md,
//! "keeper has its own intake" and "Caddy is deployed like any app, and is the one door".

use super::admit::runnable;
use super::catch_up::WITHIN;
use super::redeclare::{Redeclared, planned, redeclare};
use super::version::{archived, skip, staged};
use crate::Host;
use crate::store::{self, Action, Source, Stage};
use deploy::canary::{self, Canary, DEADLINE, GATED, waits};
use deploy::manifest::{Manifest, Rollout};
use std::collections::HashMap;
use std::sync::{Arc, PoisonError};

/// Deploy what a CI run built for this node, once GitHub's record of the run says it may be; with
/// `only`, the operator's one app, rolled out by hand or not. host's own image is keeper's first,
/// and acted on here only once passed back with `host_replaced`. True when everything went, so a
/// notice that failed on the way is taken again. See spec/architecture/host.md, "keeper has its
/// own intake", and "An app chooses how it is rolled out, and keeping nothing earns a gapless one".
pub async fn from_run(
	host: Arc<Host>,
	repository: &str,
	run: u64,
	host_replaced: bool,
	only: Option<&str>,
	by_hand: bool,
) -> bool {
	let Some(github) = host.github.as_ref() else {
		eprintln!("host: run {run}: this node has no GITHUB_ACTIONS_TOKEN");
		return false;
	};
	let built = match github.artifacts(repository, run).await {
		Ok(built) => built,
		Err(error) => {
			eprintln!("host: run {run}: {error}");
			return false;
		}
	};
	let commit = built.commit.clone();
	// The scope the node says this repository's runs deploy into; never the declaration's to say.
	let Some(scope) = github.scope_of(repository).map(str::to_owned) else {
		eprintln!("host: run {run}: {repository} is no source of this node's");
		return true;
	};
	// Read apart when the run uploaded them, so nothing placed elsewhere, held or rolled out by hand
	// is downloaded at all; a run from before reads each from its image.
	let declared = match &built.declarations {
		Some(declarations) => match github.declarations(declarations, &host.config.incoming).await {
			Ok(declared) => declared,
			Err(error) => {
				eprintln!("host: run {run}: {error}");
				return false;
			}
		},
		None => HashMap::new(),
	};
	let artifacts: Vec<_> = match only {
		Some(app) => built.artifacts.iter().filter(|artifact| artifact.app == app).cloned().collect(),
		None => built.artifacts.clone(),
	};
	let with_images =
		built.artifacts.iter().chain(&built.others).map(|artifact| artifact.app.as_str());
	let redeclared: Vec<String> = redeclared(with_images, &declared)
		.into_iter()
		.filter(|app| only.is_none_or(|only| only == app))
		.collect();
	if let Some(app) = only.filter(|_| artifacts.is_empty() && redeclared.is_empty()) {
		eprintln!("host: run {run}: it built nothing for `{app}`");
		return true;
	}
	// What this node settled of the run already is not done again, unless the operator asks for it.
	let settled = host.store.settled(run).unwrap_or_default();
	let overridden = by_hand || only.is_some();
	let artifacts = unsettled(artifacts, &settled, overridden, |artifact| artifact.app.as_str());
	let redeclared = unsettled(redeclared, &settled, overridden, String::as_str);
	// host is keeper's to deploy, and only from infra's own repository: another scope's run that
	// built one is not waited on, and keeper leaves it alone too.
	let artifacts: Vec<_> = artifacts
		.into_iter()
		.filter(|artifact| artifact.app != "host" || scope == deploy::github::INFRA_SCOPE)
		.collect();
	if !host_replaced && artifacts.iter().any(|artifact| artifact.app == "host") {
		eprintln!("host: run {run}: it built host, so keeper goes first and passes it back");
		// Not taken, so the notice keeper sends afterwards is.
		return false;
	}
	// A new host, keeper or Caddy reaches the canary first, and so does a declaration of one that
	// restarts it; deployed by hand, it goes now.
	let mut waiting = HashMap::new();
	let apps: Vec<&str> = artifacts
		.iter()
		.map(|artifact| artifact.app.as_str())
		.chain(redeclared.iter().map(String::as_str))
		.filter(|app| *app != "host")
		.collect();
	let restarts = |app: &str| {
		let manifest = declared.get(app).and_then(|text| Manifest::parse(text).ok());
		manifest.is_none_or(|manifest| {
			planned(&host, &manifest).ok().flatten().is_none_or(|apply| apply.restarts())
		})
	};
	let gated: Vec<&str> = artifacts
		.iter()
		.map(|artifact| artifact.app.as_str())
		.chain(redeclared.iter().map(String::as_str).filter(|app| restarts(app)))
		.filter(|app| GATED.contains(app))
		.collect();
	if let Canary::At(canary) = host.config.canary
		&& !by_hand
		&& !gated.is_empty()
	{
		let held = Held { host: &host, repository, run, commit: commit.clone() };
		match held.wait(canary, &apps, &gated).await {
			Some(ids) => waiting = ids,
			None => return true,
		}
	}
	let mut whole = true;
	// See spec/architecture/host.md, "Caddy is deployed like any app, and is the one door".
	let artifacts = caddy_first(artifacts, |artifact| artifact.app.as_str());
	for artifact in artifacts.iter().filter(|artifact| artifact.app != "host") {
		let app = artifact.app.as_str();
		let source = Source::run(run, commit.clone()).of(repository);
		let first = if declared.contains_key(app) { Stage::Admitting } else { Stage::Downloading };
		let running = store::Outcome::Running;
		let opened = match waiting.remove(app) {
			Some(id) => host.store.advance(id, first, None).map(|()| id),
			None => host.store.record(app, Action::Deploy, &source, None, running, Some(first)),
		};
		let id = match opened {
			Ok(id) => id,
			Err(error) => {
				eprintln!("host: run {run}: {app}: {error}");
				whole = false;
				continue;
			}
		};
		// The declaration, and the image of this node's architecture when reading it meant fetching it.
		let (manifest, carried) = match declared.get(app) {
			Some(text) => {
				let reading = async { Manifest::parse(text) };
				match staged(&host.store, id, Stage::Admitting, reading).await {
					Ok(manifest) => (manifest, None),
					Err(error) => {
						eprintln!("host: run {run}: {app}: {error}");
						continue;
					}
				}
			}
			None => {
				let fetching = github.fetch(artifact, &host.config.incoming);
				let fetched = match staged(&host.store, id, Stage::Downloading, fetching).await {
					Ok(fetched) => fetched,
					Err(error) => {
						eprintln!("host: run {run}: {app}: {error}");
						whole = false;
						continue;
					}
				};
				let reading = async { Manifest::parse(&fetched.declaration) };
				match staged(&host.store, id, Stage::Admitting, reading).await {
					Ok(manifest) => (manifest, Some(fetched.image)),
					Err(error) => {
						eprintln!("host: run {run}: {app}: {error}");
						let _ = tokio::fs::remove_file(&fetched.image).await;
						continue;
					}
				}
			}
		};
		if let Some(why) = passed_over(&host, &manifest, only, repository, run) {
			skip(&host, id, &why);
			if let Some(image) = &carried {
				let _ = tokio::fs::remove_file(image).await;
			}
			eprintln!("host: run {run}: {app} was not deployed: {why}");
			continue;
		}
		// Asked for in another architecture: that image alone, where this node can run it. See
		// spec/architecture/host.md, "An app may ask for one architecture".
		let wanted = manifest.arch.as_deref().unwrap_or(artifact.arch);
		let image = match carried {
			Some(image) if wanted == artifact.arch => image,
			carried => {
				if let Some(image) = carried {
					let _ = tokio::fs::remove_file(&image).await;
				}
				let admitting = async { runnable(&host, &manifest) };
				if let Err(error) = staged(&host.store, id, Stage::Admitting, admitting).await {
					eprintln!("host: run {run}: {app}: {error}");
					continue;
				}
				let fetching = async {
					let Some(asked) = built.built_for(app, wanted) else {
						return Err(format!("the run built no {wanted} image of `{app}`"));
					};
					github.fetch(asked, &host.config.incoming).await.map_err(|error| error.to_string())
				};
				match staged(&host.store, id, Stage::Downloading, fetching).await {
					Ok(fetched) => fetched.image,
					Err(error) => {
						eprintln!("host: run {run}: {app}: {error}");
						whole = false;
						continue;
					}
				}
			}
		};
		match archived(&host, id, app, manifest, &image, Some(run), &scope).await {
			Ok(outcome) => eprintln!("host: run {run}: {} is {}", outcome.name, outcome.image),
			Err(error) => {
				eprintln!("host: run {run}: {app}: {error}");
				whole = false;
			}
		}
	}
	for app in caddy_first(redeclared, String::as_str) {
		let source = Source::run(run, commit.clone()).of(repository);
		let (running, admitting) = (store::Outcome::Running, Some(Stage::Admitting));
		let opened = match waiting.remove(&app) {
			Some(id) => host.store.advance(id, Stage::Admitting, None).map(|()| id),
			None => host.store.record(&app, Action::Deploy, &source, None, running, admitting),
		};
		let Ok(id) = opened.inspect_err(|error| eprintln!("host: run {run}: {app}: {error}")) else {
			whole = false;
			continue;
		};
		let text = declared.get(&app).cloned().unwrap_or_default();
		let reading = async { Manifest::parse(&text) };
		let Ok(manifest) = staged(&host.store, id, Stage::Admitting, reading).await else { continue };
		if let Some(why) = passed_over(&host, &manifest, only, repository, run) {
			skip(&host, id, &why);
			eprintln!("host: run {run}: {app}'s declaration was not applied: {why}");
			continue;
		}
		let applied = match redeclare(&host, id, manifest.clone(), &scope).await {
			Ok(Redeclared::Applied(outcome)) => Ok(outcome),
			Ok(Redeclared::NeedsImage) => {
				imaged(&host, github, id, manifest, repository, run, &scope).await
			}
			Err(error) => Err(error),
		};
		match applied {
			Ok(outcome) => eprintln!("host: run {run}: {}'s declaration taken", outcome.name),
			Err(error) => {
				eprintln!("host: run {run}: {app}: {error}");
				whole = false;
			}
		}
	}
	whole
}

/// The apps whose declaration `built` changed with no image of them in any architecture: what a
/// node applies over the image it runs. host's own is keeper's, and never one: host's binary
/// carries its declaration, so a change to it builds an image.
fn redeclared<'a>(
	imaged: impl Iterator<Item = &'a str>,
	declared: &HashMap<String, String>,
) -> Vec<String> {
	let imaged: std::collections::HashSet<&str> = imaged.collect();
	let mut apps: Vec<String> = declared
		.keys()
		.filter(|app| !imaged.contains(app.as_str()) && *app != "host")
		.cloned()
		.collect();
	apps.sort_unstable();
	apps
}

/// Why a declaration that needs an image here, and has none within reach, was passed over.
pub(super) const NO_IMAGE: &str =
	"declaration changed, and no image of it is within 7 days; run the build by hand";

/// A declaration of an app this node runs no image of, in the architecture it asks for: deployed
/// with the newest image of it a run of `repository` built within 7 days, into event `id`.
async fn imaged(
	host: &Arc<Host>,
	github: &deploy::github::GitHub,
	id: i64,
	manifest: Manifest,
	repository: &str,
	run: u64,
	scope: &str,
) -> Result<super::Outcome, super::Error> {
	let app = manifest.name.clone();
	staged(&host.store, id, Stage::Admitting, async { runnable(host, &manifest) }).await?;
	let wanted = manifest.arch.as_deref().or(host.config.native).unwrap_or_default().to_owned();
	let finding = async {
		let recent = github.recent(repository, WITHIN).await.map_err(|error| error.to_string())?;
		for newer in recent.iter().rev() {
			let built = github.artifacts(repository, *newer).await.map_err(|error| error.to_string())?;
			if let Some(artifact) = built.built_for(&app, &wanted) {
				let fetched = github.fetch(artifact, &host.config.incoming).await;
				return fetched.map(|fetched| Some((*newer, fetched.image))).map_err(|e| e.to_string());
			}
		}
		Ok(None)
	};
	let found = staged(&host.store, id, Stage::Downloading, finding).await;
	let Some((newer, image)) = found.map_err(super::Error::Finding)? else {
		skip(host, id, NO_IMAGE);
		return Ok(super::Outcome { name: app, image: String::new(), routed: Ok(()) });
	};
	let why = format!("declaration applied with run {newer}'s image, the newest within 7 days");
	if let Err(error) = host.store.note(id, &why) {
		eprintln!("host: recording event {id}: {error}");
	}
	archived(host, id, &app, manifest, &image, Some(run), scope).await
}

/// `artifacts` but the apps `settled` of the run already, every one when the operator `overridden`
/// the record by deploying by hand. See catch_up.rs.
fn unsettled<T>(
	artifacts: Vec<T>,
	settled: &std::collections::HashSet<String>,
	overridden: bool,
	app: impl Fn(&T) -> &str,
) -> Vec<T> {
	if overridden {
		return artifacts;
	}
	artifacts.into_iter().filter(|artifact| !settled.contains(app(artifact))).collect()
}

/// A run held for the canary on this node.
struct Held<'a> {
	host: &'a Arc<Host>,
	repository: &'a str,
	run: u64,
	commit: Option<String>,
}

impl Held<'_> {
	/// Open a waiting event for each app `artifacts` brings but host, ask the canary at `canary`
	/// about the `gated` apps until it answers, and give back the events to deploy into once it
	/// passes. Otherwise each is closed as passed over, saying why, and there is nothing to deploy.
	async fn wait(
		&self,
		canary: std::net::Ipv4Addr,
		apps: &[&str],
		gated: &[&str],
	) -> Option<HashMap<String, i64>> {
		let (host, repository, run) = (self.host, self.repository, self.run);
		let source = Source::run(run, self.commit.clone()).of(repository);
		let (running, waiting) = (store::Outcome::Running, Some(Stage::Waiting));
		let mut ids = HashMap::new();
		for app in apps.iter().copied().filter(|app| *app != "host") {
			match host.store.record(app, Action::Deploy, &source, None, running, waiting) {
				Ok(id) => {
					ids.insert(app.to_owned(), id);
				}
				Err(error) => eprintln!("host: run {run}: {app}: {error}"),
			}
		}
		eprintln!("host: run {run}: it built {}, so it waits for the canary", gated.join(", "));
		let config = &host.config;
		let token = config.read_token.as_deref().unwrap_or_default();
		let suffix = &config.caddy.private_suffix;
		let ask = || deploy::canary::ask(canary, suffix, token, repository, run, gated);
		let key = (repository.to_owned(), run);
		let released = || host.released.lock().unwrap_or_else(PoisonError::into_inner).contains(&key);
		let held = deploy::canary::hold(ask, released, DEADLINE, waits(), tokio::time::sleep).await;
		let why = match held {
			canary::Held::Passed => {
				eprintln!("host: run {run}: the canary passed it");
				return Some(ids);
			}
			canary::Held::Failed(why) => format!("the canary failed the run: {why}"),
			canary::Held::Expired(last) => format!(
				"the canary gave no verdict within {} hours; the last answer: {last}",
				DEADLINE.as_secs() / 3600
			),
			canary::Held::Released => {
				host.released.lock().unwrap_or_else(PoisonError::into_inner).remove(&key);
				"deployed by hand instead".to_owned()
			}
		};
		eprintln!("host: run {run}: not deployed: {why}");
		for id in ids.into_values() {
			skip(host, id, &why);
		}
		None
	}
}

/// Why a run's deploy of `manifest`'s app is passed over on this node, before its image is
/// downloaded: placed elsewhere, rolled out by hand and not named by the operator, or held stopped
/// from the panel -- spec/architecture/host.md, "A stop holds until a start".
fn passed_over(
	host: &Host,
	manifest: &Manifest,
	only: Option<&str>,
	repository: &str,
	run: u64,
) -> Option<String> {
	let app = manifest.name.as_str();
	if !manifest.placements.iter().any(|placement| placement == &host.config.node) {
		return Some(unplaced(&manifest.placements));
	}
	if manifest.rollout == Rollout::Manual && only.is_none() {
		return Some(by_hand(&host.config.node, repository, run, app));
	}
	if host.store.app(app).ok().flatten().is_some_and(|app| app.held) {
		return Some(HELD.to_owned());
	}
	None
}

/// Why a run's deploy was passed over for an app held stopped.
const HELD: &str = "held stopped from the panel";

/// Why a run's deploy was passed over for an app rolled out by hand, and how to deploy it.
fn by_hand(node: &str, repository: &str, run: u64, app: &str) -> String {
	format!(
		"rolled out by hand: mise run node deploy {node} --run {run} --repository {repository} --app {app}"
	)
}

/// Why a run's deploy was passed over for an app not placed on this node.
fn unplaced(placements: &[String]) -> String {
	match placements {
		[] => "placed on no node".to_owned(),
		placements => format!("placed on {}, not on this node", placements.join(", ")),
	}
}

/// The run's artifacts with caddy's first and the rest in the order they came. Deploying an app
/// attaches caddy to its network, so caddy must already exist.
fn caddy_first<T>(mut artifacts: Vec<T>, app: impl Fn(&T) -> &str) -> Vec<T> {
	artifacts.sort_by_key(|artifact| app(artifact) != "caddy");
	artifacts
}

#[cfg(test)]
mod tests {
	use super::{by_hand, caddy_first, unplaced, unsettled};
	use crate::store::{Action, Outcome, Source, Stage, Store};

	/// A store holding run 42's rows for `rows`, each an app, how it ended and why.
	fn recorded(rows: &[(&str, Outcome, Option<&str>)]) -> (tempfile::TempDir, Store) {
		let directory = tempfile::tempdir().unwrap();
		let store = Store::open(directory.path()).unwrap();
		let source = Source::run(42, None).of("monoflake/platform");
		for (app, outcome, detail) in rows {
			let running = Outcome::Running;
			let id = store.record(app, Action::Deploy, &source, None, running, Some(Stage::Admitting));
			if *outcome != Outcome::Running {
				store.finish(id.unwrap(), *outcome, None, *detail).unwrap();
			}
		}
		(directory, store)
	}

	#[test]
	fn a_declaration_with_no_image_of_it_in_any_architecture_is_applied_alone() {
		let declared: std::collections::HashMap<String, String> =
			["geo", "caddy", "relay", "host"].map(|app| (app.to_owned(), String::new())).into();
		// relay was built for arm64 alone: an image, not a re-declaration, wherever it runs.
		let imaged = ["geo", "relay"].into_iter();
		assert_eq!(super::redeclared(imaged, &declared), ["caddy"]);
	}

	#[tokio::test]
	async fn a_declaration_of_an_app_this_node_runs_no_image_of_asks_for_one() {
		use crate::rollout::redeclare::{Redeclared, redeclare};
		let directory = tempfile::tempdir().unwrap();
		let host = crate::testing(directory.path());
		let manifest = deploy::Manifest::parse(
			"version = 1\nname = \"geo\"\nplacements = [\"rdu\"]\n[container]\nport = 23440\n\
			 health = \"/health\"\n",
		)
		.unwrap();
		let source = Source::run(42, None).of("monoflake/platform");
		let (running, admitting) = (Outcome::Running, Some(Stage::Admitting));
		let id = host.store.record("geo", Action::Deploy, &source, None, running, admitting).unwrap();
		let asked = redeclare(&host, id, manifest.clone(), "platform").await.unwrap();
		assert!(matches!(asked, Redeclared::NeedsImage));
		// Running another architecture is no image of the one it now asks for.
		let deployed = crate::store::Deployed {
			manifest: manifest.clone(),
			image: "sha256:a".into(),
			previous: None,
			deployed_at: String::new(),
			held: false,
		};
		host.store.put_app(&deployed).unwrap();
		let pinned = deploy::Manifest { arch: Some("arm64".into()), ..manifest };
		let host = crate::testing_with(directory.path(), |config| {
			config.emulate = vec!["arm64".into()];
		});
		assert!(matches!(
			redeclare(&host, id, pinned, "platform").await.unwrap(),
			Redeclared::NeedsImage
		));
	}

	#[test]
	fn a_manual_app_passed_over_by_the_hook_is_then_deployed_by_hand() {
		let why = by_hand("rdu", "monoflake/platform", 42, "ledger");
		let (_directory, store) = recorded(&[("ledger", Outcome::Skipped, Some(&why))]);
		let settled = store.settled(42).unwrap();
		assert!(settled.is_empty());
		assert_eq!(unsettled(vec!["ledger"], &settled, true, |app| app), ["ledger"]);
		assert_eq!(unsettled(vec!["ledger"], &settled, false, |app| app), ["ledger"]);
	}

	#[test]
	fn held_waiting_placed_elsewhere_or_failed_settles_nothing() {
		let (_directory, store) = recorded(&[
			("geo", Outcome::Skipped, Some(super::HELD)),
			("relay", Outcome::Skipped, Some("placed on tyo, not on this node")),
			("cron", Outcome::Failed, Some("not healthy within 60 seconds")),
			("apt", Outcome::Running, None),
		]);
		assert!(store.settled(42).unwrap().is_empty());
	}

	#[test]
	fn a_notice_taken_twice_does_not_deploy_again_what_it_deployed() {
		let again = format!("run 50 {}", crate::store::BUILT_AGAIN);
		let same = super::super::version::unchanged_why(Some(42));
		let (_directory, store) = recorded(&[
			("geo", Outcome::Succeeded, None),
			("cron", Outcome::Skipped, Some(&again)),
			("relay", Outcome::Failed, Some("not healthy")),
			("apt", Outcome::Skipped, Some(&same)),
		]);
		let settled = store.settled(42).unwrap();
		assert_eq!(settled, ["geo".to_owned(), "cron".to_owned(), "apt".to_owned()].into());
		let taken = unsettled(vec!["geo", "cron", "relay"], &settled, false, |app| app);
		assert_eq!(taken, ["relay"]);
		// By hand, the operator deploys what they name whatever the record says.
		assert_eq!(unsettled(vec!["geo"], &settled, true, |app| app), ["geo"]);
	}

	#[test]
	fn a_deploy_passed_over_for_the_operator_says_how_they_deploy_it() {
		assert_eq!(
			by_hand("rdu", "monoflake/platform", 42, "database"),
			"rolled out by hand: mise run node deploy rdu --run 42 --repository monoflake/platform \
			 --app database"
		);
	}

	#[test]
	fn an_app_is_passed_over_before_its_image_is_downloaded_where_it_is_not_deployed() {
		use super::passed_over;
		use crate::store::Deployed;
		let directory = tempfile::tempdir().unwrap();
		// The testing host is the node `rdu`.
		let host = crate::testing(directory.path());
		let declared = |placements: &str, rollout: &str| {
			deploy::Manifest::parse(&format!(
				"version = 1\nname = \"geo\"\nplacements = [{placements}]\nrollout = \"{rollout}\"\n\
				 [container]\nport = 23440\nhealth = \"/health\"\n"
			))
			.unwrap()
		};
		let passed = |manifest, only| passed_over(&host, manifest, only, "monoflake/platform", 42);
		let here = declared("\"rdu\"", "replace");
		assert_eq!(passed(&here, None), None);
		let elsewhere = declared("\"tyo\", \"buf\"", "replace");
		assert_eq!(passed(&elsewhere, None).as_deref(), Some("placed on tyo, buf, not on this node"));
		let by_hand = declared("\"rdu\"", "manual");
		assert!(passed(&by_hand, None).is_some_and(|why| why.starts_with("rolled out by hand")));
		assert_eq!(passed(&by_hand, Some("geo")), None);
		host
			.store
			.put_app(&Deployed {
				manifest: here.clone(),
				image: "sha256:a".into(),
				previous: None,
				deployed_at: String::new(),
				held: false,
			})
			.unwrap();
		host.store.hold("geo", true).unwrap();
		assert_eq!(passed(&here, None).as_deref(), Some(super::HELD));
	}

	#[test]
	fn caddy_is_deployed_first_and_the_rest_keep_their_order() {
		let run = vec!["keeper", "meter", "tunnel", "caddy"];
		assert_eq!(caddy_first(run, |app| app), ["caddy", "keeper", "meter", "tunnel"]);
		let without = vec!["meter", "keeper"];
		assert_eq!(caddy_first(without, |app| app), ["meter", "keeper"]);
	}

	#[test]
	fn a_deploy_passed_over_says_where_it_is_placed() {
		assert_eq!(unplaced(&[]), "placed on no node");
		let placements = ["nrt".to_owned(), "hnd".to_owned()];
		assert_eq!(unplaced(&placements), "placed on nrt, hnd, not on this node");
	}
}
