//! Taking a CI run: deploy what it built for this node, Caddy first. See spec/architecture/host.md,
//! "keeper has its own intake" and "Caddy is deployed like any app, and is the one door".

use super::version::{archived, skip, staged};
use crate::Host;
use crate::store::{self, Action, Source, Stage};
use deploy::manifest::{Manifest, Rollout};
use std::sync::Arc;

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
) -> bool {
	let Some(github) = host.github.as_ref() else {
		eprintln!("host: run {run}: this node has no GITHUB_ACTIONS_TOKEN");
		return false;
	};
	let (commit, artifacts) = match github.artifacts(repository, run).await {
		Ok(built) => (built.commit, built.artifacts),
		Err(error) => {
			eprintln!("host: run {run}: {error}");
			return false;
		}
	};
	let artifacts: Vec<_> = match only {
		Some(app) => artifacts.into_iter().filter(|artifact| artifact.app == app).collect(),
		None => artifacts,
	};
	if let Some(app) = only.filter(|_| artifacts.is_empty()) {
		eprintln!("host: run {run}: it built nothing for `{app}`");
		return true;
	}
	if !host_replaced && artifacts.iter().any(|artifact| artifact.app == "host") {
		eprintln!("host: run {run}: it built host, so keeper goes first and passes it back");
		// Not taken, so the notice keeper sends afterwards is.
		return false;
	}
	let mut whole = true;
	// See spec/architecture/host.md, "Caddy is deployed like any app, and is the one door".
	let artifacts = caddy_first(artifacts, |artifact| artifact.app.as_str());
	for artifact in artifacts.iter().filter(|artifact| artifact.app != "host") {
		let source = Source::run(run, commit.clone());
		let (app, running, downloading) =
			(artifact.app.as_str(), store::Outcome::Running, Some(Stage::Downloading));
		let opened = host.store.record(app, Action::Deploy, &source, None, running, downloading);
		let id = match opened {
			Ok(id) => id,
			Err(error) => {
				eprintln!("host: run {run}: {}: {error}", artifact.app);
				whole = false;
				continue;
			}
		};
		let fetching = github.fetch(artifact, &host.config.incoming);
		let fetched = match staged(&host.store, id, Stage::Downloading, fetching).await {
			Ok(fetched) => fetched,
			Err(error) => {
				eprintln!("host: run {run}: {}: {error}", artifact.app);
				whole = false;
				continue;
			}
		};
		let reading = async { Manifest::parse(&fetched.declaration) };
		let manifest = match staged(&host.store, id, Stage::Admitting, reading).await {
			Ok(manifest) => manifest,
			Err(error) => {
				eprintln!("host: run {run}: {}: {error}", artifact.app);
				let _ = tokio::fs::remove_file(&fetched.image).await;
				continue;
			}
		};
		// Built for every node; deployed only where it is placed.
		if !manifest.placements.iter().any(|placement| placement == &host.config.node) {
			skip(&host, id, &unplaced(&manifest.placements));
			let _ = tokio::fs::remove_file(&fetched.image).await;
			continue;
		}
		// Rolled out by hand: the operator's notice for it alone deploys it, a node at a time.
		if manifest.rollout == Rollout::Manual && only.is_none() {
			skip(&host, id, &by_hand(&host.config.node, repository, run, &artifact.app));
			let _ = tokio::fs::remove_file(&fetched.image).await;
			eprintln!("host: run {run}: {} is rolled out by hand, so it was not deployed", artifact.app);
			continue;
		}
		// Held stopped from the panel: the run is recorded, not started. See
		// spec/architecture/host.md, "A stop holds until a start".
		if host.store.app(&artifact.app).ok().flatten().is_some_and(|app| app.held) {
			skip(&host, id, HELD);
			let _ = tokio::fs::remove_file(&fetched.image).await;
			eprintln!("host: run {run}: {} is held stopped, so it was not deployed", artifact.app);
			continue;
		}
		match archived(&host, id, &artifact.app, manifest, &fetched.image).await {
			Ok(outcome) => eprintln!("host: run {run}: {} is {}", outcome.name, outcome.image),
			Err(error) => {
				eprintln!("host: run {run}: {}: {error}", artifact.app);
				whole = false;
			}
		}
	}
	whole
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
	use super::{by_hand, caddy_first, unplaced};

	#[test]
	fn a_deploy_passed_over_for_the_operator_says_how_they_deploy_it() {
		assert_eq!(
			by_hand("rdu", "monoflake/platform", 42, "database"),
			"rolled out by hand: mise run node deploy rdu --run 42 --repository monoflake/platform \
			 --app database"
		);
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
