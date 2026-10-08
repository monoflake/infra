//! Caddy and the networks it reaches: render it from the state, apply it, and attach it and host to
//! every app's network. See spec/architecture/host.md, "Caddy is deployed like any app, and is the
//! one door".

use super::admit::{RESOLVER, UNNETWORKED, admits};
use crate::grants::Role;
use crate::{Host, caddy, store};
use deploy::engine;
use deploy::manifest::OWN;

#[derive(Debug, thiserror::Error)]
pub enum RouteError {
	#[error(transparent)]
	Store(#[from] store::Error),
	#[error(transparent)]
	Caddy(#[from] caddy::Error),
	#[error(transparent)]
	Engine(#[from] engine::Error),
}

/// Render Caddy from the state as it now is, and apply it. A state that cannot be read is an
/// error and never an empty list: rendering nothing would take every route down.
pub async fn route(host: &Host) -> Result<(), RouteError> {
	let rendered = render(host)?;
	caddy::apply(&host.config.caddy, &rendered).await?;
	// Only into a resolver already deployed: its directory is the subvolume its deploy made.
	if host.store.app(RESOLVER)?.is_some() {
		apply_resolver(host).await?;
	}
	Ok(())
}

/// The resolver's file, again from host's configuration as it now is.
async fn apply_resolver(host: &Host) -> Result<(), RouteError> {
	crate::resolver::apply(&host.config.resolver, &[]).await?;
	Ok(())
}

pub fn render(host: &Host) -> Result<serde_json::Value, store::Error> {
	render_switched(host, &caddy::Switched::new())
}

fn render_switched(
	host: &Host,
	switched: &caddy::Switched,
) -> Result<serde_json::Value, store::Error> {
	let (apps, routes) = (host.store.apps()?, host.store.routes()?);
	let lan = host.config.resolver.address.is_some();
	Ok(caddy::render_switched(&host.config.caddy, &apps, &routes, lan, switched))
}

/// Render Caddy with the apps `switched` names dialed at their address, and apply it: the moment a
/// version beside its predecessor takes the route. Caddy's reload finishes what the old
/// configuration was answering. See spec/architecture/host.md, "An app chooses how it is rolled
/// out, and keeping nothing earns a gapless one".
pub(super) async fn switch(host: &Host, switched: &caddy::Switched) -> Result<(), RouteError> {
	let rendered = render_switched(host, switched)?;
	caddy::apply(&host.config.caddy, &rendered).await?;
	Ok(())
}

/// Attach Caddy and host to every app's network again. A Caddy container that was recreated
/// rather than restarted comes back attached to none of them.
pub async fn attach(host: &Host) -> Result<(), RouteError> {
	let members = [host.config.own_container.as_str(), host.config.caddy.container.as_str()];
	// host's own network is a peer's, keeper's and Caddy's. Caddy is on it for the door alone --
	// the notice and the reads its allowlist names; see caddy::door and spec/architecture/host.md,
	// "host has no interface on the node, and a door Caddy keeps".
	let own = deploy::engine::network_of(&host.config.own_container);
	host.engine.network(&host.config.own_container, &members[..1]).await?;
	host.engine.join(&own, &members[1..], false).await?;
	for app in host.store.apps()? {
		let name = app.manifest.name.as_str();
		if admits(host, &app.manifest) {
			host.engine.join(&own, &[name], false).await?;
		}
		let driver = host.config.grants.driver_of(&app.manifest).is_some();
		if !UNNETWORKED.contains(&name) && !driver && name != host.config.caddy.container {
			host.engine.network(name, &members).await?;
			proxy_into(host, name).await;
		}
	}
	Ok(())
}

/// The app the node grants the proxy role to, when it is deployed here.
fn proxy(host: &Host) -> Option<&str> {
	let name = host.config.grants.holder(Role::Proxy)?;
	host.store.app(name).ok().flatten().map(|_| name)
}

/// Whether `app`'s network takes `proxy`: any app's but infra's own and the proxy's.
pub(super) fn proxied(app: &str, proxy: &str) -> bool {
	app != proxy && !OWN.contains(&app)
}

/// Join the node's proxy to `app`'s network, which has to exist, where it takes one. Logged
/// rather than failing the caller: an app is deployed whether or not the proxy reaches it yet.
pub(super) async fn proxy_into(host: &Host, app: &str) {
	let Some(proxy) = proxy(host).filter(|proxy| proxied(app, proxy)) else { return };
	let network = engine::network_of(app);
	if let Err(error) = host.engine.join(&network, &[proxy], false).await {
		eprintln!("host: joining {proxy} to {network}: {error}");
	}
}

/// Join a newly deployed proxy to every app's network, which as a new container it is on none of.
pub(super) async fn proxy_everywhere(host: &Host) {
	let apps = match host.store.apps() {
		Ok(apps) => apps,
		Err(error) => return eprintln!("host: reading apps for the proxy: {error}"),
	};
	for app in apps {
		let name = app.manifest.name.as_str();
		let driver = host.config.grants.driver_of(&app.manifest).is_some();
		if !UNNETWORKED.contains(&name) && !driver {
			proxy_into(host, name).await;
		}
	}
}

#[cfg(test)]
mod tests {
	use super::{proxied, proxy};

	#[test]
	fn the_proxy_joins_every_apps_network_but_its_own_and_infras() {
		assert!(proxied("ledger", "pgproxy"));
		assert!(proxied("database", "pgproxy"));
		assert!(!proxied("pgproxy", "pgproxy"));
		for own in ["host", "keeper", "caddy", "meter", "tunnel", "resolver"] {
			assert!(!proxied(own, "pgproxy"), "{own}");
		}
	}

	#[test]
	fn the_proxy_is_joined_only_once_it_is_granted_and_deployed() {
		use crate::store::Deployed;
		let directory = tempfile::tempdir().unwrap();
		let host = crate::testing_with(directory.path(), |config| {
			config.grants = crate::grants::Grants::parse("pgproxy:proxy").unwrap();
		});
		assert_eq!(proxy(&host), None);
		let manifest = deploy::Manifest::parse(
			"version = 1\nname = \"pgproxy\"\nplacements = [\"rdu\"]\n[shape]\nkind = \"proxy\"\n\
			 [container]\nport = 15432\nhealth = \"/health\"\n",
		)
		.unwrap();
		let deployed = Deployed {
			manifest,
			image: "sha256:a".into(),
			previous: None,
			deployed_at: String::new(),
			held: false,
		};
		host.store.put_app(&deployed).unwrap();
		assert_eq!(proxy(&host), Some("pgproxy"));
		let elsewhere = tempfile::tempdir().unwrap();
		let ungranted = crate::testing(elsewhere.path());
		ungranted.store.put_app(&deployed).unwrap();
		assert_eq!(proxy(&ungranted), None);
	}
}
