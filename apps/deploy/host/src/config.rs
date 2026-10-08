//! Everything host is told about the node it runs on. Read from the environment, which on the
//! machine is the `.env` beside host's `docker-compose.yml`: the domains are facts about this
//! node's network, not about the program, so none of them is written into it.

use std::net::SocketAddr;
use std::path::PathBuf;

/// host's own port, after the machine's address ending in .11. See platform's
/// spec/architecture/services.md, "A service keeps one port".
pub const PORT: u16 = 11011;

#[derive(Debug, Clone)]
pub struct Config {
	/// This node's placement name, matched against what a declaration lists.
	pub node: String,
	/// This node's position in infra's `nodes/nodes.toml`, from `NODE_SLOT`, 0 when unset: what a
	/// spread job's runs are moved by. See platform's spec/architecture/cron.md.
	pub slot: u32,
	pub token: String,
	/// A second token, from `HOST_READ_TOKEN`, that reads and never acts; none when it is unset.
	pub read_token: Option<String>,
	pub listen: SocketAddr,
	/// This container's own name, which it attaches to each app's network to check its health.
	pub own_container: String,
	/// Where each app's subvolume lives, and where their snapshots are kept.
	pub apps_root: PathBuf,
	pub snapshots_root: PathBuf,
	pub logs_root: PathBuf,
	pub state: PathBuf,
	/// Where uploaded archives wait while they are loaded.
	pub incoming: PathBuf,
	/// The node's one `.env`, which keeper is started with as host is.
	pub platform_env: PathBuf,
	pub caddy: CaddyConfig,
	pub resolver: ResolverConfig,
	/// The roles the node grants beyond a sandbox, from `GRANTS`; none when it is unset.
	pub grants: crate::grants::Grants,
	/// The architectures the node runs by emulation, from `EMULATE`; none when it is unset. See
	/// spec/architecture/nodes.md, "An x86 node may run arm64 images, emulated, and never the other
	/// way".
	pub emulate: Vec<String>,
	/// The architecture this node runs natively, as an artifact name spells it.
	pub native: Option<&'static str>,
	/// Where the canary is, from `CANARY`: this node, another at its tailnet address, or none.
	pub canary: deploy::canary::Canary,
}

/// `EMULATE` read: architectures separated by whitespace, of those an app may ask for. Anything
/// else is a `.env` this host cannot use.
pub fn emulated(value: &str) -> Result<Vec<String>, String> {
	value
		.split_whitespace()
		.map(|arch| {
			let known = deploy::manifest::ARCHES.contains(&arch);
			if known { Ok(arch.to_owned()) } else { Err(arch.to_owned()) }
		})
		.collect()
}

/// Whether a node of `native` architecture, emulating `emulate`, runs an image built for `arch`:
/// its own, or arm64 emulated on x86 and never the other way.
pub fn runs(native: Option<&str>, emulate: &[String], arch: &str) -> bool {
	native == Some(arch)
		|| (native == Some("amd64") && emulate.iter().any(|emulated| emulated == arch))
}

impl Config {
	/// Whether this node runs an image built for `arch`.
	pub fn runs(&self, arch: &str) -> bool {
		runs(self.native, &self.emulate, arch)
	}
}

