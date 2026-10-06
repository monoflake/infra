//! A deploy as host sees it: admit the declaration, replace the container through the shared
//! procedure, then record the new version, route it and collect what is no longer needed. See
//! spec/architecture/host.md.

mod admit;
mod panel;
mod route;
mod run;
mod shape;
mod tell;
mod version;

pub use admit::deployable;
pub use panel::{PLATFORM, act, itself, redeploy, restorable, rollback};
pub use route::{attach, render, route};
pub use run::from_run;
#[cfg(test)]
pub(crate) use shape::bound;
pub use tell::{tell_cron, tell_telemetry};
pub use version::from_archive;

use crate::{caddy, store};
use deploy::manifest::Invalid;
use deploy::{engine, replace};

#[derive(Debug, thiserror::Error)]
pub enum Error {
	#[error(transparent)]
	Invalid(#[from] Invalid),
	#[error("the resolver needs the node's LAN address, `LAN_ADDRESS`, to publish DNS on")]
	NoLanAddress,
	#[error("writing the resolver's configuration: {0}")]
	Resolver(#[from] caddy::Error),
	#[error("port {port} is already `{holder}`'s")]
	PortTaken { port: u16, holder: String },
	#[error(transparent)]
	Store(#[from] store::Error),
	#[error(transparent)]
	Replace(#[from] replace::Error),
	#[error(transparent)]
	Engine(#[from] engine::Error),
	#[error("the node's environment at {path}: {source}")]
	Environment { path: String, source: std::io::Error },
	#[error("the archive: {0}")]
	Archive(std::io::Error),
	#[error("host does not act on itself; keeper replaces it")]
	Itself,
	#[error("`{0}` is the platform's own: it is restarted, never stopped")]
	Platform(String),
	#[error("`{0}` is not an app this node runs")]
	NoSuchApp(String),
	#[error("`{0}` has no previous version to go back to")]
	NoPrevious(String),
	#[error("the snapshot from before `{0}`'s version was deployed is no longer kept")]
	NoSnapshot(String),
	#[error("only start, stop and restart act on a container as it is")]
	NotAnAct,
	#[error("the app's environment: {0}")]
	AppEnvironment(#[from] crate::environment::Error),
	/// Docker would not load the archive: it is the upload that is wrong, not the node.
	#[error("the archive did not load: {0}")]
	Load(engine::Error),
	#[error("`{0}` declares `[{1}]`, and `{1}`, the driver, is not deployed on this node")]
	NoDriver(String, &'static str),
	#[error("`{0}` is the driver every sidecar runs, and has no container of its own to act on")]
	Driver(String),
	#[error(transparent)]
	Refused(#[from] crate::grants::Refused),
}

#[derive(Debug, serde::Serialize)]
pub struct Outcome {
	pub name: String,
	pub image: String,
	/// Whether Caddy took the new configuration. A deploy that ran but could not be routed is
	/// still a deploy; this says which half is missing.
	pub routed: Result<(), String>,
}
