//! Which names host deploys and what its own network admits: refuse what could not be run before
//! anything is stopped. See spec/architecture/host.md.

use super::Error;
use super::shape::{placed, shape_named};
use crate::Host;
use crate::grants::Role;
use crate::sidecars;
use deploy::Shape;
use deploy::engine::peer_published;
use deploy::manifest::{Invalid, Manifest};

/// Whether host takes a deploy under this name at all: any app's, and keeper, the meter, Caddy, the
/// tunnel and the resolver, the reserved names it deploys. host itself is keeper's to deploy.
pub fn deployable(name: &str) -> Result<(), Invalid> {
	if TAKEN.contains(&name) { Ok(()) } else { deploy::manifest::check_name(name) }
}

/// Infra's own that host deploys, each in the shape its name gives it. Every other app's shape is
/// the role it asks for and the node grants; see crate::grants.
pub(super) const TAKEN: [&str; 5] = ["keeper", "meter", "caddy", "tunnel", RESOLVER];

/// The house's DNS, in a shape of its own and with its configuration written before it starts.
pub(super) const RESOLVER: &str = "resolver";

/// Whether host's own network admits an app run in `shape`: a peer, which reads its own node's
/// host. Nothing else but keeper and Caddy joins it; see `attach`, and keeper's own start.
pub(super) fn admitted(shape: &Shape) -> bool {
	matches!(shape, Shape::Peer { .. })
}

/// Whether host's own network admits `manifest`'s app in the shape `shape_named` gives it, apart
/// from its environment, which no shape's admission reads.
pub(super) fn admits(host: &Host, manifest: &Manifest) -> bool {
	let name = manifest.name.as_str();
	let Ok(role) = host.config.grants.shape_of(manifest) else { return false };
	let shape = shape_named(name, role, Vec::new(), placed(host), || Ok(Vec::new()));
	shape.is_ok_and(|shape| admitted(&shape))
}

/// Infra's own that stand on no network of their own: the meter has none, and Caddy and the tunnel
/// stand on the edge. A granted driver runs no container, so has none either.
pub(super) const UNNETWORKED: [&str; 3] = ["meter", "caddy", "tunnel"];

/// Refuse an app that asks for an architecture this node runs neither natively nor by emulation.
/// See spec/architecture/host.md, "An app may ask for one architecture".
pub(super) fn runnable(host: &Host, manifest: &Manifest) -> Result<(), Error> {
	let Some(arch) = manifest.arch.as_deref().filter(|arch| !host.config.runs(arch)) else {
		return Ok(());
	};
	let native = host.config.native.unwrap_or("unknown").to_owned();
	Err(Error::Unrunnable { app: manifest.name.clone(), arch: arch.to_owned(), native })
}

/// The first of `mine` that one of `others` publishes already, and which.
fn clash<'a>(
	mine: &[u16],
	mut others: impl Iterator<Item = (&'a str, Vec<u16>)>,
) -> Option<(u16, &'a str)> {
	others.find_map(|(name, theirs)| {
		mine.iter().find(|port| theirs.contains(port)).map(|port| (*port, name))
	})
}