/// The house's DNS: where host writes its configuration, the node's LAN address it publishes DNS
/// on, and what it asks, in order. See spec/architecture/host.md, "The resolver serves the house,
/// and answers nothing of its own".
#[derive(Debug, Clone)]
pub struct ResolverConfig {
	/// The Corefile, as this container sees it.
	pub file: PathBuf,
	/// Unset on a node that runs no resolver, which then neither renders nor deploys one.
	pub address: Option<String>,
	/// The router, then the public resolvers, in the order they are asked.
	pub upstreams: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CaddyConfig {
	pub container: String,
	/// host, as Caddy dials it for the door; see `deploy::engine::on_own_network`.
	pub host: String,
	/// The admin socket, as this container sees it.
	pub admin_socket: PathBuf,
	/// The file Caddy starts from, as this container sees it.
	pub config_file: PathBuf,
	/// The admin socket as Caddy sees it, which is what the rendered configuration names.
	pub admin_listen: String,
	pub private_suffix: String,
	pub public_suffix: String,
	/// Sources admitted to the private suffix: the LAN, the tailnet, the machine itself.
	pub private_sources: Vec<String>,
	/// cloudflared's address, the one source admitted to the public suffix.
	pub tunnel_source: String,
	/// Its IPv6 address on `edge`, admitted beside it; none where `edge` has no IPv6.
	pub tunnel_source6: Option<String>,
	pub acme_email: String,
	pub dns_resolver: String,
	/// The public gateway's API host, where the private side sends a scope not deployed here.
	pub public_api: String,
	/// The node's Docker app networks, the only sources the private API admits.
	pub app_sources: Vec<String>,
	/// The private scopes, `public = false`, sent to the public gateway with `INTERNAL_TOKEN`; a
	/// node knows its own declarations alone, so the rest are named here.
	pub private_scopes: Vec<String>,
	/// Whether this node is the canary, whose Caddy answers the others' asks on the tailnet.
	pub canary: bool,
}

impl CaddyConfig {
	/// Every address cloudflared stands at on `edge`, the sources admitted to the public suffix and
	/// believed about a visitor: it reaches Caddy over IPv4 or IPv6, whichever it resolves first.
	pub fn tunnel_sources(&self) -> Vec<String> {
		std::iter::once(self.tunnel_source.clone()).chain(self.tunnel_source6.clone()).collect()
	}
}

#[derive(Debug, thiserror::Error)]
pub enum Missing {
	#[error("`{0}` is not set")]
	Unset(&'static str),
	#[error("`{0}` is set but unreadable: {1}")]
	Unreadable(&'static str, String),
}

fn required(key: &'static str) -> Result<String, Missing> {
	std::env::var(key).ok().filter(|value| !value.is_empty()).ok_or(Missing::Unset(key))
}

fn optional(key: &'static str, default: &str) -> String {
	std::env::var(key).ok().filter(|value| !value.is_empty()).unwrap_or_else(|| default.into())
}

impl Config {
	pub fn from_env() -> Result<Self, Missing> {
		let listen = optional("LISTEN", &format!("0.0.0.0:{PORT}"));
		let listen = listen
			.parse()
			.map_err(|e: std::net::AddrParseError| Missing::Unreadable("LISTEN", e.to_string()))?;
		let apps_root = PathBuf::from(optional("APPS_ROOT", "/data/apps"));
		let caddy_root = apps_root.join("caddy");
		let own_container = optional("OWN_CONTAINER", "host");
		let canary = deploy::canary::Canary::parse(&optional("CANARY", "")).map_err(|value| {
			Missing::Unreadable("CANARY", format!("`{value}` is neither self nor an IPv4"))
		})?;
		let slot = optional("NODE_SLOT", "0")
			.parse()
			.map_err(|e: std::num::ParseIntError| Missing::Unreadable("NODE_SLOT", e.to_string()))?;
		Ok(Self {
			node: required("NODE")?,
			slot,
			token: required("HOST_TOKEN")?,
			read_token: std::env::var("HOST_READ_TOKEN").ok().filter(|value| !value.is_empty()),
			listen,
			own_container: own_container.clone(),
			snapshots_root: PathBuf::from(optional("SNAPSHOTS_ROOT", "/data/.snapshots")),
			logs_root: PathBuf::from(optional("LOGS_ROOT", "/data/logs")),
			state: apps_root.join("host").join("data"),
			incoming: apps_root.join("host").join("data").join("incoming"),
			platform_env: apps_root.join("host").join(".env"),
			caddy: CaddyConfig {
				container: optional("CADDY_CONTAINER", "caddy"),
				host: deploy::engine::on_own_network(&own_container, PORT),
				// In Caddy's own directory, where host checks its health on it as on the meter's.
				admin_socket: caddy_root.join("data").join("admin.sock"),
				config_file: caddy_root.join("host").join("caddy.json"),
				admin_listen: "unix//data/admin.sock".into(),
				private_suffix: required("PRIVATE_SUFFIX")?,
				public_suffix: required("PUBLIC_SUFFIX")?,
				private_sources: required("PRIVATE_SOURCES")?
					.split(',')
					.map(|s| s.trim().to_owned())
					.collect(),
				tunnel_source: required("TUNNEL_SOURCE")?,
				tunnel_source6: std::env::var("TUNNEL_SOURCE6").ok().filter(|value| !value.is_empty()),
				acme_email: required("ACME_EMAIL")?,
				dns_resolver: optional("DNS_RESOLVER", "1.1.1.1"),
				public_api: optional("PUBLIC_API", "api.monoflake.com"),
				app_sources: optional("APP_SOURCES", "172.16.0.0/12")
					.split(',')
					.map(|s| s.trim().to_owned())
					.collect(),
				private_scopes: optional("PRIVATE_SCOPES", "")
					.split_whitespace()
					.map(str::to_owned)
					.collect(),
				canary: canary == deploy::canary::Canary::Itself,
			},
			resolver: ResolverConfig {
				file: apps_root.join("resolver").join("host").join("Corefile"),
				address: std::env::var("LAN_ADDRESS").ok().filter(|value| !value.is_empty()),
				upstreams: optional("RESOLVER_UPSTREAMS", "1.1.1.1 1.0.0.1")
					.split_whitespace()
					.map(str::to_owned)
					.collect(),
			},
			grants: crate::grants::Grants::parse(&optional("GRANTS", ""))
				.map_err(|pair| Missing::Unreadable("GRANTS", format!("`{pair}` is not app:role")))?,
			native: deploy::github::node_arch(),
			canary,
			emulate: emulated(&optional("EMULATE", "")).map_err(|arch| {
				Missing::Unreadable("EMULATE", format!("`{arch}` is not an architecture to emulate"))
			})?,
			apps_root,
		})
	}
}

#[cfg(test)]
mod tests {
	#[test]
	fn an_x86_node_emulates_arm64_when_told_and_nothing_emulates_x86() {
		use super::{emulated, runs};
		let arm64 = emulated("arm64").unwrap();
		assert_eq!(emulated(""), Ok(vec![]));
		assert_eq!(emulated("amd64"), Err("amd64".into()));
		assert_eq!(emulated("arm64 riscv64"), Err("riscv64".into()));
		assert!(runs(Some("amd64"), &arm64, "arm64"));
		assert!(!runs(Some("amd64"), &[], "arm64"));
		assert!(runs(Some("arm64"), &[], "arm64"));
		assert!(runs(Some("amd64"), &[], "amd64"));
		assert!(!runs(Some("arm64"), &["amd64".into()], "amd64"));
		assert!(!runs(None, &arm64, "arm64"));
	}

	#[test]
	fn every_nodes_own_grants_are_pairs_host_takes() {
		let nodes: toml::Table = include_str!("../../../../nodes/nodes.toml").parse().unwrap();
		for (name, node) in nodes {
			let Some(grants) = node.get("grants") else { continue };
			let pairs: Vec<&str> =
				grants.as_array().unwrap().iter().map(|pair| pair.as_str().unwrap()).collect();
			assert!(crate::grants::Grants::parse(&pairs.join(" ")).is_ok(), "{name}: {pairs:?}");
		}
	}

	#[test]
	fn every_node_emulates_only_what_host_takes() {
		let nodes: toml::Table = include_str!("../../../../nodes/nodes.toml").parse().unwrap();
		for (name, node) in nodes {
			let Some(emulate) = node.get("emulate") else { continue };
			let words: Vec<&str> =
				emulate.as_array().unwrap().iter().map(|arch| arch.as_str().unwrap()).collect();
			assert_eq!(super::emulated(&words.join(" ")), Ok(vec!["arm64".to_owned()]), "{name}");
		}
	}

	#[test]
	fn the_port_is_the_one_the_declaration_states() {
		let declaration = include_str!("../service.toml");
		assert!(declaration.lines().any(|line| line.trim() == format!("port = {}", super::PORT)));
	}
}
