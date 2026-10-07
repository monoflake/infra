//! A new host, keeper or Caddy reaches one node first. The canary takes such a run as it comes;
//! every other node holds it and asks the canary, over the tailnet, until the canary's verdict on
//! it passes, fails, or a deadline does. See spec/architecture/host.md, "A new host, keeper or
//! Caddy reaches the canary first".

use serde::{Deserialize, Serialize};
use std::future::Future;
use std::time::Duration;

/// Infra's own three, any of which a run that built it puts on the canary first.
pub const GATED: [&str; 3] = ["host", "keeper", "caddy"];

/// How long a node holds a run for the canary before it passes the run over.
pub const DEADLINE: Duration = Duration::from_secs(2 * 60 * 60);

/// The first wait between two asks; each after is twice the last, up to `LONGEST_WAIT`.
const FIRST_WAIT: Duration = Duration::from_secs(30);
const LONGEST_WAIT: Duration = Duration::from_secs(5 * 60);

/// How long one ask of the canary may take.
pub const ASKING: Duration = Duration::from_secs(10);

/// The label the canary's Caddy answers its verdicts on, under the private suffix, on its tailnet
/// address alone.
pub const LABEL: &str = "canary";

/// The tailnet's addresses, the only sources the canary's Caddy takes the route from.
pub const TAILNET: &str = "100.64.0.0/10";

/// Where this node stands, from `CANARY` in the node's `.env`: the canary itself, a node that
/// asks the canary at its tailnet address, or a node with no canary at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Canary {
	None,
	Itself,
	At(std::net::Ipv4Addr),
}

impl Canary {
	/// `CANARY` read: `self` on the canary, its tailnet IPv4 on every other node, empty for none.
	pub fn parse(value: &str) -> Result<Self, String> {
		match value.trim() {
			"" => Ok(Canary::None),
			"self" => Ok(Canary::Itself),
			address => address.parse().map(Canary::At).map_err(|_| address.to_owned()),
		}
	}
}

/// Whether a run that built `apps` reaches the canary first.
pub fn gated<'a>(mut apps: impl Iterator<Item = &'a str>) -> bool {
	apps.any(|app| GATED.contains(&app))
}

/// The path of the canary's verdict on `repository`'s `run`, for the gated apps it built.
pub fn path(repository: &str, run: u64, built: &[&str]) -> String {
	format!("/api/runs/{repository}/{run}?built={}", built.join(","))
}

/// What the canary says of a run.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
	/// Not taken yet, still being deployed, or deployed and not answering its health yet.
	Pending,
	/// Every gated app the run built was deployed there and answers its health.
	Passed,
	/// One of them failed there, or was passed over.
	Failed,
}

/// The canary's verdict, as its route answers it inside host's envelope.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Verdict {
	pub state: State,
	/// Why, when it has not passed.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub why: Option<String>,
}

/// How holding a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Held {
	/// The canary passed it: deploy it.
	Passed,
	/// The canary failed it, and why.
	Failed(String),
	/// The deadline passed with no verdict; the last answer, or why there was none.
	Expired(String),
	/// It was deployed by hand meanwhile.
	Released,
}

/// The waits between asks: 30 s, doubling to five minutes, then five minutes each.
pub fn waits() -> impl Iterator<Item = Duration> {
	std::iter::successors(Some(FIRST_WAIT), |wait| Some((*wait * 2).min(LONGEST_WAIT)))
}

/// Ask with `ask` until the canary's verdict passes or fails, `released` says it was deployed by
/// hand, or `deadline` has been waited out, sleeping each of `waits` by `sleep` between asks. An
/// ask that fails is waited past like a pending verdict: the canary may be restarting.
pub async fn hold<A, F, S, W>(
	mut ask: A,
	released: impl Fn() -> bool,
	deadline: Duration,
	waits: impl Iterator<Item = Duration>,
	mut sleep: S,
) -> Held
where
	A: FnMut() -> F,
	F: Future<Output = Result<Verdict, String>>,
	S: FnMut(Duration) -> W,
	W: Future<Output = ()>,
{
	let mut waited = Duration::ZERO;
	let mut waits = waits;
	loop {
		if released() {
			return Held::Released;
		}
		let last = match ask().await {
			Ok(Verdict { state: State::Passed, .. }) => return Held::Passed,
			Ok(Verdict { state: State::Failed, why }) => {
				return Held::Failed(why.unwrap_or_else(|| "no reason given".into()));
			}
			Ok(Verdict { state: State::Pending, why }) => why.unwrap_or_else(|| "pending".into()),
			Err(error) => error,
		};
		let wait = waits.next().unwrap_or(LONGEST_WAIT);
		if waited + wait > deadline {
			return Held::Expired(last);
		}
		sleep(wait).await;
		waited += wait;
	}
}

