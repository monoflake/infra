//! Rolling a new version out beside the one it replaces: started under a name of its own and
//! checked, the route moved to it, the old one given a grace period and removed, and the app's name
//! handed to the new one. Nothing is snapshotted, since such an app keeps nothing on its node. See
//! spec/architecture/host.md, "An app chooses how it is rolled out, and keeping nothing earns a
//! gapless one".

use crate::replace::Error;
use std::future::Future;
use std::time::Duration;

/// How long the version being replaced has, after SIGTERM, to finish what it is answering -- a
/// socket that stays open included -- before it is killed.
pub const GRACE: Duration = Duration::from_secs(30);

/// The name a new version runs under beside its predecessor, until it takes the app's own. An
/// underscore no app's name may hold, so it is never another app's.
pub fn beside_of(app: &str) -> String {
	format!("{app}_next")
}

/// How far a rollout beside the running version has come.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
	/// The new version is being started under its own name.
	Starting,
	/// It is answering its health check.
	Checking,
	/// The route is moving to it.
	Switching,
	/// The version before is finishing what it answers, and then goes.
	Draining,
}

/// What a rollout beside the running version asks of the node: host answers it with Docker and
/// Caddy, a test with a record of what was asked.
pub trait Node {
	/// It has moved on to `step`.
	fn reached(&self, step: Step);
	/// Start the new version under `container`.
	fn start(&self, container: &str) -> impl Future<Output = Result<(), String>> + Send;
	/// Its health check, as a replacement's is.
	fn check(&self, container: &str) -> impl Future<Output = Result<(), String>> + Send;
	/// The last lines `container` wrote.
	fn logs(&self, container: &str) -> impl Future<Output = String> + Send;
	/// Route the app to `container`, or to the container under its own name when none.
	fn route(&self, to: Option<&str>) -> impl Future<Output = Result<(), String>> + Send;
	/// Stop `container` with `grace` after SIGTERM, keep its log, and remove it.
	fn retire(
		&self,
		container: &str,
		grace: Duration,
	) -> impl Future<Output = Result<(), String>> + Send;
	/// Keep `container`'s log and remove it at once, where it is there.
	fn discard(&self, container: &str) -> impl Future<Output = ()> + Send;
	/// Give `from` the name `to`.
	fn rename(&self, from: &str, to: &str) -> impl Future<Output = Result<(), String>> + Send;
}

/// Put a new version of `app` in place of the running one without stopping it first. Until the
/// route moves, a failure removes the new container and leaves the running one as it was.
pub async fn roll(node: &impl Node, app: &str) -> Result<(), Error> {
	let next = beside_of(app);
	// What an earlier rollout cut short left under the name.
	node.discard(&next).await;
	node.reached(Step::Starting);
	if let Err(reason) = node.start(&next).await {
		return Err(abandon(node, &next, reason).await);
	}
	node.reached(Step::Checking);
	if let Err(reason) = node.check(&next).await {
		return Err(abandon(node, &next, reason).await);
	}
	node.reached(Step::Switching);
	if let Err(reason) = node.route(Some(&next)).await {
		// The file Caddy starts from may already name the new one.
		if let Err(error) = node.route(None).await {
			eprintln!("deploy: routing {app} back to the running version: {error}");
		}
		return Err(abandon(node, &next, format!("Caddy did not take the switch: {reason}")).await);
	}
	node.reached(Step::Draining);
	let switched = |what: &str, reason: String| {
		Error::Switched(format!("`{next}` answers for `{app}`, and {what} failed: {reason}"))
	};
	node
		.retire(app, GRACE)
		.await
		.map_err(|reason| switched("retiring the version before", reason))?;
	node.rename(&next, app).await.map_err(|reason| switched("giving it the app's name", reason))
}

/// The new version given up: its last lines kept for the answer, and the container gone.
async fn abandon(node: &impl Node, next: &str, reason: String) -> Error {
	let logs = node.logs(next).await;
	node.discard(next).await;
	Error::Beside { reason, logs }
}

#[cfg(test)]
mod tests {
	use super::{GRACE, Node, Step, roll};
	use crate::replace::Error;
	use std::sync::Mutex;
	use std::time::Duration;

	/// A node that records each thing asked of it, failing the one step named.
	#[derive(Default)]
	struct Recorded {
		asked: Mutex<Vec<String>>,
		failing: Option<&'static str>,
	}

