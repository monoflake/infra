//! Caddy and the networks it reaches: render it from the state, apply it, and attach it and host to
//! every app's network. See spec/architecture/host.md, "Caddy is deployed like any app, and is the
//! one door".

use super::admit::{RESOLVER, UNNETWORKED, admits};
use crate::{Host, caddy, store};
use deploy::engine;

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
	let (apps, routes) = (host.store.apps()?, host.store.routes()?);
	let lan = host.config.resolver.address.is_some();
	Ok(caddy::render(&host.config.caddy, &apps, &routes, lan))
}

/// Attach Caddy and host to every app's network again. A Caddy container that was recreated
/// rather than restarted comes back attached to none of them.
pub async fn attach(host: &Host) -> Result<(), RouteError> {
	let members = [host.config.own_container.as_str(), host.config.caddy.container.as_str()];
	// host's own network is the panel's, a peer's and keeper's, never Caddy's: nothing is routed
	// to host.
	let own = deploy::engine::network_of(&host.config.own_container);
	host.engine.network(&host.config.own_container, &members[..1]).await?;
	host.engine.leave(&own, &members[1..]).await?;
	for app in host.store.apps()? {
		let name = app.manifest.name.as_str();
		if admits(host, &app.manifest) {
			host.engine.join(&own, &[name], false).await?;
		}
		let driver = host.config.grants.driver_of(&app.manifest).is_some();
		if !UNNETWORKED.contains(&name) && !driver && name != host.config.caddy.container {
			host.engine.network(name, &members).await?;
		}
	}
	Ok(())
}
