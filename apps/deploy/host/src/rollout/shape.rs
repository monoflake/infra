//! How the node runs an app: the shape each name or granted role is given, and the environment it
//! is run with. See spec/architecture/host.md, "An app's environment is two files".

use super::Error;
use super::admit::RESOLVER;
use crate::Host;
use crate::grants::Role;
use deploy::Shape;
use deploy::manifest::Manifest;
use std::path::PathBuf;

/// How the node runs an app: keeper in the platform's shape, the meter as an observer, Caddy on the
/// edge, the tunnel at the address Caddy trusts, the resolver on the LAN, and every other app in
/// the role it asks for and the node grants -- the scheduler with every socket-served service it
/// schedules mounted in, the steward, the reporter with the meter's directory -- or sandboxed. All
/// but keeper read their own environment.
pub(super) fn shape_of(host: &Host, manifest: &Manifest) -> Result<Shape, Error> {
	let name = manifest.name.as_str();
	if name == "keeper" {
		let path = &host.config.platform_env;
		let env = deploy::read_env(path)
			.map_err(|source| Error::Environment { path: path.display().to_string(), source })?;
		return Ok(Shape::Platform { env });
	}
	let mut env = crate::environment::variables(&host.volumes.root(name))?;
	// Every app is told which node it runs on, whatever its own files say. See
	// spec/architecture/host.md, "An app's environment is two files".
	bound(&mut env, vec![format!("NODE={}", host.config.node)]);
	let sockets = || {
		Ok(
			crate::cron::socket_services(&host.store.apps()?)
				.into_iter()
				.map(|service| {
					let directory = host.volumes.data(&service);
					(service, directory)
				})
				.collect(),
		)
	};
	let role = host.config.grants.shape_of(manifest)?;
	shape_named(name, role, env, placed(host), sockets)
}

pub(super) fn placed(host: &Host) -> Placed<'_> {
	Placed {
		tunnel: &host.config.caddy.tunnel_source,
		meter: host.volumes.data(crate::node::METER),
		lan: host.config.resolver.address.as_deref(),
	}
}

/// What a shape is given from the node beside its environment.
pub(super) struct Placed<'a> {
	/// The one address Caddy believes a visitor's address from.
	pub(super) tunnel: &'a str,
	/// The meter's data directory, which the reporter shape mounts.
	pub(super) meter: PathBuf,
	/// The node's LAN address, which the resolver publishes DNS on; absent where it runs none.
	pub(super) lan: Option<&'a str>,
}

/// `shape_of` for every name but keeper's, apart from the host it reads: infra's own by name, and
/// any other by the role granted it. `sockets` is asked only for the scheduler, since it reads the
/// store.
pub(super) fn shape_named(
	name: &str,
	role: Option<Role>,
	env: Vec<String>,
	placed: Placed<'_>,
	sockets: impl FnOnce() -> Result<Vec<(String, PathBuf)>, Error>,
) -> Result<Shape, Error> {
	Ok(match (name, role) {
		(crate::node::METER, _) => Shape::Observer { env },
		("caddy", _) => Shape::Edge { env },
		("tunnel", _) => Shape::Tunnel { env, address: placed.tunnel.to_owned() },
		(RESOLVER, _) => {
			let address = placed.lan.ok_or(Error::NoLanAddress)?;
			Shape::Resolver { env, address: address.to_owned() }
		}
		(_, Some(Role::Scheduler)) => Shape::Scheduler { env, sockets: sockets()? },
		(_, Some(Role::Steward)) => Shape::Steward { env },
		(_, Some(Role::Reporter)) => Shape::Reporter { env, meter: placed.meter },
		(_, Some(Role::Peer)) => Shape::Peer { env },
		// Sandboxed as any app; what it is given is every app's network, by `route::proxy_into`.
		(_, Some(Role::Proxy)) => Shape::Sandboxed { env },
		_ => Shape::Sandboxed { env },
	})
}

/// Every name in `binding` in place of whatever the app's own files said under it.
pub(crate) fn bound(env: &mut Vec<String>, binding: Vec<String>) {
	let names: Vec<String> = binding
		.iter()
		.filter_map(|line| line.split_once('='))
		.map(|(name, _)| format!("{name}="))
		.collect();
	env.retain(|line| !names.iter().any(|name| line.starts_with(name.as_str())));
	env.extend(binding);
}

#[cfg(test)]
mod tests {
	use super::{Placed, shape_named};
	#[test]
	fn infra_is_shaped_by_name_and_every_other_app_by_the_role_it_is_granted() {
		use crate::grants::Role;
		use deploy::Shape;
		use std::path::PathBuf;
		let placed = || Placed {
			tunnel: "172.30.0.2",
			meter: PathBuf::from("/data/apps/meter/data"),
			lan: Some("10.0.0.11"),
		};
		let unasked = || -> Result<Vec<(String, PathBuf)>, crate::rollout::Error> {
			panic!("only the scheduler's shape reads the store")
		};
		let reporter = Some(Role::Reporter);
		let shape = shape_named("telemetry", reporter, vec!["A=1".into()], placed(), unasked).unwrap();
		let Shape::Reporter { env, meter } = shape else { panic!("{shape:?}") };
		assert_eq!((env, meter), (vec!["A=1".to_owned()], PathBuf::from("/data/apps/meter/data")));
		// The same name with no role granted is any app; a role, not a name, is what is shaped.
		let plain = shape_named("telemetry", None, vec![], placed(), unasked).unwrap();
		assert!(matches!(plain, Shape::Sandboxed { .. }));
		let geo = shape_named("geo", None, vec![], placed(), unasked).unwrap();
		assert!(matches!(geo, Shape::Sandboxed { .. }));
		let scheduler = Some(Role::Scheduler);
		let cron = shape_named("cron", scheduler, vec![], placed(), || Ok(vec![])).unwrap();
		assert!(matches!(cron, Shape::Scheduler { .. }));
		let apt = shape_named("apt", Some(Role::Steward), vec![], placed(), unasked).unwrap();
		assert!(matches!(apt, Shape::Steward { .. }));
		let relay = shape_named("relay", Some(Role::Peer), vec![], placed(), unasked).unwrap();
		assert!(matches!(relay, Shape::Peer { .. }));
		let meter = shape_named("meter", None, vec![], placed(), unasked).unwrap();
		assert!(matches!(meter, Shape::Observer { .. }));
		let resolver = shape_named("resolver", None, vec![], placed(), unasked).unwrap();
		let Shape::Resolver { address, .. } = resolver else { panic!("{resolver:?}") };
		assert_eq!(address, "10.0.0.11");
		let nowhere = Placed { lan: None, ..placed() };
		assert!(matches!(
			shape_named("resolver", None, vec![], nowhere, unasked),
			Err(crate::rollout::Error::NoLanAddress)
		));
	}
}
