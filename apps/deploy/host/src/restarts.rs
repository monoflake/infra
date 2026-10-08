//! A declared restart limit: host counts an app's failing exits as Docker tells of them, and holds
//! the app stopped once `attempts` of them fall within `within`. Docker's own restart policy still
//! revives it in between, host or no host. See spec/architecture/host.md.

use crate::Host;
use crate::store::{self, Action, Outcome, Source};
use deploy::engine::{Ending, STOP_GRACE};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long a `kill` explains the `die` that follows it: a stop's grace and then some.
const ASKED: Duration = Duration::from_secs(120);

/// How long host waits to watch again once Docker's stream has ended.
const AGAIN: Duration = Duration::from_secs(5);

/// Watch Docker's containers end, for as long as host runs.
pub async fn watch(host: Arc<Host>) {
	let mut asked = HashMap::new();
	loop {
		let mut endings = host.engine.endings();
		while let Some(ending) = endings.next().await {
			match ending {
				Ok(Ending::Asked(name)) => {
					asked.insert(name, Instant::now());
				}
				// A stop, a restart or a removal asks before the process ends; an end nothing asked
				// for is the app's own.
				Ok(Ending::Died { name, code }) => {
					if asked.remove(&name).is_some_and(|at| at.elapsed() < ASKED) {
						continue;
					}
					match counted(&host, &name, code, jiff::Timestamp::now()) {
						Ok(true) => {
							tokio::spawn(held(host.clone(), name));
						}
						Ok(false) => {}
						Err(error) => eprintln!("host: counting {name}'s exit: {error}"),
					}
				}
				Err(error) => {
					eprintln!("host: Docker's events: {error}");
					break;
				}
			}
		}
		tokio::time::sleep(AGAIN).await;
	}
}

/// Hold the app container `name` belongs to once nothing else is being done to it, if its limit is
/// still reached then: a deploy or a start in between began its count again.
async fn held(host: Arc<Host>, name: String) {
	let _one = host.deploying.lock().await;
	let why = match reached(&host, app_of(&name), jiff::Timestamp::now()) {
		Ok(Some(why)) => why,
		Ok(None) => return,
		Err(error) => return eprintln!("host: counting {name}'s exits: {error}"),
	};
	if let Err(error) = hold(&host, &name, &why, async || stop(&host, &name).await).await {
		eprintln!("host: holding {name}: {error}");
	}
	crate::rollout::tell_telemetry(&host).await;
}

/// The app a container is: its own, or the next version of one rolled out beside it.
fn app_of(name: &str) -> &str {
	name.strip_suffix("_next").unwrap_or(name)
}

/// Record the exit of container `name` with `code` at `at`, where its app declares a restart limit,
/// is not held and failed; answer whether the limit is reached by it.
fn counted(host: &Host, name: &str, code: i64, at: jiff::Timestamp) -> Result<bool, store::Error> {
	let Some(app) = host.store.app(app_of(name))? else { return Ok(false) };
	let declared =
		app.manifest.container.as_ref().is_some_and(|container| container.restart.is_some());
	if !declared || app.held || code == 0 {
		return Ok(false);
	}
	host.store.exited(&app.manifest.name, Some(&app.image), code, at)?;
	Ok(reached(host, &app.manifest.name, at)?.is_some())
}

/// Why `app` is to be held at `at`, when its last `attempts` failing exits all fall within `within`
/// of it; none when it declares no limit or is held already.
fn reached(host: &Host, app: &str, at: jiff::Timestamp) -> Result<Option<String>, store::Error> {
	let Some(deployed) = host.store.app(app)?.filter(|deployed| !deployed.held) else {
		return Ok(None);
	};
	let Some(limit) = deployed.manifest.container.and_then(|container| container.restart) else {
		return Ok(None);
	};
	let Some(window) = limit.window() else { return Ok(None) };
	let since = at - jiff::SignedDuration::try_from(window).unwrap_or(jiff::SignedDuration::MAX);
	let exits = host.store.exits(app, limit.attempts)?;
	let reached = exits.len() == limit.attempts as usize && exits.iter().all(|exit| *exit >= since);
	Ok(reached.then(|| format!("held: {} failing exits within {}", limit.attempts, limit.within)))
}

/// Hold the app container `name` belongs to stopped, saying `why`, and `stop` the container: the
/// hold first, so nothing host does in between starts it again.
async fn hold(
	host: &Host,
	name: &str,
	why: &str,
	stop: impl AsyncFnOnce() -> Result<(), deploy::engine::Error>,
) -> Result<(), store::Error> {
	let app = app_of(name);
	let image = host.store.app(app)?.map(|app| app.image);
	host.store.hold(app, true)?;
	let source = Source::host();
	let id =
		host.store.record(app, Action::Stop, &source, image.as_deref(), Outcome::Running, None)?;
	match stop().await {
		Ok(()) => host.store.finish(id, Outcome::Succeeded, None, Some(why)),
		Err(error) => {
			let detail = format!("{why}; stopping it: {error}");
			host.store.finish(id, Outcome::Failed, None, Some(&detail))
		}
	}
}

