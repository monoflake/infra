//! host: the deployment platform on one node. It takes an app's image and declaration, runs
//! exactly one container for it, puts the previous version back when a new one fails, and renders
//! all of Caddy from what it holds. See spec/architecture/host.md and
//! platform's spec/architecture/services.md.

mod api;
mod caddy;
mod canary;
mod config;
mod cron;
mod environment;
mod grants;
mod images;
mod inspect;
mod node;
mod resolver;
mod restarts;
mod rollout;
mod sidecars;
mod store;
mod telemetry;

use std::sync::Arc;

/// musl's allocator is slow under many small allocations, and images are built for speed; see
/// spec/architecture/host.md, "An image is built for speed, and for any node of its architecture".
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// A notice taken: its repository, its run, the one app the operator named, and whether it was
/// sent by hand.
pub type Taken = (String, u64, Option<String>, bool);

/// Everything a request handler reaches.
pub struct Host {
	pub config: config::Config,
	pub store: store::Store,
	pub engine: deploy::Engine,
	pub volumes: deploy::Volumes,
	pub deploying: tokio::sync::Mutex<()>,
	/// Absent without a GITHUB_ACTIONS_TOKEN and DEPLOY_SOURCES, and then CI's notices are refused.
	pub github: Option<deploy::github::GitHub>,
	/// The runs a notice has been taken for, each by its repository, and the one app named when the
	/// operator named one; and whether it was sent by hand, past the canary.
	pub notices: std::sync::Mutex<std::collections::HashSet<Taken>>,
	/// The runs deployed by hand past the canary, which a notice holding the same run gives up.
	pub released: std::sync::Mutex<std::collections::HashSet<(String, u64)>>,
	/// The images as the background last found them, and what the panel asked of them.
	pub images: images::Images,
	/// When `cron` was last redeployed for its socket mounts, so a read-back that never settles
	/// logs once and stops rather than redeploying it forever. See
	/// `rollout::tell::CRON_MOUNT_REDEPLOY_COOLDOWN`.
	pub cron_mount_redeployed_at: std::sync::Mutex<Option<std::time::Instant>>,
}

/// A host over a directory of its own, reaching no node: the state, the apps' directories and the
/// configuration are there, and Docker is never asked.
#[cfg(test)]
pub(crate) fn testing(root: &std::path::Path) -> Arc<Host> {
	testing_with(root, |_| {})
}

