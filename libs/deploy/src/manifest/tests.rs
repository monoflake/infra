use super::*;

/// The file geo ships. Two programs read this format, so the reader is tested against a real
/// declaration rather than one written to suit it.
const GEO: &str = include_str!("../../fixtures/geo.toml");

#[test]
fn reads_the_declaration_geo_ships() {
	let manifest = Manifest::parse(GEO).unwrap();
	assert_eq!(manifest.name, "geo");
	assert_eq!(manifest.data, None);
	let limits = vec![
		Limit {
			methods: vec!["GET".into(), "HEAD".into()],
			path: "/address".into(),
			count: 60,
			seconds: 60,
			burst: None,
			subject: None,
		},
		Limit {
			methods: vec!["GET".into(), "HEAD".into()],
			path: "/ip".into(),
			count: 60,
			seconds: 60,
			burst: None,
			subject: None,
		},
	];
	assert_eq!(manifest.api, Some(Api { public: true, prefix: None, limits, sides: None }));
	assert_eq!(manifest.check("geo", "rdu"), Ok(()));
}

#[test]
fn an_unknown_version_is_refused_before_anything_else_is_read() {
	let newer = GEO.replace("version = 1", "version = 2").replace("port =", "port_moved =");
	assert_eq!(Manifest::parse(&newer), Err(Invalid::Version(2)));
}

#[test]
fn an_unknown_key_is_ignored() {
	// A key a newer declaration adds is compatible by definition; refusing it would turn every
	// addition into an outage on the hosts not yet updated. See spec/json.md.
	assert!(Manifest::parse(&format!("{GEO}\nlater = true\n")).is_ok());
}

#[test]
fn names_are_labels_and_not_the_platforms() {
	assert!(check_name("geo").is_ok());
	assert!(check_name("ip-lookup2").is_ok());
	assert_eq!(check_name("Geo"), Err(Invalid::Name("Geo".into())));
	assert_eq!(check_name("-geo"), Err(Invalid::Name("-geo".into())));
	assert_eq!(check_name("geo_ip"), Err(Invalid::Name("geo_ip".into())));
	assert_eq!(check_name("api"), Err(Invalid::Reserved("api".into())));
	assert_eq!(check_name("caddy"), Err(Invalid::Reserved("caddy".into())));
	assert_eq!(check_name("infra"), Err(Invalid::Reserved("infra".into())));
}

#[test]
fn a_node_runs_only_what_is_placed_on_it() {
	let manifest = Manifest::parse(GEO).unwrap();
	assert_eq!(
		manifest.check("geo", "vps"),
		Err(Invalid::NotPlaced { name: "geo".into(), node: "vps".into() })
	);
	assert!(matches!(manifest.check("other", "rdu"), Err(Invalid::Mismatch { .. })));
}

#[test]
fn only_the_platforms_own_pass_its_own_check() {
	let mut manifest = Manifest::parse(GEO).unwrap();
	manifest.name = "keeper".into();
	assert_eq!(manifest.check_own("keeper", "rdu"), Ok(()));
	// The ordinary check still refuses the name, so no app can be sent as keeper.
	assert_eq!(manifest.check("keeper", "rdu"), Err(Invalid::Reserved("keeper".into())));
	assert_eq!(check_name("meter"), Err(Invalid::Reserved("meter".into())));
	manifest.name = "api".into();
	assert_eq!(manifest.check_own("api", "rdu"), Err(Invalid::Name("api".into())));
}

#[test]
fn a_default_port_is_refused() {
	let mut manifest = Manifest::parse(GEO).unwrap();
	manifest.container.as_mut().unwrap().port = Some(8080);
	assert_eq!(manifest.check("geo", "rdu"), Err(Invalid::Port(8080)));
}

