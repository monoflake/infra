//! How a container is run, by shape: what each is allowed, and the one `run` that turns a shape
//! into a container. See spec/architecture/host.md, "What a deployment may ask for is host's
//! decision".

use super::{
	DEFAULT_MEMORY_MB, Engine, Error, Manifest, VERSION_LABEL, Version, create_options, network_of,
	stop_timeout,
};
use bollard::models::{
	ContainerCreateBody, EndpointIpamConfig, EndpointSettings, HostConfig, HostConfigLogConfig,
	Mount, MountType, PortBinding, RestartPolicy, RestartPolicyNameEnum,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// How a container is run. Chosen by the program deploying it: infra's own by name, and any other
/// app by the role it asks for and the node grants, never by its declaration alone -- see
/// spec/architecture/host.md, "A role is asked for by the app and granted by the node".
#[derive(Debug, Clone)]
pub enum Shape {
	/// Every app: no capabilities, a read-only root, its own directory and nothing else, and the
	/// environment its two files give it.
	Sandboxed { env: Vec<String> },
	/// host and keeper only: privileged, the Docker socket, the whole of `/data`, and the
	/// environment of the node's one `.env`.
	Platform { env: Vec<String> },
	/// The meter only: sandboxed as any app, but with no network, the machine's PIDs, and the
	/// machine's `/proc` and `/sys` read-only under `/host`. See spec/architecture/meter.md, "Run
	/// beside the machine, not inside it".
	Observer { env: Vec<String> },
	/// Caddy only: the node's one door. Its ports published on the machine, the `edge` network
	/// cloudflared shares, and every app's network joined after it starts; its configuration, which
	/// host writes, read-only; no capability but binding a low port. See
	/// spec/architecture/host.md, "Caddy is deployed like any app, and is the one door".
	Edge { env: Vec<String> },
	/// The tunnel only: sandboxed, on the `edge` network at `address`, and at `address6` where
	/// `edge` has IPv6 -- the addresses Caddy believes a visitor's address from, since cloudflared
	/// reaches it over either. See spec/architecture/host.md, "The tunnel is deployed like any app,
	/// at the address Caddy trusts".
	Tunnel { env: Vec<String>, address: String, address6: Option<String> },
	/// The scheduler, `cron` here: sandboxed like any app, on its own network, plus a bind of each
	/// socket-served service's data directory at `/sockets/<service>`. See platform's
	/// spec/architecture/cron.md, "host gives `cron` the table".
	Scheduler { env: Vec<String>, sockets: Vec<(String, PathBuf)> },
	/// The steward, `apt` or `apk`: sandboxed, its own network, root, and whichever door the machine
	/// has -- see `steward_doors`. See platform's spec/architecture/packages.md, "`apk` reaches the
	/// machine through a named pipe".
	Steward { env: Vec<String> },
	/// The reporter, `telemetry` here: sandboxed like any app, on its own network, plus the meter's
	/// data directory, `meter`, bound read-only at `socket_mount("meter")`. See
	/// platform's spec/architecture/telemetry.md, "Where it comes from".
	Reporter { env: Vec<String>, meter: PathBuf },
	/// The resolver only: sandboxed on its own network, DNS published on the node's LAN `address`
	/// alone, and its configuration, which host writes, read-only. See spec/architecture/host.md,
	/// "The resolver serves the house, and answers nothing of its own".
	Resolver { env: Vec<String>, address: String },
	/// The peer, the platform's relay: sandboxed like any app, `peer_port` published on the
	/// machine at the same number, and joined to host's own network by host. See
	/// spec/architecture/host.md, "A role is asked for by the app and granted by the node".
	Peer { env: Vec<String> },
}

impl Shape {
	/// Whether it runs on the app's own network, which Caddy and host join. The meter has no
	/// network at all, and Caddy stands on the edge and joins the others.
	pub fn networked(&self) -> bool {
		!matches!(self, Shape::Observer { .. } | Shape::Edge { .. } | Shape::Tunnel { .. })
	}
}

/// The network the edge shape stands on, shared with cloudflared.
pub const EDGE_NETWORK: &str = "edge";

/// The tunnel on `edge` at its fixed addresses, the ones Caddy trusts: a Docker-assigned IPv6 would
/// be one Caddy refuses, which every request over IPv6 then is.
pub(super) fn tunnel_endpoint(address: &str, address6: Option<&str>) -> EndpointSettings {
	let fixed = EndpointIpamConfig {
		ipv4_address: Some(address.to_owned()),
		ipv6_address: address6.map(str::to_owned),
		..Default::default()
	};
	EndpointSettings { ipam_config: Some(fixed), ..Default::default() }
}

/// What the edge shape publishes on the machine: HTTP, HTTPS, and HTTPS over QUIC.
pub const EDGE_PORTS: [&str; 3] = ["80/tcp", "443/tcp", "443/udp"];

/// Where the edge shape mounts the parts of its directory beside `data/`: host's rendered
/// configuration, read-only, and Caddy's own configuration state.
pub const EDGE_MOUNTS: [(&str, &str, bool); 2] =
	[("host", "/etc/caddy/host", true), ("config", "/config", false)];

/// The port the resolver answers DNS on inside its container, above the ones a user may not bind,
/// and what it is published as on the node.
pub const RESOLVER_PORTS: [(&str, &str); 2] = [("1053/udp", "53"), ("1053/tcp", "53")];

/// Where the resolver shape mounts host's rendered configuration, read-only.
pub const RESOLVER_MOUNT: (&str, &str) = ("host", "/etc/coredns");

/// What the peer shape publishes each of `ports` as: the same number on every address of the
/// machine, rather than the tailnet's, which is not there yet when Docker starts containers at
/// boot. The firewall drops what does not come over the tailnet; see spec/architecture/nodes.md,
/// "Nothing comes in but over the tailnet".
pub(super) fn peer_ports(ports: &[u16]) -> HashMap<String, Option<Vec<PortBinding>>> {
	let on = |address: &str, port: u16| PortBinding {
		host_ip: Some(address.into()),
		host_port: Some(port.to_string()),
	};
	ports
		.iter()
		.map(|port| (format!("{port}/tcp"), Some(vec![on("0.0.0.0", *port), on("::", *port)])))
		.collect()
}

/// The ports the peer shape publishes: what its `[shape]` names -- `ports`, or `port` -- and its
/// declared port when it names none. See spec/architecture/host.md, "A role is asked for by the
/// app and granted by the node".
pub fn peer_published(manifest: &Manifest) -> Vec<u16> {
	let named = manifest.shape.as_ref().and_then(|shape| shape.published());
	let declared = || manifest.container.as_ref().and_then(|container| container.port);
	named.unwrap_or_else(|| declared().into_iter().collect())
}

/// Where the observer shape puts the machine's two kernel filesystems.
pub const OBSERVED: [(&str, &str); 2] = [("/proc", "/host/proc"), ("/sys", "/host/sys")];

/// Where the scheduler shape mounts a socket-served service's data directory, in `cron`'s own
/// container. See platform's spec/architecture/cron.md, "`reach` is how `cron` asks".
pub fn socket_mount(service: &str) -> String {
	format!("/sockets/{service}")
}

/// The service a mount `destination` was made for, when it sits directly under
/// `socket_mount("")` -- `socket_mount("")` is itself `/sockets/`, so the prefix stripped here is
/// that path as it stands, not with a second slash appended to it. `None` for anything outside
/// that directory, for the directory itself, or for a path with a further slash below it.
pub(super) fn socket_service_of(destination: &str) -> Option<String> {
	let remainder = destination.strip_prefix(&socket_mount(""))?;
	if remainder.is_empty() || remainder.contains('/') {
		return None;
	}
	Some(remainder.to_owned())
}

/// The machine's D-Bus system bus socket, mounted into the steward shape at the same path. See
/// platform's spec/architecture/apt.md, "The door".
pub const DBUS_SOCKET: &str = "/run/dbus/system_bus_socket";

/// The directory of the machine's own door where there is no bus, its named pipe and the state
/// files beside it, and where the steward shape mounts it read-only. See platform's
/// spec/architecture/packages.md, "`apk` reaches the machine through a named pipe".
pub const APK_DOOR: (&str, &str) = ("/var/lib/apk-door", "/door");

/// Each door the steward shape may be given, in the order tried, as a bind mount. host cannot see
/// the machine's paths, so the first Docker accepts as a bind source is the one the machine has.
/// The door's directory goes first: it is there only where `mise run node` put it, while a bus can
/// be on a machine for something else.
pub(super) fn steward_doors() -> [Mount; 2] {
	let (directory, mounted) = APK_DOOR;
	[
		bind(directory.into(), mounted.into(), true),
		bind(DBUS_SOCKET.into(), DBUS_SOCKET.into(), false),
	]
}

/// Docker refusing a bind mount because its source is not on the machine, which it checks before
/// it creates anything.
pub(super) fn missing_source(error: &bollard::errors::Error) -> bool {
	matches!(
		error,
		bollard::errors::Error::DockerResponseServerError { status_code: 400, message }
			if message.contains("bind source path does not exist")
	)
}

/// A structured mount rather than a `source:target` string, which a target containing a colon could
/// extend with options of its own.
pub(crate) fn bind(source: String, target: String, read_only: bool) -> Mount {
	Mount {
		source: Some(source),
		target: Some(target),
		typ: Some(MountType::BIND),
		read_only: Some(read_only),
		..Default::default()
	}
}

/// The scheduler shape's mounts: the app's own directory, if declared, plus each socket-served
/// service's data directory at `/sockets/<service>`. See platform's spec/architecture/cron.md.
pub(super) fn scheduler_mounts(own: Option<Mount>, sockets: &[(String, PathBuf)]) -> Vec<Mount> {
	own
		.into_iter()
		.chain(
			sockets
				.iter()
				.map(|(service, source)| bind(source.display().to_string(), socket_mount(service), false)),
		)
		.collect()
}

/// The reporter shape's mounts: the app's own directory, if declared, plus the meter's data
/// directory at `/sockets/meter`, read-only -- telemetry asks and never changes. See
/// platform's spec/architecture/telemetry.md, "Where it comes from".
pub(super) fn reporter_mounts(own: Option<Mount>, meter: &Path) -> Vec<Mount> {
	own
		.into_iter()
		.chain(std::iter::once(bind(meter.display().to_string(), socket_mount("meter"), true)))
		.collect()
}

/// How every tmpfs a container writes its scratch to is mounted.
pub(crate) const SCRATCH: &str = "rw,noexec,nosuid,size=64m";

/// What every app's container is allowed, on `network` with `mounts` and `memory` bytes: no
/// capabilities, a read-only root, a ceiling with no swap past it, and every line kept -- see
/// spec/architecture/host.md, "What a deployment may ask for is host's decision".
pub(crate) fn sandbox(network: String, mounts: Vec<Mount>, memory: i64) -> HostConfig {
	HostConfig {
		network_mode: Some(network),
		mounts: Some(mounts),
		restart_policy: Some(RestartPolicy {
			name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
			..Default::default()
		}),
		cap_drop: Some(vec!["ALL".into()]),
		security_opt: Some(vec!["no-new-privileges".into()]),
		readonly_rootfs: Some(true),
		tmpfs: Some(HashMap::from([("/tmp".into(), SCRATCH.into())])),
		memory: Some(memory),
		memory_swap: Some(memory),
		pids_limit: Some(512),
		init: Some(true),
		// Not rotated: see spec/architecture/host.md, "Every line an app writes is kept".
		log_config: Some(HostConfigLogConfig { typ: Some("json-file".into()), config: None }),
		..Default::default()
	}
}

/// What Docker is asked to create for `version` in `shape`: its image, environment, labels, ports
/// and networks, and the grace a stop gives it. See spec/architecture/host.md, "What a deployment
/// may ask for is host's decision".
pub(super) fn container_body(
	version: &Version,
	shape: &Shape,
	env: Vec<String>,
	host_config: HostConfig,
	published: &[u16],
) -> ContainerCreateBody {
	let recorded = serde_json::to_string(version).unwrap_or_default();
	ContainerCreateBody {
		image: Some(version.image.clone()),
		stop_timeout: stop_timeout(),
		env: Some(env),
		// The steward runs as root, so systemd's D-Bus API lets it start a unit, and the door's
		// pipe takes its word; see platform's spec/architecture/apt.md, "The door", and
		// packages.md.
		user: matches!(shape, Shape::Steward { .. }).then(|| "0:0".to_owned()),
		labels: Some(HashMap::from([
			("host.app".into(), version.manifest.name.clone()),
			(VERSION_LABEL.into(), recorded),
		])),
		exposed_ports: match shape {
			Shape::Edge { .. } => Some(EDGE_PORTS.iter().map(|port| (*port).to_owned()).collect()),
			Shape::Resolver { .. } => {
				Some(RESOLVER_PORTS.iter().map(|(inside, _)| (*inside).to_owned()).collect())
			}
			Shape::Peer { .. } => Some(published.iter().map(|port| format!("{port}/tcp")).collect()),
			_ => None,
		},
		host_config: Some(host_config),
		networking_config: match shape {
			Shape::Observer { .. } => None,
			Shape::Edge { .. } => Some((EDGE_NETWORK.to_owned(), EndpointSettings::default())),
			Shape::Tunnel { address, address6, .. } => {
				Some((EDGE_NETWORK.to_owned(), tunnel_endpoint(address, address6.as_deref())))
			}
			_ => Some((network_of(&version.manifest.name), EndpointSettings::default())),
		}
		.map(|(network, settings)| bollard::models::NetworkingConfig {
			endpoints_config: Some(HashMap::from([(network, settings)])),
		}),
		..Default::default()
	}
}

impl Engine {
	/// Create and start the app's one container. Everything it is allowed is here, whatever its
	/// declaration says; see spec/architecture/host.md, "What a deployment may ask for is host's
	/// decision".
	pub async fn run(&self, version: &Version, shape: &Shape, data: &Path) -> Result<(), Error> {
		self.run_as(version, shape, data, &version.manifest.name).await
	}

	/// `run`, under `container` rather than the app's own name: a version started beside the one
	/// it replaces. Everything else -- its network, its label, its mounts -- is the app's. See
	/// spec/architecture/host.md, "An app chooses how it is rolled out, and keeping nothing earns a
	/// gapless one".
	pub async fn run_as(
		&self,
		version: &Version,
		shape: &Shape,
		data: &Path,
		container: &str,
	) -> Result<(), Error> {
		let manifest = &version.manifest;
		let name = &manifest.name;
		// A ceiling on every container, and no swap past it: a limit that can be exceeded into swap
		// is a slower machine rather than a limit. See spec/architecture/host.md.
		let declared = manifest.container.as_ref().and_then(|container| container.memory_mb);
		let published = peer_published(manifest);
		let memory = i64::from(declared.unwrap_or(DEFAULT_MEMORY_MB)) * 1024 * 1024;
		let logs = HostConfigLogConfig { typ: Some("json-file".into()), config: None };
		let restart =
			RestartPolicy { name: Some(RestartPolicyNameEnum::UNLESS_STOPPED), ..Default::default() };
		let own = manifest
			.data
			.as_ref()
			.map(|mount| bind(data.display().to_string(), mount.path.clone(), false));
		let sandboxed = |mounts: Vec<Mount>| sandbox(network_of(name), mounts, memory);
		let (host_config, env) = match shape {
			Shape::Sandboxed { env } => (sandboxed(own.into_iter().collect()), env.clone()),
			Shape::Observer { env } => {
				let observed = OBSERVED.iter().map(|(from, to)| bind((*from).into(), (*to).into(), true));
				let config = HostConfig {
					network_mode: Some("none".into()),
					pid_mode: Some("host".into()),
					..sandboxed(own.into_iter().chain(observed).collect())
				};
				(config, env.clone())
			}
			Shape::Edge { env } => {
				let root = data.parent().unwrap_or(data);
				// A bind mount's source has to exist, and only `data/` is made for every app.
				for (from, _, _) in EDGE_MOUNTS {
					let beside = root.join(from);
					tokio::fs::create_dir_all(&beside)
						.await
						.map_err(|source| Error::Directory { path: beside.display().to_string(), source })?;
				}
				let beside = EDGE_MOUNTS.iter().map(|(from, to, read_only)| {
					bind(root.join(from).display().to_string(), (*to).into(), *read_only)
				});
				let published = EDGE_PORTS.iter().map(|port| {
					let host_port = port.split('/').next().map(str::to_owned);
					(port.to_string(), Some(vec![PortBinding { host_ip: None, host_port }]))
				});
				let config = HostConfig {
					network_mode: Some(EDGE_NETWORK.into()),
					port_bindings: Some(published.collect()),
					cap_add: Some(vec!["NET_BIND_SERVICE".into()]),
					..sandboxed(own.into_iter().chain(beside).collect())
				};
				(config, env.clone())
			}
			Shape::Tunnel { env, .. } => {
				let config = HostConfig {
					network_mode: Some(EDGE_NETWORK.into()),
					..sandboxed(own.into_iter().collect())
				};
				(config, env.clone())
			}
			Shape::Scheduler { env, sockets } => (sandboxed(scheduler_mounts(own, sockets)), env.clone()),
			// Its door is added as it is created, below.
			Shape::Steward { env } => (sandboxed(own.into_iter().collect()), env.clone()),
			Shape::Reporter { env, meter } => (sandboxed(reporter_mounts(own, meter)), env.clone()),
			Shape::Peer { env } => {
				let config = HostConfig {
					port_bindings: (!published.is_empty()).then(|| peer_ports(&published)),
					..sandboxed(own.into_iter().collect())
				};
				(config, env.clone())
			}
			Shape::Resolver { env, address } => {
				let (from, to) = RESOLVER_MOUNT;
				let beside = data.parent().unwrap_or(data).join(from);
				tokio::fs::create_dir_all(&beside)
					.await
					.map_err(|source| Error::Directory { path: beside.display().to_string(), source })?;
				let configured = bind(beside.display().to_string(), to.into(), true);
				let published = RESOLVER_PORTS.iter().map(|(inside, outside)| {
					let binding =
						PortBinding { host_ip: Some(address.clone()), host_port: Some((*outside).into()) };
					((*inside).to_owned(), Some(vec![binding]))
				});
				let config = HostConfig {
					port_bindings: Some(published.collect()),
					..sandboxed(own.into_iter().chain(std::iter::once(configured)).collect())
				};
				(config, env.clone())
			}
			Shape::Platform { env } => {
				let config = HostConfig {
					network_mode: Some(network_of(name)),
					binds: Some(vec![
						"/var/run/docker.sock:/var/run/docker.sock".into(),
						"/data:/data".into(),
					]),
					restart_policy: Some(restart),
					memory: Some(memory),
					memory_swap: Some(memory),
					privileged: Some(true),
					init: Some(true),
					log_config: Some(logs),
					..Default::default()
				};
				(config, env.clone())
			}
		};
		let body = container_body(version, shape, env, host_config, &published);
		let platform = manifest.platform();
		match shape {
			Shape::Steward { .. } => self.create_steward(container, platform.as_deref(), body).await?,
			_ => self.create(container, platform.as_deref(), body).await?,
		}
		self.docker.start_container(container, None).await?;
		Ok(())
	}

	async fn create(
		&self,
		name: &str,
		platform: Option<&str>,
		body: ContainerCreateBody,
	) -> Result<(), Error> {
		self.docker.create_container(Some(create_options(name, platform)), body).await?;
		Ok(())
	}

	/// Create the steward with the first of `steward_doors` the machine has, which Docker answers by
	/// refusing a bind whose source is missing before it creates anything; with neither, refuse.
	/// See platform's spec/architecture/packages.md, "`apk` reaches the machine through a named
	/// pipe".
	async fn create_steward(
		&self,
		name: &str,
		platform: Option<&str>,
		body: ContainerCreateBody,
	) -> Result<(), Error> {
		for door in steward_doors() {
			let mut body = body.clone();
			let config = body.host_config.get_or_insert_with(HostConfig::default);
			config.mounts.get_or_insert_with(Vec::new).push(door);
			match self.create(name, platform, body).await {
				Err(Error::Docker(error)) if missing_source(&error) => continue,
				done => return done,
			}
		}
		Err(Error::NoDoor)
	}
}
