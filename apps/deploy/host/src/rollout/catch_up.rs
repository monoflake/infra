//! Runs a notice never brought: at start and every 15 minutes, host lists each source's runs to
//! deploy from the last 7 days and takes the ones it has no record of, newer than the oldest it
//! holds, each deploying only the apps no newer run built. A missed run that built host goes to
//! keeper. See spec/architecture/host.md, "The machine pulls; nothing pushes into it".

use super::run::from_run;
use crate::Host;
use crate::store::{self, Action, Outcome, Source, Stage, Taken};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, PoisonError};
use std::time::Duration;

/// How often host looks for runs it missed, beyond once at start.
const EVERY: Duration = Duration::from_secs(15 * 60);

/// How far back: as long as CI keeps a run's artifacts.
pub(super) const WITHIN: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// keeper's port, on host's own network, where a missed run that built host is handed to it.
const KEEPER_PORT: u16 = 11010;

/// What to do with one missed run.
#[derive(Debug, PartialEq)]
pub(super) struct Missed {
	pub(super) run: u64,
	/// Apps it built that a newer run built too, each with that run: passed over, never deployed.
	pub(super) masked: Vec<(String, u64)>,
	/// It built host, and no newer run did: keeper's to take first.
	pub(super) to_keeper: bool,
}

/// The runs of `repository` among `recent` that this node missed: none it has a row of, and newer
/// than the oldest it holds of that repository. A node holding none of its runs misses none, so a
/// host just set up does not take the past week.
pub(super) fn missed(recent: &[u64], taken: &[Taken], repository: &str) -> Vec<u64> {
	let held: HashSet<u64> = taken.iter().map(|taken| taken.run).collect();
	let ours = taken.iter().filter(|taken| taken.repository.as_deref() == Some(repository));
	let Some(oldest) = ours.map(|taken| taken.run).min() else { return Vec::new() };
	let mut missed: Vec<u64> =
		recent.iter().copied().filter(|run| *run > oldest && !held.contains(run)).collect();
	missed.sort_unstable();
	missed
}

/// What each missed run deploys, given the apps each `built`, the oldest first: an app goes from
/// the newest run that built it, taken or missed, and from no other.
pub(super) fn plan(taken: &[Taken], built: &[(u64, Vec<String>)]) -> Vec<Missed> {
	let mut newest: HashMap<&str, u64> = HashMap::new();
	let runs = taken.iter().map(|taken| (taken.app.as_str(), taken.run));
	let missed =
		built.iter().flat_map(|(run, apps)| apps.iter().map(move |app| (app.as_str(), *run)));
	for (app, run) in runs.chain(missed) {
		let entry = newest.entry(app).or_insert(run);
		*entry = (*entry).max(run);
	}
	let mut planned: Vec<Missed> = built
		.iter()
		.map(|(run, apps)| {
			let masked: Vec<(String, u64)> = apps
				.iter()
				.filter_map(|app| {
					let newer = newest[app.as_str()];
					(newer > *run).then(|| (app.clone(), newer))
				})
				.collect();
			let to_keeper =
				apps.iter().any(|app| app == "host") && !masked.iter().any(|(app, _)| app == "host");
			Missed { run: *run, masked, to_keeper }
		})
		.collect();
	planned.sort_by_key(|missed| missed.run);
	planned
}

/// The apps a run touched: those it built an image of, and those whose declaration it carries.
pub(super) fn touched(imaged: impl Iterator<Item = String>, declared: Vec<String>) -> Vec<String> {
	let mut apps: Vec<String> = imaged.chain(declared).collect();
	apps.sort_unstable();
	apps.dedup();
	apps
}

/// Look for missed runs at start and every 15 minutes after, one pass at a time.
pub async fn every(host: Arc<Host>) {
	let mut empty = HashSet::new();
	loop {
		pass(&host, &mut empty).await;
		tokio::time::sleep(EVERY).await;
	}
}