#[test]
fn a_container_answers_on_a_port_or_a_socket() {
	let socketed = |extra: &str| {
		Manifest::parse(&format!(
			"version = 1\nname = \"probe\"\nplacements = [\"rdu\"]\n{extra}\n[container]\nhealth = \"/health\"\nsocket = \"probe.sock\"\n[data]\npath = \"/data\"\n",
		))
		.unwrap()
	};
	assert_eq!(socketed("").data, Some(Data { path: "/data".into() }));
	assert_eq!(socketed("").check("probe", "rdu"), Ok(()));
	let mut both = socketed("");
	both.container.as_mut().unwrap().port = Some(20000);
	assert_eq!(both.check("probe", "rdu"), Err(Invalid::Answer));
	let mut neither = socketed("");
	neither.container.as_mut().unwrap().socket = None;
	assert_eq!(neither.check("probe", "rdu"), Err(Invalid::Answer));
	let mut nested = socketed("");
	nested.container.as_mut().unwrap().socket = Some("../probe.sock".into());
	assert_eq!(nested.check("probe", "rdu"), Err(Invalid::Socket));
	let mut homeless = socketed("");
	homeless.data = None;
	assert_eq!(homeless.check("probe", "rdu"), Err(Invalid::Socket));
	let routed = socketed("[api]\npublic = false");
	assert_eq!(routed.check("probe", "rdu"), Err(Invalid::Unroutable));
}

#[test]
fn a_node_refuses_a_service_with_no_container() {
	let manifest =
		Manifest::parse("version = 1\nname = \"edge\"\nplacements = [\"workers\", \"rdu\"]\n").unwrap();
	assert_eq!(manifest.check("edge", "rdu"), Err(Invalid::NoContainer("edge".into())));
}