/// Stop container `name`, and its app's sidecars after it when it is the app's own: what the
/// panel's stop does.
async fn stop(host: &Host, name: &str) -> Result<(), deploy::engine::Error> {
	host.engine.stop_within(name, STOP_GRACE).await?;
	if app_of(name) == name {
		let app = host.store.app(name).ok().flatten();
		for sidecar in app.map(|app| app.manifest.sidecars()).unwrap_or_default() {
			host.engine.stop_within(&sidecar, STOP_GRACE).await?;
		}
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::store::Deployed;

	const LIMIT: &str = "restart = { attempts = 3, within = \"10m\" }";

	/// A host running geo, with `lines` added to its `[container]`.
	fn running(root: &std::path::Path, lines: &str) -> Arc<Host> {
		let host = crate::testing(root);
		let geo = include_str!("../../../../libs/deploy/fixtures/geo.toml");
		let geo = geo.replacen("health = \"/health\"", &format!("health = \"/health\"\n{lines}"), 1);
		let deployed = Deployed {
			manifest: deploy::Manifest::parse(&geo).unwrap(),
			image: "sha256:a".into(),
			previous: None,
			deployed_at: "2026-10-08T00:00:00Z".into(),
			held: false,
		};
		host.store.put_app(&deployed).unwrap();
		host
	}

	fn at(minutes: i64) -> jiff::Timestamp {
		"2026-10-08T00:00:00Z".parse::<jiff::Timestamp>().unwrap()
			+ jiff::SignedDuration::from_mins(minutes)
	}

	fn exits(host: &Host) -> usize {
		let events = host.store.events(Some("geo"), None, 100).unwrap();
		events.iter().filter(|event| event.action == Action::Exit).count()
	}

	#[test]
	fn exits_count_within_the_window_and_a_clean_one_never_does() {
		let root = tempfile::tempdir().unwrap();
		let host = running(root.path(), LIMIT);
		// Spread out, no three fall within ten minutes of each other.
		for minutes in [0, 6, 12, 18] {
			assert!(!counted(&host, "geo", 1, at(minutes)).unwrap(), "{minutes}");
		}
		assert!(!counted(&host, "geo", 0, at(19)).unwrap());
		assert!(!counted(&host, "geo_next", 0, at(19)).unwrap());
		assert_eq!(exits(&host), 4);
		assert!(counted(&host, "geo_next", 137, at(20)).unwrap());
		let events = host.store.events(Some("geo"), None, 1).unwrap();
		assert_eq!(events[0].detail.as_deref(), Some("exited with code 137"));
		assert_eq!((events[0].outcome, &*events[0].source.kind), (Outcome::Failed, "host"));
	}

	#[test]
	fn an_app_without_a_limit_is_never_counted() {
		let root = tempfile::tempdir().unwrap();
		let host = running(root.path(), "");
		for minutes in 0..5 {
			assert!(!counted(&host, "geo", 1, at(minutes)).unwrap());
		}
		assert!(!counted(&host, "geo_objects", 1, at(5)).unwrap());
		assert_eq!(exits(&host), 0);
		assert_eq!(reached(&host, "geo", at(5)).unwrap(), None);
	}

	#[tokio::test]
	async fn the_limit_reached_stops_and_holds_the_app_until_a_start() {
		let root = tempfile::tempdir().unwrap();
		let host = running(root.path(), LIMIT);
		let now = jiff::Timestamp::now();
		for seconds in [-30, -20, -10] {
			let exit = now + jiff::SignedDuration::from_secs(seconds);
			assert_eq!(counted(&host, "geo", 1, exit).unwrap(), seconds == -10);
		}
		let why = reached(&host, "geo", now).unwrap().unwrap();
		assert_eq!(why, "held: 3 failing exits within 10m");
		let mut stopped = None;
		hold(&host, "geo", &why, async || {
			stopped = Some("geo");
			Ok(())
		})
		.await
		.unwrap();
		assert_eq!(stopped, Some("geo"));
		assert!(host.store.app("geo").unwrap().unwrap().held);
		let event = &host.store.events(Some("geo"), None, 1).unwrap()[0];
		assert_eq!(
			(event.action, event.outcome, &*event.source.kind),
			(Action::Stop, Outcome::Succeeded, "host")
		);
		assert_eq!(event.detail.as_deref(), Some(why.as_str()));
		// Held, it is counted no more, and is not held again.
		assert!(!counted(&host, "geo", 1, now).unwrap());
		assert_eq!(reached(&host, "geo", now).unwrap(), None);

		// A start by hand ends the hold, as the panel's does, and the count begins again.
		host
			.store
			.record("geo", Action::Start, &Source::panel(), None, Outcome::Succeeded, None)
			.unwrap();
		host.store.hold("geo", false).unwrap();
		assert!(!counted(&host, "geo", 1, now).unwrap());
		assert!(!counted(&host, "geo", 1, now).unwrap());
		assert!(counted(&host, "geo", 1, now).unwrap());
	}

	#[tokio::test]
	async fn a_stop_that_fails_still_holds_and_says_so() {
		let root = tempfile::tempdir().unwrap();
		let host = running(root.path(), LIMIT);
		let why = "held: 3 failing exits within 10m";
		hold(&host, "geo_next", why, async || Err(deploy::engine::Error::NothingLoaded)).await.unwrap();
		assert!(host.store.app("geo").unwrap().unwrap().held);
		let event = &host.store.events(Some("geo"), None, 1).unwrap()[0];
		assert_eq!(event.outcome, Outcome::Failed);
		assert!(
			event
				.detail
				.as_deref()
				.unwrap()
				.starts_with("held: 3 failing exits within 10m; stopping it: ")
		);
	}
}
