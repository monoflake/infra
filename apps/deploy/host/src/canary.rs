//! The canary's side of a new host, keeper, Caddy or relay reaching one node first: its verdict on
//! a run, read from its own history and its apps' health now. The asking side is deploy::canary.

use crate::Host;
use crate::store::{self, Action, Event, Outcome, Store, UNCHANGED};
use deploy::canary::{GATED, State, Verdict};

/// How long each app has to answer its health while a verdict is given.
const HEALTH_WITHIN: std::time::Duration = std::time::Duration::from_secs(3);

/// How many of an app's newest events are searched for a run's deploy.
const SEARCHED: u32 = 100;

/// The verdict on `repository`'s `run` for the gated apps it `built`: failed when any of them
/// failed or was passed over here, pending while any is not yet deployed here or does not answer
/// its health, and passed once all are deployed and answer. host answering is its own health.
pub async fn verdict(
	host: &Host,
	repository: &str,
	run: u64,
	built: &[&str],
) -> Result<Verdict, store::Error> {
	if let Some(judged) = judged(&host.store, repository, run, built)? {
		return Ok(judged);
	}
	for app in built.iter().filter(|app| **app != "host") {
		let Some(deployed) = host.store.app(app)? else {
			return Ok(pending(format!("`{app}` is not deployed on the canary")));
		};
		let Some(container) = deployed.manifest.container.as_ref() else { continue };
		let Some(asked) = crate::rollout::asked(host, &deployed.manifest) else { continue };
		match asked.get(&container.health, HEALTH_WITHIN).await {
			Ok((status, _)) if (200..300).contains(&status) => {}
			Ok((status, _)) => return Ok(pending(format!("`{app}` answers its health {status}"))),
			Err(error) => return Ok(pending(format!("`{app}` does not answer its health: {error}"))),
		}
	}
	Ok(Verdict { state: State::Passed, why: None })
}

fn pending(why: String) -> Verdict {
	Verdict { state: State::Pending, why: Some(why) }
}

/// What the history says of each app `built` by the run: a failure or a pass-over first, then
/// anything not finished; none when every one of them was deployed.
fn judged(
	store: &Store,
	repository: &str,
	run: u64,
	built: &[&str],
) -> Result<Option<Verdict>, store::Error> {
	let mut waiting = None;
	for app in built.iter().filter(|app| GATED.contains(app)) {
		let events = store.events(Some(app), None, SEARCHED)?;
		let Some(deploy) = events.iter().find(|event| of_run(event, repository, run)) else {
			waiting.get_or_insert_with(|| format!("the canary has not taken `{app}` of run {run} yet"));
			continue;
		};
		let detail = deploy.detail.as_deref().and_then(|detail| detail.lines().next()).unwrap_or("");
		let unchanged = deploy.detail.as_deref().is_some_and(|detail| detail.starts_with(UNCHANGED));
		let why = match deploy.outcome {
			Outcome::Succeeded => continue,
			// The image it already ran, as good as deployed.
			Outcome::Skipped if unchanged => continue,
			Outcome::Failed => format!("`{app}` failed on the canary: {detail}"),
			Outcome::Skipped => format!("`{app}` was passed over on the canary: {detail}"),
			Outcome::Running => {
				waiting.get_or_insert_with(|| format!("`{app}` is still being deployed on the canary"));
				continue;
			}
		};
		return Ok(Some(Verdict { state: State::Failed, why: Some(why) }));
	}
	Ok(waiting.map(pending))
}

/// Whether `event` is the deploy of `repository`'s `run`. A row from before rows named their
/// repository is taken by its run alone.
fn of_run(event: &Event, repository: &str, run: u64) -> bool {
	event.action == Action::Deploy
		&& event.source.run == Some(run)
		&& event.source.repository.as_deref().is_none_or(|named| named == repository)
}

#[cfg(test)]
mod tests {
	use super::judged;
	use crate::store::{Action, Outcome, Source, Stage, Store};
	use deploy::canary::State;

