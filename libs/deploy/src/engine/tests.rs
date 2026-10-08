use super::report::*;
use super::shape::*;
use super::*;
use bollard::models::{
	ContainerInspectResponse, ContainerSummary, EndpointSettings, HostConfig, NetworkInspect,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[test]
fn shapes_a_container_from_what_list_and_inspect_each_report() {
	use bollard::models::{
		ContainerState, ContainerStateStatusEnum, ContainerSummaryNetworkSettings, MountPoint,
	};
	let summary = ContainerSummary {
		id: Some("abcdef0123456789".into()),
		names: Some(vec!["/geo".into()]),
		image: Some("host/geo:0f939c217676".into()),
		status: Some("Up 3 hours".into()),
		network_settings: Some(ContainerSummaryNetworkSettings {
			networks: Some(HashMap::from([(
				"app-geo".into(),
				EndpointSettings { ip_address: Some("10.0.0.5".into()), ..Default::default() },
			)])),
		}),
		mounts: Some(vec![MountPoint {
			typ: Some("bind".into()),
			source: Some("/data/apps/geo".into()),
			destination: Some("/data".into()),
			rw: Some(false),
			..Default::default()
		}]),
		..Default::default()
	};
	let inspected = ContainerInspectResponse {
		state: Some(ContainerState {
			status: Some(ContainerStateStatusEnum::RUNNING),
			started_at: Some("2026-09-28T00:00:00Z".into()),
			oom_killed: Some(true),
			..Default::default()
		}),
		restart_count: Some(2),
		host_config: Some(HostConfig { memory: Some(512 * 1024 * 1024), ..Default::default() }),
		..Default::default()
	};
	let info = container_info(summary, inspected);
	assert_eq!(info.name, "geo");
	assert_eq!(info.id, "abcdef012345");
	assert_eq!(info.image, "host/geo:0f939c217676");
	assert_eq!(info.state, "running");
	assert_eq!(info.status, "Up 3 hours");
	assert_eq!(info.started_at, Some("2026-09-28T00:00:00Z".into()));
	assert_eq!(info.restart_count, 2);
	assert!(info.oom_killed);
	assert_eq!(info.memory_limit, Some(512 * 1024 * 1024));
	assert_eq!(
		info.networks,
		vec![ContainerNetwork { name: "app-geo".into(), address: Some("10.0.0.5".into()) }]
	);
	assert_eq!(
		info.mounts,
		vec![ContainerMount {
			source: "/data/apps/geo".into(),
			destination: "/data".into(),
			read_only: true,
		}]
	);
}

#[test]
fn socket_service_of_takes_only_a_direct_child_of_the_sockets_directory() {
	assert_eq!(socket_service_of("/sockets/apt"), Some("apt".into()));
	assert_eq!(socket_service_of("/data"), None);
	assert_eq!(socket_service_of("/sockets/"), None);
	assert_eq!(socket_service_of("/sockets/a/b"), None);
}

#[test]
fn shapes_a_network_and_who_is_on_it() {
	use bollard::models::{EndpointResource, Ipam, IpamConfig};
	let network = NetworkInspect {
		name: Some("app-geo".into()),
		driver: Some("bridge".into()),
		ipam: Some(Ipam {
			config: Some(vec![IpamConfig { subnet: Some("172.20.0.0/16".into()), ..Default::default() }]),
			..Default::default()
		}),
		containers: Some(HashMap::from([(
			"endpoint-1".into(),
			EndpointResource {
				name: Some("geo".into()),
				ipv4_address: Some("172.20.0.2/16".into()),
				..Default::default()
			},
		)])),
		..Default::default()
	};
	let info = network_info(network);
	assert_eq!(info.name, "app-geo");
	assert_eq!(info.driver, "bridge");
	assert_eq!(info.subnet, Some("172.20.0.0/16".into()));
	assert_eq!(
		info.members,
		vec![NetworkMember { name: "geo".into(), address: Some("172.20.0.2/16".into()) }]
	);
}

#[test]
fn reads_the_filesystem_a_path_is_under() {
	let root = tempfile::tempdir().unwrap();
	let usage = filesystem_usage(root.path()).unwrap();
	assert!(usage.total > 0);
	assert!(usage.total >= usage.available);
	assert!(filesystem_usage(std::path::Path::new("/no/such/mount")).is_err());
}

#[test]
fn reads_a_numeric_user() {
	assert_eq!(numeric_user("65532:65532"), Some((65532, 65532)));
	assert_eq!(numeric_user("1000"), Some((1000, 1000)));
	assert_eq!(numeric_user("0:0"), None);
	assert_eq!(numeric_user("nobody"), None);
	assert_eq!(numeric_user(""), None);
}

#[test]
fn the_scheduler_shape_mounts_its_own_directory_and_every_socket_service() {
	let own = Some(bind("/data/apps/cron/data".into(), "/state".into(), false));
	let sockets = vec![
		("apt".to_owned(), PathBuf::from("/data/apps/apt/data")),
		("shot".to_owned(), PathBuf::from("/data/apps/shot/data")),
	];
	let mounts = scheduler_mounts(own, &sockets);
	assert_eq!(mounts.len(), 3);
	assert_eq!(
		(mounts[0].source.as_deref(), mounts[0].target.as_deref()),
		(Some("/data/apps/cron/data"), Some("/state"))
	);
	assert_eq!(
		(mounts[1].source.as_deref(), mounts[1].target.as_deref()),
		(Some("/data/apps/apt/data"), Some("/sockets/apt"))
	);
	assert_eq!(
		(mounts[2].source.as_deref(), mounts[2].target.as_deref()),
		(Some("/data/apps/shot/data"), Some("/sockets/shot"))
	);
	assert!(mounts.iter().all(|mount| mount.read_only == Some(false)));
	assert!(scheduler_mounts(None, &[]).is_empty());
}

#[test]
fn the_steward_tries_the_doors_directory_read_only_and_then_the_bus() {
	let [door, bus] = steward_doors();
	assert_eq!(bus.source.as_deref(), Some(DBUS_SOCKET));
	assert_eq!(bus.target.as_deref(), Some(DBUS_SOCKET));
	assert_eq!(bus.read_only, Some(false));
	assert_eq!(door.source.as_deref(), Some("/var/lib/apk-door"));
	assert_eq!(door.target.as_deref(), Some("/door"));
	assert_eq!(door.read_only, Some(true));
	assert!([door, bus].iter().all(|mount| mount.typ == Some(bollard::models::MountType::BIND)));
}

#[test]
fn only_a_missing_bind_source_moves_the_steward_on_to_the_next_door() {
	let refused = |status_code: u16, message: &str| {
		bollard::errors::Error::DockerResponseServerError { status_code, message: message.into() }
	};
	assert!(missing_source(&refused(
		400,
		"invalid mount config for type \"bind\": bind source path does not exist: /run/dbus/system_bus_socket",
	)));
	assert!(!missing_source(&refused(
		400,
		"invalid mount config for type \"bind\": field Target must not be empty"
	)));
	assert!(!missing_source(&refused(
		409,
		"Conflict. The container name \"/apk\" is already in use"
	)));
	assert!(!missing_source(&refused(500, "bind source path does not exist: /door")));
}

#[test]
fn the_reporter_shape_mounts_the_meters_directory_read_only() {
	let own = Some(bind("/data/apps/telemetry/data".into(), "/data".into(), false));
	let mounts = reporter_mounts(own, Path::new("/data/apps/meter/data"));
	assert_eq!(mounts.len(), 2);
	assert_eq!(
		(mounts[1].source.as_deref(), mounts[1].target.as_deref()),
		(Some("/data/apps/meter/data"), Some("/sockets/meter"))
	);
	assert_eq!(mounts[1].read_only, Some(true));
	assert_eq!(reporter_mounts(None, Path::new("/m")).len(), 1);
	let shape = Shape::Reporter { env: vec![], meter: "/m".into() };
	assert!(shape.networked());
}

#[test]
fn the_tunnel_stands_on_edge_at_both_of_its_fixed_addresses() {
	let both = tunnel_endpoint("172.30.0.20", Some("fd34:1053:16bd::20")).ipam_config.unwrap();
	assert_eq!(both.ipv4_address.as_deref(), Some("172.30.0.20"));
	assert_eq!(both.ipv6_address.as_deref(), Some("fd34:1053:16bd::20"));
	// An `edge` without IPv6 is asked for no IPv6 address, which Docker would refuse there.
	let four = tunnel_endpoint("172.30.0.20", None).ipam_config.unwrap();
	assert_eq!((four.ipv4_address.as_deref(), four.ipv6_address), (Some("172.30.0.20"), None));
}

#[test]
fn the_peer_shape_publishes_each_port_on_every_address_at_the_same_number() {
	let published = peer_ports(&[2379, 2380]);
	assert_eq!(published.len(), 2);
	for port in ["2379", "2380"] {
		let bindings = published[&format!("{port}/tcp")].as_deref().unwrap();
		let addresses: Vec<_> = bindings.iter().map(|binding| binding.host_ip.as_deref()).collect();
		assert_eq!(addresses, [Some("0.0.0.0"), Some("::")]);
		assert!(bindings.iter().all(|binding| binding.host_port.as_deref() == Some(port)));
	}
	assert!(Shape::Peer { env: vec![] }.networked());
}

#[test]
fn a_peer_publishes_its_shapes_port_in_place_of_its_declared_one() {
	let geo = include_str!("../../fixtures/geo.toml");
	let shaped = |shape: &str| {
		crate::manifest::Manifest::parse(&format!("{geo}\n[shape]\nkind = \"peer\"\n{shape}\n"))
			.unwrap()
	};
	let relay = shaped("");
	let declared = relay.container.as_ref().and_then(|container| container.port);
	assert!(declared.is_some());
	assert_eq!(peer_published(&relay), Vec::from_iter(declared));
	let database = shaped("port = 5432");
	assert_eq!(peer_published(&database), [5432]);
	let published = peer_ports(&peer_published(&database));
	assert_eq!(published.keys().collect::<Vec<_>>(), ["5432/tcp"]);
	// Several, in place of the declared port, which is published only when it is listed.
	assert_eq!(peer_published(&shaped("ports = [5432, 8008]")), [5432, 8008]);
	let listed = shaped(&format!("ports = [{}, 2380]", declared.unwrap()));
	assert_eq!(peer_published(&listed), [declared.unwrap(), 2380]);
}

#[test]
fn a_container_is_dialed_on_its_own_network_and_no_other() {
	assert_eq!(on_own_network("host", 11011), "host.app-host:11011");
}

#[test]
fn a_container_is_asked_for_as_the_platform_its_app_asks_for() {
	let pinned = create_options("database", Some("linux/arm64"));
	assert_eq!((pinned.name.as_deref(), pinned.platform.as_str()), (Some("database"), "linux/arm64"));
	// Asked for nothing, Docker picks the image's own as it always did.
	let native = create_options("geo", None);
	assert_eq!((native.name.as_deref(), native.platform.as_str()), (Some("geo"), ""));
}

#[test]
fn every_container_is_given_the_stop_grace_whoever_stops_it() {
	assert_eq!(STOP_GRACE.as_secs(), 20);
	let geo = crate::manifest::Manifest::parse(include_str!("../../fixtures/geo.toml")).unwrap();
	let version = Version { manifest: geo, image: "sha256:aa".into() };
	let shapes = [
		Shape::Sandboxed { env: vec![] },
		Shape::Platform { env: vec![] },
		Shape::Peer { env: vec![] },
		Shape::Steward { env: vec![] },
	];
	for shape in shapes {
		let body = container_body(&version, &shape, vec![], HostConfig::default(), &[]);
		assert_eq!(body.stop_timeout, Some(20), "{shape:?}");
	}
	let body =
		container_body(&version, &Shape::Sandboxed { env: vec![] }, vec![], HostConfig::default(), &[]);
	assert_eq!(body.image.as_deref(), Some("sha256:aa"));
	assert_eq!(body.labels.unwrap()["host.app"], "geo");
}

#[test]
fn a_granted_identity_is_the_containers_hostname_and_its_mac_on_its_own_network() {
	let geo = include_str!("../../fixtures/geo.toml");
	let declared = geo.replace(
		"health = \"/health\"",
		"health = \"/health\"\nhostname = \"qq\"\nmac_address = \"02:42:ac:11:00:2a\"",
	);
	let manifest = crate::manifest::Manifest::parse(&declared).unwrap();
	let version = Version { manifest, image: "sha256:aa".into() };
	let body =
		container_body(&version, &Shape::Sandboxed { env: vec![] }, vec![], HostConfig::default(), &[]);
	assert_eq!(body.hostname.as_deref(), Some("qq"));
	let endpoints = body.networking_config.unwrap().endpoints_config.unwrap();
	assert_eq!(endpoints["app-geo"].mac_address.as_deref(), Some("02:42:ac:11:00:2a"));
	// Without one, Docker names and draws them as ever.
	let plain = crate::manifest::Manifest::parse(geo).unwrap();
	let version = Version { manifest: plain, image: "sha256:aa".into() };
	let body =
		container_body(&version, &Shape::Sandboxed { env: vec![] }, vec![], HostConfig::default(), &[]);
	assert_eq!(body.hostname, None);
	assert_eq!(
		body.networking_config.unwrap().endpoints_config.unwrap()["app-geo"].mac_address,
		None
	);
}

#[test]
fn a_container_goes_with_its_anonymous_volumes_and_by_force() {
	let options = removal();
	assert!(options.v && options.force && !options.link);
}

#[test]
fn a_kill_is_an_ask_and_a_die_carries_its_exit_code() {
	use bollard::models::{EventActor, EventMessage};
	let message = |action: &str, attributes: &[(&str, &str)]| EventMessage {
		action: Some(action.into()),
		actor: Some(EventActor {
			id: Some("0123".into()),
			attributes: Some(attributes.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect()),
		}),
		..EventMessage::default()
	};
	let died = message("die", &[("name", "qq"), ("exitCode", "137"), ("image", "host/qq")]);
	assert_eq!(Ending::of(&died), Some(Ending::Died { name: "qq".into(), code: 137 }));
	let asked = message("kill", &[("name", "qq"), ("signal", "15")]);
	assert_eq!(Ending::of(&asked), Some(Ending::Asked("qq".into())));
	assert_eq!(Ending::of(&message("start", &[("name", "qq")])), None);
	assert_eq!(Ending::of(&message("die", &[("name", "qq")])), None);
}