/// One look at every source. `empty` remembers the runs that built nothing for this node, which
/// leave no row to say they were looked at.
async fn pass(host: &Arc<Host>, empty: &mut HashSet<u64>) {
	let Some(github) = host.github.as_ref() else { return };
	for repository in github.sources().iter().map(|source| source.repository.as_str()) {
		let recent = match github.recent(repository, WITHIN).await {
			Ok(recent) => recent,
			Err(error) => {
				eprintln!("host: catching up on {repository}: {error}");
				continue;
			}
		};
		let taken = match host.store.taken() {
			Ok(taken) => taken,
			Err(error) => return eprintln!("host: catching up: {error}"),
		};
		let mut built = Vec::new();
		for run in missed(&recent, &taken, repository) {
			if empty.contains(&run) || in_flight(host, repository, run) {
				continue;
			}
			let listed = match github.artifacts(repository, run).await {
				Ok(listed) => listed,
				Err(error) => {
					eprintln!("host: catching up on {repository} run {run}: {error}");
					continue;
				}
			};
			// A run that changed declarations alone carries them and no image: it touched them.
			let declared = match &listed.declarations {
				Some(declarations) => {
					match github.declarations(declarations, &host.config.incoming).await {
						Ok(declared) => declared.into_keys().collect(),
						Err(error) => {
							eprintln!("host: catching up on {repository} run {run}: {error}");
							continue;
						}
					}
				}
				None => Vec::new(),
			};
			let apps = touched(listed.artifacts.into_iter().map(|artifact| artifact.app), declared);
			if apps.is_empty() {
				empty.insert(run);
			} else {
				built.push((run, apps));
			}
		}
		for missed in plan(&taken, &built) {
			take(host, repository, &missed).await;
		}
	}
}

/// Whether a notice of `repository`'s `run` is being taken now.
fn in_flight(host: &Host, repository: &str, run: u64) -> bool {
	let notices = host.notices.lock().unwrap_or_else(PoisonError::into_inner);
	notices.iter().any(|(named, taken, _, _)| named == repository && *taken == run)
}

/// Take one missed run: its masked apps passed over and recorded, then the rest as a notice would
/// take them, or handed to keeper when it built host.
async fn take(host: &Arc<Host>, repository: &str, missed: &Missed) {
	let run = missed.run;
	eprintln!("host: run {run} of {repository} never reached this node; taking it now");
	for (app, newer) in &missed.masked {
		pass_over(&host.store, repository, run, app, *newer);
	}
	if missed.to_keeper {
		hand_to_keeper(host, repository, run).await;
		return;
	}
	let key = (repository.to_owned(), run, None, false);
	if !host.notices.lock().unwrap_or_else(PoisonError::into_inner).insert(key.clone()) {
		return;
	}
	if !from_run(host.clone(), repository, run, false, None, false).await {
		host.notices.lock().unwrap_or_else(PoisonError::into_inner).remove(&key);
	}
}

/// Record `app` of `run` as passed over for `newer`, so the run is settled for it.
pub(super) fn pass_over(store: &store::Store, repository: &str, run: u64, app: &str, newer: u64) {
	let source = Source::run(run, None).of(repository);
	let why = format!("run {newer} {}", store::BUILT_AGAIN);
	let recorded = store
		.record(app, Action::Deploy, &source, None, Outcome::Skipped, Some(Stage::Admitting))
		.and_then(|id| store.finish(id, Outcome::Skipped, None, Some(&why)));
	if let Err(error) = recorded {
		eprintln!("host: recording run {run}'s {app}: {error}");
	}
}

/// Hand a missed run that built host to keeper, as the hook would have, on host's own network;
/// keeper replaces host and passes the run back, the rest of it then taken as any notice is.
async fn hand_to_keeper(host: &Host, repository: &str, run: u64) {
	let network = deploy::engine::network_of(&host.config.own_container);
	let keeper = format!("keeper.{network}:{KEEPER_PORT}");
	let notice = serde_json::json!({ "run": run, "repository": repository });
	match deploy::http::post(&keeper, "/notice", notice.to_string().into_bytes()).await {
		Ok(status) if (200..300).contains(&status) => {}
		Ok(status) => eprintln!("host: handing run {run} to keeper: it answered {status}"),
		Err(error) => eprintln!("host: handing run {run} to keeper: {error}"),
	}
}

#[cfg(test)]
mod tests {
	use super::{Missed, missed, pass_over, plan};
	use crate::store::{Outcome, Store, Taken};

	const PLATFORM: &str = "monoflake/platform";

	fn taken(run: u64, repository: Option<&str>, app: &str) -> Taken {
		let repository = repository.map(str::to_owned);
		Taken { run, repository, app: app.into(), outcome: Outcome::Succeeded }
	}

	fn apps(names: &[&str]) -> Vec<String> {
		names.iter().map(|name| (*name).to_owned()).collect()
	}