	impl Recorded {
		fn failing(step: &'static str) -> Self {
			Self { failing: Some(step), ..Self::default() }
		}

		fn ask(&self, what: String) -> Result<(), String> {
			let verb = what.split(' ').next().unwrap_or_default().to_owned();
			self.asked.lock().unwrap().push(what);
			if self.failing == Some(verb.as_str()) { Err(format!("{verb} failed")) } else { Ok(()) }
		}

		fn asked(&self) -> Vec<String> {
			self.asked.lock().unwrap().clone()
		}
	}

	impl Node for Recorded {
		fn reached(&self, step: Step) {
			self.asked.lock().unwrap().push(format!("reached {step:?}"));
		}
		async fn start(&self, container: &str) -> Result<(), String> {
			self.ask(format!("start {container}"))
		}
		async fn check(&self, container: &str) -> Result<(), String> {
			self.ask(format!("check {container}"))
		}
		async fn logs(&self, container: &str) -> String {
			format!("the last lines of {container}")
		}
		async fn route(&self, to: Option<&str>) -> Result<(), String> {
			self.ask(format!("route {}", to.unwrap_or("back")))
		}
		async fn retire(&self, container: &str, grace: Duration) -> Result<(), String> {
			self.ask(format!("retire {container} {}s", grace.as_secs()))
		}
		async fn discard(&self, container: &str) {
			self.asked.lock().unwrap().push(format!("discard {container}"));
		}
		async fn rename(&self, from: &str, to: &str) -> Result<(), String> {
			self.ask(format!("rename {from} {to}"))
		}
	}

	#[tokio::test]
	async fn the_new_version_is_checked_and_routed_before_the_old_one_is_touched() {
		let node = Recorded::default();
		roll(&node, "geo").await.unwrap();
		assert_eq!(GRACE.as_secs(), 30);
		assert_eq!(
			node.asked(),
			[
				"discard geo_next",
				"reached Starting",
				"start geo_next",
				"reached Checking",
				"check geo_next",
				"reached Switching",
				"route geo_next",
				"reached Draining",
				"retire geo 30s",
				"rename geo_next geo",
			]
		);
	}

	#[tokio::test]
	async fn a_failed_check_removes_the_new_one_and_leaves_the_old_one_running_and_routed() {
		let node = Recorded::failing("check");
		let failed = roll(&node, "geo").await.unwrap_err();
		let Error::Beside { reason, logs } = failed else { panic!("{failed:?}") };
		assert_eq!((reason.as_str(), logs.as_str()), ("check failed", "the last lines of geo_next"));
		let asked = node.asked();
		assert_eq!(asked.last().map(String::as_str), Some("discard geo_next"));
		// Nothing was asked of the running version, nor of the route.
		assert!(asked.iter().all(|what| !what.ends_with(" geo") && !what.starts_with("route")));
	}

	#[tokio::test]
	async fn a_new_one_that_does_not_start_is_removed_too() {
		let node = Recorded::failing("start");
		assert!(matches!(roll(&node, "geo").await, Err(Error::Beside { .. })));
		let asked = node.asked();
		assert_eq!(asked.last().map(String::as_str), Some("discard geo_next"));
		assert!(!asked.iter().any(|what| what.starts_with("check") || what.starts_with("retire")));
	}

	#[tokio::test]
	async fn a_switch_caddy_refuses_is_routed_back_and_the_old_one_kept() {
		let node = Recorded::failing("route");
		let failed = roll(&node, "geo").await.unwrap_err();
		assert!(matches!(&failed, Error::Beside { reason, .. } if reason.starts_with("Caddy did not")));
		let asked = node.asked();
		let tail: Vec<&str> = asked.iter().rev().take(3).rev().map(String::as_str).collect();
		assert_eq!(tail, ["route geo_next", "route back", "discard geo_next"]);
		assert!(!asked.iter().any(|what| what.starts_with("retire")));
	}

	#[tokio::test]
	async fn once_switched_a_failure_says_the_new_one_answers() {
		let node = Recorded::failing("retire");
		let failed = roll(&node, "geo").await.unwrap_err();
		assert!(matches!(&failed, Error::Switched(why) if why.starts_with("`geo_next` answers")));
		// It is routed and healthy, so it is not taken away: the one discard is the first clean-up.
		assert_eq!(node.asked().iter().filter(|what| *what == "discard geo_next").count(), 1);
		assert_eq!(node.asked().last().map(String::as_str), Some("retire geo 30s"));
	}
}