/// Refuse what could not be run before anything is stopped.
pub fn admit(host: &Host, requested: &str, manifest: &Manifest) -> Result<(), Error> {
	deployable(requested)?;
	if TAKEN.contains(&requested) {
		manifest.check_own(requested, &host.config.node)?;
	} else {
		manifest.check(requested, &host.config.node)?;
	}
	let Some(container) = manifest.container.as_ref() else {
		return Err(deploy::manifest::Invalid::NoContainer(manifest.name.clone()).into());
	};
	// Refused before anything is stopped, as everything here is.
	let role = host.config.grants.shape_of(manifest)?;
	runnable(host, manifest)?;
	// Docker refuses a port another container publishes only once the old version is stopped.
	if role == Some(Role::Peer) {
		let apps = host.store.apps()?;
		let others = apps.iter().filter(|app| app.manifest.name != manifest.name).filter(|app| {
			host.config.grants.shape_of(&app.manifest).is_ok_and(|role| role == Some(Role::Peer))
		});
		let published = others.map(|app| (app.manifest.name.as_str(), peer_published(&app.manifest)));
		if let Some((port, holder)) = clash(&peer_published(manifest), published) {
			return Err(Error::PortTaken { port, holder: holder.to_owned() });
		}
	}
	for driver in manifest.drivers() {
		if sidecars::driver(host, driver)?.is_none() {
			return Err(Error::NoDriver(manifest.name.clone(), driver.name()));
		}
	}
	// An app on a socket holds no port.
	let Some(port) = container.port else { return Ok(()) };
	let holder = host.store.apps()?.into_iter().find(|app| {
		app.manifest.name != manifest.name
			&& app.manifest.container.as_ref().and_then(|container| container.port) == Some(port)
	});
	if let Some(holder) = holder {
		return Err(Error::PortTaken { port, holder: holder.manifest.name });
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::{TAKEN, admitted, clash, deployable};

	#[test]
	fn a_peer_may_not_publish_a_port_another_peer_publishes() {
		let others = || [("database", vec![5432, 8008]), ("relay", vec![12012])].into_iter();
		assert_eq!(clash(&[2379, 2380], others()), None);
		assert_eq!(clash(&[2379, 8008], others()), Some((8008, "database")));
		assert_eq!(clash(&[12012], others()), Some((12012, "relay")));
	}
	use crate::rollout::shape::{Placed, shape_named};
	use deploy::manifest::{Invalid, OWN};

	#[test]
	fn deploys_every_name_the_platform_owns_but_its_own() {
		// host is deployed by keeper, never by itself; every other name the manifest reserves for
		// the platform is one this host deploys, so the two lists cannot drift apart.
		let deployed: Vec<&str> = OWN.into_iter().filter(|name| *name != "host").collect();
		assert_eq!(deployed.len(), TAKEN.len());
		assert!(deployed.iter().all(|name| TAKEN.contains(name)), "{deployed:?}");
	}

	#[test]
	fn host_takes_keeper_and_any_app_but_not_itself() {
		assert!(deployable("geo").is_ok());
		// keeper is reserved for every app and still deployable by host, which is the whole of how
		// keeper arrives on a node; turning it away here once stopped the first one arriving.
		assert!(deployable("keeper").is_ok());
		assert!(deployable("meter").is_ok());
		assert!(deployable("caddy").is_ok());
		assert!(deployable("tunnel").is_ok());
		assert!(deployable("objects").is_ok());
		assert!(deployable("postgres").is_ok());
		// Above infra a name is any app's; the node's grants, not the name, make it more.
		assert!(deployable("cron").is_ok());
		assert!(deployable("gateway").is_ok());
		assert_eq!(deployable("geo-postgres"), Err(Invalid::Reserved("geo-postgres".into())));
		assert_eq!(deployable("host"), Err(Invalid::Reserved("host".into())));
		assert_eq!(deployable("api"), Err(Invalid::Reserved("api".into())));
		assert_eq!(deployable("geo-objects"), Err(Invalid::Reserved("geo-objects".into())));
	}

	#[test]
	fn host_admits_a_granted_peer_and_nothing_else() {
		use crate::grants::Role;
		use std::path::PathBuf;
		let admits = |name: &str, role: Option<Role>| {
			let placed = Placed { tunnel: "172.30.0.2", meter: PathBuf::new(), lan: Some("10.0.0.11") };
			let shape = shape_named(name, role, vec![], placed, || Ok(vec![])).unwrap();
			admitted(&shape)
		};
		assert!(!admits("panel", None));
		assert!(admits("relay", Some(Role::Peer)));
		assert!(!admits("relay", None));
		// A proxy joins every app's network, and host's own is not among them.
		assert!(!admits("pgproxy", Some(Role::Proxy)));
		assert!(!admits("geo", None));
		assert!(!admits("telemetry", Some(Role::Reporter)));
		// Infra's own are shaped by name, so a peer granted to one does not make it a peer.
		assert!(!admits("caddy", Some(Role::Peer)));
		assert!(!admits("resolver", Some(Role::Peer)));
	}
}