	#[test]
	fn a_run_is_missed_when_it_has_no_row_and_is_newer_than_the_oldest_held() {
		let held = [taken(100, Some(PLATFORM), "geo"), taken(300, Some(PLATFORM), "relay")];
		assert_eq!(missed(&[50, 100, 200, 300, 400], &held, PLATFORM), [200, 400]);
		// Another repository's runs are not this one's oldest, but a run id is no other's.
		let infra = [taken(250, Some("monoflake/infra"), "keeper")];
		assert_eq!(missed(&[200, 250, 300], &[&held[..], &infra[..]].concat(), PLATFORM), [200]);
	}

	#[test]
	fn a_host_that_holds_nothing_of_a_repository_misses_none_of_it() {
		assert!(missed(&[100, 200], &[], PLATFORM).is_empty());
		// A row from before rows named their repository is no repository's oldest.
		assert!(missed(&[100, 200], &[taken(50, None, "geo")], PLATFORM).is_empty());
	}

	#[test]
	fn an_app_goes_from_the_newest_run_that_built_it_and_once() {
		// Two missed runs: the older built relay and geo, the newer geo alone.
		let built = [(200, apps(&["relay", "geo"])), (300, apps(&["geo"]))];
		let planned = plan(&[], &built);
		assert_eq!(
			planned,
			[
				Missed { run: 200, masked: vec![("geo".into(), 300)], to_keeper: false },
				Missed { run: 300, masked: vec![], to_keeper: false },
			]
		);
		// So relay is deployed from 200 and geo from 300, each once.
		let deployed = |app: &str| {
			let runs = built.iter().filter(|(run, apps)| {
				apps.iter().any(|built| built == app)
					&& !planned.iter().any(|missed| {
						missed.run == *run && missed.masked.iter().any(|(masked, _)| masked == app)
					})
			});
			runs.map(|(run, _)| *run).collect::<Vec<_>>()
		};
		assert_eq!((deployed("relay"), deployed("geo")), (vec![200], vec![300]));
	}

	#[test]
	fn a_run_that_changed_a_declaration_alone_is_not_an_empty_one() {
		use super::touched;
		assert_eq!(touched(std::iter::empty(), apps(&["geo"])), ["geo"]);
		assert_eq!(touched(apps(&["relay", "geo"]).into_iter(), apps(&["geo"])), ["geo", "relay"]);
		assert!(touched(std::iter::empty(), vec![]).is_empty());
		// A missed re-declaration of geo, older than a run that built geo, is masked like an image.
		let planned = plan(&[], &[(200, apps(&["geo"])), (300, apps(&["geo"]))]);
		assert_eq!(planned[0].masked, [("geo".to_owned(), 300)]);
	}

	#[test]
	fn a_newer_run_already_taken_masks_the_app_it_built() {
		let held = [taken(400, Some(PLATFORM), "geo")];
		let planned = plan(&held, &[(300, apps(&["geo", "cron"]))]);
		assert_eq!(planned[0].masked, [("geo".to_owned(), 400)]);
	}

	#[test]
	fn a_missed_run_that_built_host_goes_to_keeper_unless_a_newer_one_did() {
		let built = [(200, apps(&["host", "caddy"]))];
		assert!(plan(&[], &built)[0].to_keeper);
		let held = [taken(300, Some("monoflake/infra"), "host")];
		let planned = plan(&held, &built);
		assert!(!planned[0].to_keeper);
		assert_eq!(planned[0].masked, [("host".to_owned(), 300)]);
	}

	#[test]
	fn a_masked_app_is_settled_for_its_run_so_no_notice_deploys_it_again() {
		let directory = tempfile::tempdir().unwrap();
		let store = Store::open(directory.path()).unwrap();
		pass_over(&store, PLATFORM, 200, "geo", 300);
		assert_eq!(store.settled(200).unwrap(), ["geo".to_owned()].into());
		assert!(store.settled(300).unwrap().is_empty());
		let [event] = store.events(Some("geo"), None, 10).unwrap().try_into().unwrap();
		assert_eq!(event.outcome, Outcome::Skipped);
		assert_eq!(
			event.detail.as_deref(),
			Some("run 300 built it again, and is the one this node runs")
		);
		// The row counts as taking the run, so it is missed no more.
		let taken = store.taken().unwrap();
		assert_eq!(
			taken,
			[Taken {
				run: 200,
				repository: Some(PLATFORM.into()),
				app: "geo".into(),
				outcome: Outcome::Skipped
			}]
		);
	}
}
