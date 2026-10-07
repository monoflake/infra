//! A version rolled out beside the one it replaces, as host does it: whether there is room for two,
//! and each step of `deploy::beside` done with Docker and Caddy and recorded on the deploy's event.
//! See spec/architecture/host.md, "An app chooses how it is rolled out, and keeping nothing earns a
//! gapless one".

use super::Error;
use super::route::switch;
use crate::Host;
use crate::caddy::Switched;
use crate::store::Stage;
use deploy::beside::{Node, Step};
use deploy::engine::{DEFAULT_MEMORY_MB, network_of};
use deploy::manifest::Rollout;
use deploy::replace::Asked;
use deploy::{Shape, Version, replace};
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

/// What the node keeps free beside the new version's whole ceiling while both run: room for host,
/// the page cache the running apps lean on, and a version that has not yet settled into its own.
pub(super) const MARGIN_MB: u64 = 256;

/// Where the machine says how much memory it could give without swapping. host runs privileged, so
/// this is the machine's, not its own container's.
const MEMINFO: &str = "/proc/meminfo";

/// How a version is put in place.
#[derive(Debug, PartialEq)]
pub(super) enum Plan {
	/// Beside the running one.
	Beside,
	/// Stopped and started again in place, with why when the app asked to be rolled out beside.
	Replace(Option<String>),
}

/// How `next` takes `current`'s place: beside it when it asks to, something runs to be beside,
/// that runs twice as well, and the node has `available_mb` for both; in place otherwise.
pub(super) fn plan(next: &Version, current: Option<&Version>, available_mb: Option<u64>) -> Plan {
	if next.manifest.rollout != Rollout::Beside {
		return Plan::Replace(None);
	}
	// A first deploy has nothing running to be beside.
	let Some(current) = current else { return Plan::Replace(None) };
	if let Err(why) = current.manifest.check_beside() {
		return Plan::Replace(Some(format!("rolled out in place, as the version before was: {why}")));
	}
	let ceiling = next.manifest.container.as_ref().and_then(|container| container.memory_mb);
	let needed = u64::from(ceiling.unwrap_or(DEFAULT_MEMORY_MB)) + MARGIN_MB;
	match available_mb {
		None => Some(format!("rolled out in place: {MEMINFO} did not say how much memory is free")),
		Some(available) if available < needed => Some(format!(
			"rolled out in place: {available} MiB available, and running beside needs {needed}"
		)),
		Some(_) => None,
	}
	.map_or(Plan::Beside, |why| Plan::Replace(Some(why)))
}

/// `MemAvailable` in MiB, from `/proc/meminfo` as the kernel writes it.
pub(super) fn available_mb(meminfo: &str) -> Option<u64> {
	let line = meminfo.lines().find_map(|line| line.strip_prefix("MemAvailable:"))?;
	let kib = line.trim().strip_suffix("kB")?.trim().parse::<u64>().ok()?;
	Some(kib / 1024)
}

/// The machine's `MemAvailable` now.
pub(super) async fn available() -> Option<u64> {
	tokio::fs::read_to_string(MEMINFO).await.ok().as_deref().and_then(available_mb)
}

/// Roll `next` out beside what runs, as event `id`. Its network and directory are made first, as a
/// replacement makes them; nothing is snapshotted.
pub(super) async fn roll(
	host: &Host,
	id: i64,
	next: &Version,
	shape: &Shape,
	members: &[&str],
) -> Result<(), Error> {
	let name = next.manifest.name.as_str();
	host.engine.network(name, members).await?;
	host.volumes.ensure(name).await.map_err(replace::Error::from)?;
	let node = OnHost { host, id, next, shape };
	deploy::beside::roll(&node, name).await?;
	Ok(())
}

/// The node a beside rollout is asked of: host's Docker, its Caddy, and the event it moves on.
struct OnHost<'a> {
	host: &'a Host,
	id: i64,
	next: &'a Version,
	shape: &'a Shape,
}

impl OnHost<'_> {
	fn app(&self) -> &str {
		&self.next.manifest.name
	}

	/// `container`'s address on the app's network. The version beside is asked and routed to by
	/// it: its name is one no app may take, which musl would not look up, and its address
	/// outlasts the rename that gives it the app's.
	async fn address(&self, container: &str) -> Result<IpAddr, String> {
		let network = network_of(self.app());
		let address = self.host.engine.address_on(container, &network).await;
		let address = address.map_err(|error| error.to_string())?;
		address.ok_or_else(|| format!("`{container}` has no address on {network}"))
	}
}