/// Ask the canary at `address` for its verdict on `repository`'s `run`, with the read token every
/// node shares, through its Caddy as `canary.<private_suffix>`.
pub async fn ask(
	address: std::net::Ipv4Addr,
	private_suffix: &str,
	read_token: &str,
	repository: &str,
	run: u64,
	built: &[&str],
) -> Result<Verdict, String> {
	let host = format!("{LABEL}.{private_suffix}");
	let path = path(repository, run, built);
	let at = format!("{address}:80");
	let (status, body) = crate::http::get_as(&at, &host, &path, read_token, ASKING)
		.await
		.map_err(|error| format!("asking the canary at {address}: {error}"))?;
	if !(200..300).contains(&status) {
		return Err(format!("the canary at {address} answered {status}"));
	}
	#[derive(Deserialize)]
	struct Envelope {
		data: Verdict,
	}
	serde_json::from_str::<Envelope>(&body)
		.map(|envelope| envelope.data)
		.map_err(|error| format!("the canary at {address} answered what is not a verdict: {error}"))
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::cell::{Cell, RefCell};

	fn verdict(state: State, why: Option<&str>) -> Result<Verdict, String> {
		Ok(Verdict { state, why: why.map(str::to_owned) })
	}

	/// Hold with `answers` given in turn, recording each wait rather than sleeping it.
	async fn held(
		answers: Vec<Result<Verdict, String>>,
		released_after: Option<usize>,
		deadline: Duration,
	) -> (Held, Vec<u64>) {
		let answers = RefCell::new(answers.into_iter());
		let asked = Cell::new(0);
		let slept = RefCell::new(Vec::new());
		let ask = || {
			asked.set(asked.get() + 1);
			let next = answers.borrow_mut().next().unwrap_or_else(|| verdict(State::Pending, None));
			async move { next }
		};
		let released = || released_after.is_some_and(|after| asked.get() >= after);
		let sleep = |wait: Duration| {
			slept.borrow_mut().push(wait.as_secs());
			async {}
		};
		let held = hold(ask, released, deadline, waits(), sleep).await;
		(held, slept.into_inner())
	}

	#[tokio::test]
	async fn a_run_is_held_until_the_canary_passes_it_asking_less_often_each_time() {
		let answers = vec![
			verdict(State::Pending, Some("keeper is deploying")),
			Err("connection refused".into()),
			verdict(State::Pending, None),
			verdict(State::Passed, None),
		];
		let (held, slept) = held(answers, None, DEADLINE).await;
		assert_eq!(held, Held::Passed);
		assert_eq!(slept, [30, 60, 120]);
	}

	#[tokio::test]
	async fn a_run_the_canary_failed_is_given_up_with_its_reason() {
		let answers = vec![verdict(State::Pending, None), verdict(State::Failed, Some("caddy failed"))];
		assert_eq!(held(answers, None, DEADLINE).await.0, Held::Failed("caddy failed".into()));
	}

	#[tokio::test]
	async fn the_deadline_ends_the_hold_with_the_last_answer() {
		let answers = vec![Err("connection refused".into())];
		let (held, slept) = held(answers, None, DEADLINE).await;
		assert_eq!(held, Held::Expired("pending".into()));
		// 30, 60, 120, 240, then five minutes at a time, never past two hours in all.
		assert_eq!(&slept[..5], [30, 60, 120, 240, 300]);
		assert!(slept.iter().sum::<u64>() <= DEADLINE.as_secs());
		assert!(slept.iter().sum::<u64>() + 300 > DEADLINE.as_secs());
	}

	#[tokio::test]
	async fn a_run_deployed_by_hand_meanwhile_is_released() {
		assert_eq!(held(vec![], Some(2), DEADLINE).await.0, Held::Released);
	}

	#[test]
	fn a_node_is_the_canary_asks_one_or_has_none() {
		assert_eq!(Canary::parse(""), Ok(Canary::None));
		assert_eq!(Canary::parse("self"), Ok(Canary::Itself));
		assert_eq!(Canary::parse("100.101.102.103"), Ok(Canary::At([100, 101, 102, 103].into())));
		assert_eq!(Canary::parse("nrt"), Err("nrt".into()));
	}

	#[test]
	fn only_a_run_that_built_host_keeper_or_caddy_is_held() {
		assert!(gated(["geo", "caddy"].into_iter()));
		assert!(gated(["keeper"].into_iter()));
		assert!(!gated(["geo", "meter", "tunnel", "resolver"].into_iter()));
		assert_eq!(
			path("monoflake/infra", 42, &["caddy", "keeper"]),
			"/api/runs/monoflake/infra/42?built=caddy,keeper"
		);
	}
}