	#[test]
	fn a_run_passes_once_each_app_it_built_was_deployed_and_fails_on_the_first_that_was_not() {
		let directory = tempfile::tempdir().unwrap();
		let store = Store::open(directory.path()).unwrap();
		let infra = |run| Source::run(run, None).of("monoflake/infra");
		let starting = Some(Stage::Starting);
		let open = |app, source: &Source| {
			store.record(app, Action::Deploy, source, None, Outcome::Running, starting).unwrap()
		};
		let judge = |built: &[&str]| judged(&store, "monoflake/infra", 42, built).unwrap();
		let built = ["caddy", "keeper"];

		let pending = judge(&built).unwrap();
		assert_eq!(pending.state, State::Pending);
		assert!(pending.why.unwrap().contains("has not taken `caddy`"));

		let caddy = open("caddy", &infra(42));
		let keeper = open("keeper", &infra(42));
		// Another repository's run 42, and this one's run 41, say nothing of it.
		let other = open("keeper", &Source::run(42, None).of("monoflake/platform"));
		store.finish(other, Outcome::Failed, None, Some("not this run")).unwrap();
		assert!(judge(&built).unwrap().why.unwrap().contains("still being deployed"));

		store.finish(caddy, Outcome::Succeeded, None, None).unwrap();
		store.finish(keeper, Outcome::Succeeded, None, None).unwrap();
		assert_eq!(judge(&built), None);
		// An app outside the three is never asked about.
		assert_eq!(judge(&["caddy", "geo"]), None);

		let host = open("host", &infra(42));
		store.finish(host, Outcome::Failed, None, Some("not healthy within 60 seconds\nlogs")).unwrap();
		// keeper rebuilt by a later run unchanged is as good as deployed.
		let again =
			store.record("keeper", Action::Deploy, &infra(43), None, Outcome::Running, starting);
		let why = "unchanged: run 43 built the image this node already runs";
		store.finish(again.unwrap(), Outcome::Skipped, None, Some(why)).unwrap();
		assert_eq!(judged(&store, "monoflake/infra", 43, &["keeper"]).unwrap(), None);
		let failed = judge(&["host", "caddy", "keeper"]).unwrap();
		assert_eq!(failed.state, State::Failed);
		assert_eq!(
			failed.why.as_deref(),
			Some("`host` failed on the canary: not healthy within 60 seconds")
		);
	}

	#[test]
	fn a_platform_run_that_built_the_relay_is_judged_by_the_relay_of_that_run() {
		let directory = tempfile::tempdir().unwrap();
		let store = Store::open(directory.path()).unwrap();
		let platform = Source::run(7, None).of("monoflake/platform");
		let starting = Some(Stage::Starting);
		let judge = || judged(&store, "monoflake/platform", 7, &["relay"]).unwrap();

		// infra's run 7 is not platform's.
		let infra = Source::run(7, None).of("monoflake/infra");
		let other = store.record("relay", Action::Deploy, &infra, None, Outcome::Running, starting);
		store.finish(other.unwrap(), Outcome::Succeeded, None, None).unwrap();
		assert!(judge().unwrap().why.unwrap().contains("has not taken `relay` of run 7"));

		let relay = store.record("relay", Action::Deploy, &platform, None, Outcome::Running, starting);
		let relay = relay.unwrap();
		assert!(judge().unwrap().why.unwrap().contains("`relay` is still being deployed"));
		store.finish(relay, Outcome::Failed, None, Some("not healthy within 60 seconds")).unwrap();
		let failed = judge().unwrap();
		assert_eq!(failed.state, State::Failed);
		assert_eq!(
			failed.why.as_deref(),
			Some("`relay` failed on the canary: not healthy within 60 seconds")
		);

		let again = store.record("relay", Action::Deploy, &platform, None, Outcome::Running, starting);
		store.finish(again.unwrap(), Outcome::Succeeded, None, None).unwrap();
		assert_eq!(judge(), None);
	}
}