impl Node for OnHost<'_> {
	fn reached(&self, step: Step) {
		let stage = match step {
			Step::Starting => Stage::Starting,
			Step::Checking => Stage::Checking,
			Step::Switching => Stage::Switching,
			Step::Draining => Stage::Draining,
		};
		if let Err(error) = self.host.store.advance(self.id, stage, None) {
			eprintln!("host: recording event {}: {error}", self.id);
		}
	}

	async fn start(&self, container: &str) -> Result<(), String> {
		let data = self.host.volumes.data(self.app());
		let started = self.host.engine.run_as(self.next, self.shape, &data, container).await;
		started.map_err(|error| error.to_string())
	}

	async fn check(&self, container: &str) -> Result<(), String> {
		let port = self.next.manifest.container.as_ref().and_then(|container| container.port);
		let port = port.ok_or("the declaration has no port to check")?;
		let at = SocketAddr::new(self.address(container).await?, port).to_string();
		replace::healthy_at(&self.host.engine, self.next, container, &Asked::At(at)).await
	}

	async fn logs(&self, container: &str) -> String {
		self.host.engine.tail(container).await
	}

	async fn route(&self, to: Option<&str>) -> Result<(), String> {
		let mut switched = Switched::new();
		if let Some(container) = to {
			switched.insert(self.app().to_owned(), self.address(container).await?);
		}
		switch(self.host, &switched).await.map_err(|error| error.to_string())
	}

	async fn retire(&self, container: &str, grace: Duration) -> Result<(), String> {
		let engine = &self.host.engine;
		let failed = |error: deploy::engine::Error| error.to_string();
		// Stopped before its log is kept, so the log holds how it finished.
		engine.stop_within(container, grace).await.map_err(failed)?;
		engine.archive(container, &self.host.volumes.logs(self.app())).await.map_err(failed)?;
		engine.remove(container).await.map_err(failed)
	}

	async fn discard(&self, container: &str) {
		let engine = &self.host.engine;
		if let Err(error) = engine.archive(container, &self.host.volumes.logs(self.app())).await {
			eprintln!("host: keeping the log of {container}: {error}");
		}
		if let Err(error) = engine.remove(container).await {
			eprintln!("host: removing {container}: {error}");
		}
	}

	async fn rename(&self, from: &str, to: &str) -> Result<(), String> {
		self.host.engine.rename(from, to).await.map_err(|error| error.to_string())
	}
}

#[cfg(test)]
mod tests {
	use super::{MARGIN_MB, Plan, available_mb, plan};
	use deploy::Version;
	use deploy::manifest::{Manifest, Rollout};

	fn version(rollout: Rollout, memory_mb: Option<u32>, extra: &str) -> Version {
		let text = format!(
			"version = 1\nname = \"geo\"\nplacements = [\"rdu\"]\n[container]\nport = 23440\n\
			 health = \"/health\"\n{}\n{extra}\n",
			memory_mb.map(|mb| format!("memory_mb = {mb}")).unwrap_or_default()
		);
		let mut manifest = Manifest::parse(&text).unwrap();
		manifest.rollout = rollout;
		Version { manifest, image: "sha256:a".into() }
	}

	#[test]
	fn beside_goes_ahead_only_with_room_for_both() {
		let (next, current) =
			(version(Rollout::Beside, Some(64), ""), version(Rollout::Beside, None, ""));
		let needed = 64 + MARGIN_MB;
		assert_eq!(plan(&next, Some(&current), Some(needed)), Plan::Beside);
		let short = plan(&next, Some(&current), Some(needed - 1));
		let why = format!(
			"rolled out in place: {} MiB available, and running beside needs {needed}",
			needed - 1
		);
		assert_eq!(short, Plan::Replace(Some(why)));
		let Plan::Replace(Some(unread)) = plan(&next, Some(&current), None) else { panic!() };
		assert!(unread.contains("/proc/meminfo"), "{unread}");
		// No ceiling declared is the 512 every container gets.
		let unbounded = version(Rollout::Beside, None, "");
		assert_eq!(plan(&unbounded, Some(&current), Some(512 + MARGIN_MB)), Plan::Beside);
		assert!(matches!(plan(&unbounded, Some(&current), Some(512)), Plan::Replace(Some(_))));
	}

	#[test]
	fn replace_and_a_first_deploy_are_in_place_and_say_nothing() {
		let current = version(Rollout::Replace, None, "");
		let replaced = version(Rollout::Replace, None, "");
		assert_eq!(plan(&replaced, Some(&current), Some(1 << 20)), Plan::Replace(None));
		let manual = version(Rollout::Manual, None, "");
		assert_eq!(plan(&manual, Some(&current), Some(1 << 20)), Plan::Replace(None));
		let first = version(Rollout::Beside, None, "");
		assert_eq!(plan(&first, None, Some(1 << 20)), Plan::Replace(None));
	}

	#[test]
	fn a_version_before_that_could_not_run_twice_is_replaced_in_place() {
		let next = version(Rollout::Beside, None, "");
		let kept = version(Rollout::Replace, None, "[data]\npath = \"/data\"");
		let Plan::Replace(Some(why)) = plan(&next, Some(&kept), Some(1 << 20)) else { panic!() };
		assert!(why.starts_with("rolled out in place, as the version before was"), "{why}");
	}

	#[test]
	fn reads_mem_available_as_the_kernel_writes_it() {
		let meminfo = "MemTotal:        8039912 kB\nMemFree:          412000 kB\n\
			MemAvailable:    3145728 kB\nBuffers:          102400 kB\n";
		assert_eq!(available_mb(meminfo), Some(3072));
		assert_eq!(available_mb("MemTotal: 8039912 kB\n"), None);
		assert_eq!(available_mb("MemAvailable: lots\n"), None);
	}
}