/// `testing`, its configuration changed by `change` first.
#[cfg(test)]
pub(crate) fn testing_with(
	root: &std::path::Path,
	change: impl FnOnce(&mut config::Config),
) -> Arc<Host> {
	use std::path::PathBuf;
	let apps_root = root.join("apps");
	let mut config = config::Config {
		node: "rdu".into(),
		slot: 0,
		token: "full".into(),
		read_token: Some("reader".into()),
		listen: ([127, 0, 0, 1], config::PORT).into(),
		own_container: "host".into(),
		snapshots_root: root.join("snapshots"),
		logs_root: root.join("logs"),
		state: root.join("state"),
		incoming: root.join("incoming"),
		platform_env: root.join(".env"),
		caddy: config::CaddyConfig {
			container: "caddy".into(),
			host: deploy::engine::on_own_network("host", config::PORT),
			admin_socket: root.join("caddy/admin.sock"),
			config_file: root.join("caddy/caddy.json"),
			admin_listen: "unix//data/admin.sock".into(),
			private_suffix: "inside.test".into(),
			public_suffix: "outside.test".into(),
			private_sources: vec![],
			tunnel_source: "172.30.0.20".into(),
			tunnel_source6: None,
			acme_email: "a@example.test".into(),
			dns_resolver: "1.1.1.1".into(),
			public_api: "api.public.test".into(),
			app_sources: vec![],
			private_scopes: vec![],
			canary: false,
		},
		resolver: config::ResolverConfig {
			file: PathBuf::from("/nowhere/Corefile"),
			address: None,
			upstreams: vec![],
		},
		grants: grants::Grants::default(),
		emulate: vec![],
		native: Some("amd64"),
		canary: deploy::canary::Canary::None,
		apps_root: apps_root.clone(),
	};
	change(&mut config);
	Arc::new(Host {
		store: store::Store::open(&config.state).unwrap(),
		engine: deploy::Engine::connect().unwrap(),
		volumes: deploy::Volumes::new(
			apps_root,
			config.snapshots_root.clone(),
			config.logs_root.clone(),
		),
		deploying: tokio::sync::Mutex::new(()),
		github: None,
		notices: std::sync::Mutex::default(),
		released: std::sync::Mutex::default(),
		images: images::Images::default(),
		cron_mount_redeployed_at: std::sync::Mutex::new(None),
		config,
	})
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	let config = config::Config::from_env()?;
	deploy::clear_arrivals(&config.incoming)?;
	let host = Arc::new(Host {
		store: store::Store::open(&config.state)?,
		// Inside every app's network the private API host is the node's own Caddy.
		engine: deploy::Engine::connect()?
			.aliased(&config.caddy.container, &[caddy::private_api_host(&config.caddy).as_str()]),
		volumes: deploy::Volumes::new(
			config.apps_root.clone(),
			config.snapshots_root.clone(),
			config.logs_root.clone(),
		),
		deploying: tokio::sync::Mutex::new(()),
		github: deploy::github::GitHub::from_env(),
		notices: std::sync::Mutex::default(),
		released: std::sync::Mutex::default(),
		images: images::Images::default(),
		cron_mount_redeployed_at: std::sync::Mutex::new(None),
		config,
	});

	// Nothing is left to finish what was running when host last stopped.
	match host.store.sweep() {
		Ok(0) => {}
		Ok(closed) => eprintln!("host: closed {closed} events cut short by the last restart"),
		Err(error) => eprintln!("host: closing events cut short by the last restart: {error}"),
	}

	// What was running keeps running whatever happens here; these only put Caddy back in step with
	// the state. A failure is reported and serving goes on, since host's API is how it is fixed.
	if let Err(error) = rollout::attach(&host).await {
		eprintln!("host: attaching networks: {error}");
	}
	match rollout::route(&host).await {
		Ok(()) => eprintln!("host: Caddy is in step"),
		Err(error) => eprintln!("host: Caddy was not updated: {error}"),
	}
	rollout::tell_cron(&host).await;
	rollout::tell_telemetry(&host).await;

	tokio::spawn(images::run(host.clone()));
	// Runs whose notice never arrived, at start and every 15 minutes. See rollout/catch_up.rs.
	tokio::spawn(rollout::catch_up(host.clone()));
	// Failing exits of an app declaring a restart limit, counted. See restarts.rs.
	tokio::spawn(restarts::watch(host.clone()));

	let telling = host.clone();
	tokio::spawn(async move {
		loop {
			let directory = telling.volumes.data(node::METER);
			if let Err(error) = node::tell(&telling.engine, &directory).await {
				eprintln!("host: telling the meter which container is which: {error}");
			}
			tokio::time::sleep(node::TELLING).await;
		}
	});

	// On the node, only on host's own network, which keeper, a peer and Caddy share, Caddy for its
	// allowlist alone: every app's network host joins to check health leaves its port unreachable
	// from there. See spec/architecture/host.md, "host has no interface on the node, and a door
	// Caddy keeps".
	let mut listen = host.config.listen;
	if std::env::var_os("LISTEN").is_none() {
		let network = deploy::engine::network_of(&host.config.own_container);
		match host.engine.address_on(&host.config.own_container, &network).await {
			Ok(Some(address)) => listen.set_ip(address),
			Ok(None) => eprintln!("host: on no network of its own; answering on every one"),
			Err(error) => eprintln!("host: reading its own address: {error}"),
		}
	}
	let listener = tokio::net::TcpListener::bind(listen).await?;
	eprintln!("host: node `{}`, listening on {listen}", host.config.node);
	axum::serve(listener, api::router(host)).with_graceful_shutdown(stopped()).await?;
	Ok(())
}

/// `docker stop` sends SIGTERM to a process that is PID 1 in its container, which ignores it
/// unless it asks.
async fn stopped() {
	let terminated = async {
		if let Ok(mut signal) =
			tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
		{
			signal.recv().await;
		}
	};
	tokio::select! {
		_ = tokio::signal::ctrl_c() => {}
		() = terminated => {}
	}
}