#[test]
fn every_declaration_in_the_repository_is_one_this_reader_takes() {
	// This repository's own apps, and the platform's as fixtures/ keeps a copy of them: the
	// platform's repository is another one, so its declarations are read from the copy.
	let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
	let own =
		std::fs::read_dir(root.join("apps")).unwrap().map(|e| e.unwrap().path().join("service.toml"));
	let fixtures =
		std::fs::read_dir(root.join("libs/deploy/fixtures")).unwrap().map(|e| e.unwrap().path());
	let mut read = 0;
	for path in own.chain(fixtures) {
		let Ok(text) = std::fs::read_to_string(&path) else { continue };
		let manifest = Manifest::parse(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
		let node = manifest.placements.iter().find(|placement| *placement != WORKERS);
		if let Some(node) = node {
			let checked = if OWN.contains(&manifest.name.as_str()) {
				manifest.check_own(&manifest.name, node)
			} else {
				manifest.check(&manifest.name, node)
			};
			assert_eq!(checked, Ok(()), "{}", path.display());
		}
		read += 1;
	}
	assert!(read >= 3);
}

#[test]
fn counts_a_prefix_beside_an_exact_path_it_does_not_cover() {
	let rows = "[[api.limits]]\nmethods = [\"GET\"]\npath = \"/checks\"\ncount = 1\nseconds = 60\n\
		[[api.limits]]\nmethods = [\"GET\"]\npath = \"/checks/*\"\ncount = 1\nseconds = 60";
	let manifest = Manifest::parse(&format!("{GEO}\n{rows}\n")).unwrap();
	assert_eq!(manifest.check("geo", "rdu"), Ok(()));
}

#[test]
fn refuses_a_limit_it_could_not_count() {
	for broken in [
		"methods = []\npath = \"/a\"\ncount = 1\nseconds = 60",
		"methods = [\"get\"]\npath = \"/a\"\ncount = 1\nseconds = 60",
		"methods = [\"GET\"]\npath = \"a\"\ncount = 1\nseconds = 60",
		"methods = [\"GET\"]\npath = \"/a\"\ncount = 0\nseconds = 60",
		"methods = [\"GET\"]\npath = \"/a\"\ncount = 1\nseconds = 0",
		"methods = [\"GET\"]\npath = \"/a\"\ncount = 1\nseconds = 86401",
		"methods = [\"GET\"]\npath = \"/a/*/b\"\ncount = 1\nseconds = 60",
		"methods = [\"GET\"]\npath = \"/a\"\ncount = 1\nseconds = 60\nburst = 0",
		// Only an address is counted until there are accounts.
		"methods = [\"GET\"]\npath = \"/a\"\ncount = 1\nseconds = 60\nsubject = \"account\"",
		// geo's own row covers GET /address, and a prefix over it would count it twice.
		"methods = [\"GET\"]\npath = \"/*\"\ncount = 1\nseconds = 60",
		// geo's own row covers GET /address already.
		"methods = [\"GET\"]\npath = \"/address\"\ncount = 1\nseconds = 60",
	] {
		let text = format!("{GEO}\n[[api.limits]]\n{broken}\n");
		let manifest = Manifest::parse(&text).unwrap();
		assert_eq!(manifest.check("geo", "rdu"), Err(Invalid::Limit), "{broken}");
	}
}

#[test]
fn names_its_sides_from_the_two_there_are() {
	let mut geo = Manifest::parse(GEO).unwrap();
	let api = geo.api.as_mut().unwrap();
	assert!(api.carried_on("private") && api.carried_on("tunnel"));
	api.sides = Some(vec!["tunnel".into()]);
	assert!(!api.carried_on("private") && api.carried_on("tunnel"));
	assert_eq!(geo.check("geo", "rdu"), Ok(()));
	for sides in [vec!["inside"], vec!["private", "outside"], vec![]] {
		let mut broken = geo.clone();
		broken.api.as_mut().unwrap().sides = Some(sides.iter().map(|side| (*side).into()).collect());
		assert_eq!(broken.check("geo", "rdu"), Err(Invalid::Sides), "{sides:?}");
	}
}

#[test]
fn a_home_stays_on_the_apps_own_site() {
	let gemini = Manifest::parse(include_str!("../../fixtures/gemini.toml")).unwrap();
	assert_eq!(gemini.check("gemini", "rdu"), Ok(()));
	for home in ["/", "admin", "//evil.example"] {
		let mut elsewhere = gemini.clone();
		elsewhere.interface.as_mut().unwrap().home = Some(home.into());
		assert_eq!(elsewhere.check("gemini", "rdu"), Err(Invalid::Home), "{home}");
	}
	let tunnel =
		Manifest::parse(include_str!("../../../../apps/network/tunnel/service.toml")).unwrap();
	assert_eq!(tunnel.check_own("tunnel", "rdu"), Ok(()));
}

#[test]
fn a_node_refuses_an_api_prefix() {
	let mut manifest = Manifest::parse(GEO).unwrap();
	manifest.api.as_mut().unwrap().prefix = Some("/api".into());
	assert_eq!(manifest.check("geo", "rdu"), Err(Invalid::Prefix));
}

#[test]
fn a_label_maps_an_apps_name_for_what_reaches_it_from_outside() {
	let gemini = Manifest::parse(include_str!("../../fixtures/gemini.toml")).unwrap();
	let interface = gemini.interface.as_ref().unwrap();
	// No `domain` set: the label is the app's own name.
	assert_eq!(interface.label("gemini"), "gemini");
	let mut labeled = gemini.clone();
	labeled.interface.as_mut().unwrap().domain = Some("ai".into());
	assert_eq!(labeled.interface.as_ref().unwrap().label("gemini"), "ai");
	assert_eq!(labeled.check("gemini", "rdu"), Ok(()));
}

#[test]
fn a_reserved_label_is_refused() {
	let mut manifest = Manifest::parse(GEO).unwrap();
	manifest.interface = Some(Interface { domain: Some("cms".into()), lan: true, home: None });
	assert_eq!(manifest.check("geo", "rdu"), Err(Invalid::Reserved("cms".into())));
	// host's door: the panel answered on it, and Caddy now routes it to host itself.
	manifest.interface.as_mut().unwrap().domain = Some("infra".into());
	assert_eq!(manifest.check("geo", "rdu"), Err(Invalid::Reserved("infra".into())));
	manifest.interface.as_mut().unwrap().domain = Some("host".into());
	assert_eq!(manifest.check("geo", "rdu"), Err(Invalid::Reserved("host".into())));
	manifest.interface.as_mut().unwrap().domain = Some("Geo".into());
	assert_eq!(manifest.check("geo", "rdu"), Err(Invalid::Label("Geo".into())));
}

#[test]
fn an_old_manifests_interface_public_key_is_ignored() {
	// A manifest a store already holds from before this key was replaced with `domain` and
	// `lan` still has to deserialize; see spec/json.md and the note on `Interface` below.
	let text = format!("{GEO}\n[interface]\npublic = false\n");
	let manifest = Manifest::parse(&text).unwrap();
	let interface = manifest.interface.unwrap();
	assert_eq!((interface.domain, interface.lan), (None, true));
}

#[test]
fn objects_are_declared_as_buckets_s3_takes() {
	let declared =
		|buckets: &str| Manifest::parse(&format!("{GEO}\n[objects]\nbuckets = [{buckets}]\n")).unwrap();
	let manifest = declared("\"photos\", \"thumbs.v2\"");
	assert_eq!(manifest.check("geo", "rdu"), Ok(()));
	assert_eq!(manifest.sidecar().as_deref(), Some("geo-objects"));
	assert_eq!(Manifest::parse(GEO).unwrap().sidecar(), None);
	for broken in [
		"Photos",
		"ph",
		"-photos",
		"photos-",
		"ph..otos",
		"192.168.1.1",
		"xn--photos",
		"photos-s3alias",
		"ph_otos",
	] {
		let manifest = declared(&format!("\"{broken}\""));
		assert_eq!(manifest.check("geo", "rdu"), Err(Invalid::Bucket(broken.into())), "{broken}");
	}
	assert_eq!(declared("").check("geo", "rdu"), Err(Invalid::Buckets));
	assert_eq!(declared("\"photos\", \"photos\"").check("geo", "rdu"), Err(Invalid::Buckets));
	let mut long = declared("\"photos\"");
	long.name = "a".repeat(56);
	let named = Invalid::SidecarName(format!("{}-objects", "a".repeat(56)));
	assert_eq!(long.check(&"a".repeat(56), "rdu"), Err(named));
	long.name = "a".repeat(55);
	assert_eq!(long.check(&"a".repeat(55), "rdu"), Ok(()));
}

#[test]
fn every_sidecar_name_is_reserved_and_the_driver_is_named_like_any_app() {
	assert_eq!(check_name("geo-objects"), Err(Invalid::Reserved("geo-objects".into())));
	assert!(check_name("objects-geo").is_ok());
	// A driver is what its declaration offers and the node grants, not what it is called.
	assert!(check_name("objects").is_ok());
	let driver = Manifest::parse(include_str!("../../fixtures/objects.toml")).unwrap();
	assert_eq!(driver.driver.as_ref().map(|driver| driver.provides.as_str()), Some("objects"));
	assert_eq!(driver.check("objects", "rdu"), Ok(()));
}

#[test]
fn databases_are_declared_each_with_an_optional_ceiling() {
	let text = format!("{GEO}\n[postgres]\nmemory_mb = 192\n");
	let manifest = Manifest::parse(&text).unwrap();
	assert_eq!(manifest.postgres, Some(Database { memory_mb: Some(192) }));
	assert_eq!(manifest.check("geo", "rdu"), Ok(()));
	assert_eq!(manifest.sidecars(), ["geo-postgres"]);
	assert_eq!(Driver::Postgres.memory_mb(&manifest), Some(192));
	// Objects are not declared, so its sidecar is not among them, and the one it names is none.
	assert_eq!(manifest.sidecar(), None);
	let geo = Manifest::parse(GEO).unwrap();
	assert!(geo.sidecars().is_empty() && geo.postgres.is_none());

	let unbounded = Manifest::parse(&format!("{GEO}\n[postgres]\nmemory_mb = 0\n")).unwrap();
	assert_eq!(unbounded.check("geo", "rdu"), Err(Invalid::SidecarMemory("postgres".into())));
	let mut long = Manifest::parse(&format!("{GEO}\n[postgres]\n")).unwrap();
	long.name = "a".repeat(55);
	let named = Invalid::SidecarName(format!("{}-postgres", "a".repeat(55)));
	assert_eq!(long.check(&"a".repeat(55), "rdu"), Err(named));
	long.name = "a".repeat(54);
	assert_eq!(long.check(&"a".repeat(54), "rdu"), Ok(()));
}

#[test]
fn every_database_sidecar_name_is_reserved_and_only_a_driver_keeps_its_own_port() {
	assert_eq!(check_name("geo-postgres"), Err(Invalid::Reserved("geo-postgres".into())));
	assert!(check_name("postgres-geo").is_ok());
	let driver = Manifest::parse(include_str!("../../fixtures/postgres.toml")).unwrap();
	assert_eq!(driver.check("postgres", "rdu"), Ok(()));
	// Only a driver keeps a port outside the services' range; an app on 5432 is still refused.
	let mut geo = Manifest::parse(GEO).unwrap();
	geo.container.as_mut().unwrap().port = Some(5432);
	assert_eq!(geo.check("geo", "rdu"), Err(Invalid::Port(5432)));
}

#[test]
fn lan_defaults_to_on_and_can_be_turned_off() {
	let gemini = Manifest::parse(include_str!("../../fixtures/gemini.toml")).unwrap();
	assert!(gemini.interface.as_ref().unwrap().lan);
	let mut off = gemini;
	off.interface.as_mut().unwrap().lan = false;
	assert_eq!(off.check("gemini", "rdu"), Ok(()));
}

#[test]
fn a_role_or_a_driver_is_asked_for_by_a_word_the_node_knows() {
	// Infra's names are its own; a service above it is named like any app and asks for a role.
	assert!(check_name("cron").is_ok() && check_name("apt").is_ok());
	assert!(OWN.contains(&"keeper") && !OWN.contains(&"cron"));
	let cron = Manifest::parse(include_str!("../../fixtures/cron.toml"));
	assert_eq!(cron.unwrap().shape.map(|shape| shape.kind), Some("scheduler".into()));
	let peer = Manifest::parse(&format!("{GEO}\n[shape]\nkind = \"peer\"\n")).unwrap();
	assert_eq!(peer.check("geo", "rdu"), Ok(()));
	let root = Manifest::parse(&format!("{GEO}\n[shape]\nkind = \"root\"\n")).unwrap();
	assert_eq!(root.check("geo", "rdu"), Err(Invalid::Shape("root".into())));
	let redis = Manifest::parse(&format!("{GEO}\n[driver]\nprovides = \"redis\"\n")).unwrap();
	assert_eq!(redis.check("geo", "rdu"), Err(Invalid::Driver("redis".into())));
}

#[test]
fn a_peer_alone_names_a_port_of_its_own_to_publish() {
	let shaped = |shape: &str| Manifest::parse(&format!("{GEO}\n[shape]\n{shape}\n")).unwrap();
	let relay = shaped("kind = \"peer\"");
	assert_eq!(relay.shape.as_ref().and_then(|shape| shape.port), None);
	let written: toml::Table = toml::to_string(&relay).unwrap().parse().unwrap();
	assert!(!written["shape"].as_table().unwrap().contains_key("port"));
	let database = shaped("kind = \"peer\"\nport = 5432");
	assert_eq!(database.shape.as_ref().and_then(|shape| shape.port), Some(5432));
	assert_eq!(database.check("geo", "rdu"), Ok(()));
	let back = Manifest::parse(&toml::to_string(&database).unwrap()).unwrap();
	assert_eq!(back.shape, database.shape);
	let cron = shaped("kind = \"scheduler\"\nport = 5432");
	assert_eq!(cron.check("geo", "rdu"), Err(Invalid::ShapePort("scheduler".into())));
	let declared = Manifest::parse(GEO).unwrap().container.unwrap().port.unwrap();
	let own = shaped(&format!("kind = \"peer\"\nport = {declared}"));
	assert_eq!(own.check("geo", "rdu"), Err(Invalid::PeerPort(declared)));
	assert_eq!(shaped("kind = \"peer\"\nport = 0").check("geo", "rdu"), Err(Invalid::PeerPort(0)));
}

#[test]
fn a_schedule_declares_exactly_one_clock() {
	let text = format!(
		"{GEO}\n[[schedules]]\nname = \"refresh\"\ncron = \"0 4 * * *\"\nevery = \"1m\"\npath = \"/jobs/refresh\"\n"
	);
	let manifest = Manifest::parse(&text).unwrap();
	assert_eq!(manifest.check("geo", "rdu"), Err(Invalid::Schedule("refresh".into())));
}

#[test]
fn a_schedule_reads_its_defaults_and_checks_its_shape() {
	let scheduled = |extra: &str| {
		Manifest::parse(&format!(
			"{GEO}\n[[schedules]]\nname = \"refresh\"\npath = \"/jobs/refresh\"\n{extra}\n"
		))
		.unwrap()
	};
	let manifest = scheduled("cron = \"0 4 * * *\"");
	let schedule = &manifest.schedules[0];
	assert_eq!(schedule.catch_up, CatchUp::Once);
	assert_eq!(schedule.overlap, Overlap::Skip);
	assert_eq!(schedule.timeout, 300);
	assert_eq!(manifest.check("geo", "rdu"), Ok(()));

	assert_eq!(scheduled("every = \"30s\"").check("geo", "rdu"), Ok(()));
	for broken in ["cron = \"0 4 * *\"", "cron = \"a 4 * * *\"", "every = \"m\"", "every = \"0s\""] {
		assert_eq!(
			scheduled(broken).check("geo", "rdu"),
			Err(Invalid::Schedule("refresh".into())),
			"{broken}"
		);
	}
	let no_slash = Manifest::parse(&format!(
		"{GEO}\n[[schedules]]\nname = \"refresh\"\ncron = \"0 4 * * *\"\npath = \"jobs/refresh\"\n"
	))
	.unwrap();
	assert_eq!(no_slash.check("geo", "rdu"), Err(Invalid::Schedule("refresh".into())));
	let too_long = scheduled("cron = \"0 4 * * *\"\ntimeout = 86401");
	assert_eq!(too_long.check("geo", "rdu"), Err(Invalid::Schedule("refresh".into())));
}

#[test]
fn a_display_name_is_short_trimmed_free_text() {
	let shown = |value: &str| {
		Manifest::parse(&GEO.replace("name = \"geo\"\n", &format!("name = \"geo\"\n{value}\n")))
			.unwrap()
	};
	assert_eq!(shown("").display_name, None);
	let geo = shown("display_name = \"Geolocation\"");
	assert_eq!(geo.display_name.as_deref(), Some("Geolocation"));
	assert_eq!(geo.check("geo", "rdu"), Ok(()));
	let back = toml::to_string(&geo).unwrap();
	assert_eq!(Manifest::parse(&back).unwrap().display_name.as_deref(), Some("Geolocation"));
	let longest = "É".repeat(DISPLAY_NAME_LENGTH);
	assert_eq!(shown(&format!("display_name = \"{longest}\"")).check("geo", "rdu"), Ok(()));
	let too_long = "a".repeat(DISPLAY_NAME_LENGTH + 1);
	for bad in ["", " ", " Geo", "Geo ", too_long.as_str()] {
		assert_eq!(
			shown(&format!("display_name = \"{bad}\"")).check("geo", "rdu"),
			Err(Invalid::DisplayName(bad.into())),
			"{bad:?}"
		);
	}
}

#[test]
fn a_schedule_spreads_by_the_week_or_not_at_all() {
	let spread = |value: &str| {
		Manifest::parse(&format!(
			"{GEO}\n[[schedules]]\nname = \"upgrade\"\ncron = \"0 8 * * 0\"\npath = \"/jobs/upgrade\"\n{value}\n"
		))
	};
	assert_eq!(spread("").unwrap().schedules[0].spread, None);
	let weekly = spread("spread = \"week\"").unwrap();
	assert_eq!(weekly.schedules[0].spread, Some(Spread::Week));
	assert_eq!(weekly.check("geo", "rdu"), Ok(()));
	for other in ["spread = \"day\"", "spread = \"\"", "spread = 7"] {
		assert!(matches!(spread(other), Err(Invalid::Malformed(_))), "{other}");
	}
}

#[test]
fn a_scheduled_service_answers_through_its_scope_or_its_socket() {
	let socketed = Manifest::parse(
		"version = 1\nname = \"probe\"\nplacements = [\"rdu\"]\n[container]\nhealth = \"/health\"\nsocket = \"probe.sock\"\n[data]\npath = \"/data\"\n[[schedules]]\nname = \"update\"\ncron = \"0 7 * * *\"\npath = \"/jobs/update\"\n",
	)
	.unwrap();
	assert_eq!(socketed.check("probe", "rdu"), Ok(()));

	let unreachable = Manifest::parse(
		"version = 1\nname = \"probe\"\nplacements = [\"rdu\"]\n[container]\nhealth = \"/health\"\nport = 20000\n[[schedules]]\nname = \"update\"\ncron = \"0 7 * * *\"\npath = \"/jobs/update\"\n",
	)
	.unwrap();
	assert_eq!(unreachable.check("probe", "rdu"), Err(Invalid::Unscheduled("probe".into())));
}

/// geo's declaration with `rollout` set to `rollout`, and `extra` after it.
fn rolled(rollout: &str, extra: &str) -> Manifest {
	let declared = GEO.replacen("version = 1", &format!("version = 1\nrollout = \"{rollout}\""), 1);
	Manifest::parse(&format!("{declared}\n{extra}\n")).unwrap()
}

#[test]
fn a_rollout_is_replace_unless_it_says_otherwise_and_left_out_when_it_is() {
	let geo = Manifest::parse(GEO).unwrap();
	assert_eq!(geo.rollout, Rollout::Replace);
	let written: toml::Table = toml::to_string(&geo).unwrap().parse().unwrap();
	assert!(!written.contains_key("rollout"));
	assert_eq!(rolled("replace", "").rollout, Rollout::Replace);
	for (word, rollout) in [("beside", Rollout::Beside), ("manual", Rollout::Manual)] {
		let manifest = rolled(word, "");
		assert_eq!((manifest.rollout, manifest.rollout.word()), (rollout, word));
		assert_eq!(manifest.check("geo", "rdu"), Ok(()));
		let back = Manifest::parse(&toml::to_string(&manifest).unwrap()).unwrap();
		assert_eq!(back.rollout, rollout);
	}
	assert!(matches!(
		Manifest::parse(&GEO.replacen("version = 1", "version = 1\nrollout = \"canary\"", 1)),
		Err(Invalid::Malformed(_))
	));
}

#[test]
fn beside_is_refused_to_anything_two_containers_could_not_hold_at_once() {
	let beside = |extra: &str| rolled("beside", extra).check("geo", "rdu");
	assert_eq!(beside("[data]\npath = \"/data\""), Err(Invalid::BesideData));
	assert_eq!(
		beside("[objects]\nbuckets = [\"photos\"]"),
		Err(Invalid::BesideSidecar("objects".into()))
	);
	assert_eq!(beside("[postgres]"), Err(Invalid::BesideSidecar("postgres".into())));
	for role in ["peer", "scheduler", "steward", "reporter"] {
		let asked = format!("[shape]\nkind = \"{role}\"");
		assert_eq!(beside(&asked), Err(Invalid::BesideRole(role.into())), "{role}");
	}
	let driver = beside("[driver]\nprovides = \"objects\"");
	assert_eq!(driver, Err(Invalid::BesideRole("the objects driver".into())));
	// Replace takes every one of them as before.
	assert_eq!(rolled("replace", "[shape]\nkind = \"peer\"").check("geo", "rdu"), Ok(()));
	assert_eq!(rolled("manual", "[data]\npath = \"/data\"").check("geo", "rdu"), Ok(()));
}

#[test]
fn beside_is_refused_to_an_app_on_a_socket_and_to_infras_own() {
	let mut socketed = rolled("beside", "[data]\npath = \"/data\"");
	let container = socketed.container.as_mut().unwrap();
	(container.port, container.socket) = (None, Some("geo.sock".into()));
	socketed.api = None;
	assert_eq!(socketed.check("geo", "rdu"), Err(Invalid::BesideSocket));
	for own in ["caddy", "resolver", "tunnel", "keeper"] {
		let mut manifest = rolled("beside", "");
		manifest.name = own.into();
		assert_eq!(manifest.check_own(own, "rdu"), Err(Invalid::BesideOwn(own.into())), "{own}");
	}
}

#[test]
fn beside_needs_room_in_a_name_for_the_version_beside_it() {
	let mut long = rolled("beside", "");
	long.name = "a".repeat(59);
	assert_eq!(long.check(&"a".repeat(59), "rdu"), Err(Invalid::BesideName("a".repeat(59))));
	long.name = "a".repeat(58);
	assert_eq!(long.check(&"a".repeat(58), "rdu"), Ok(()));
}
