//! Whether a declaration may run: its name, where it answers, and everything it asks for. See
//! spec/architecture/host.md, "What a deployment may ask for is host's decision".

use super::{
	ARCHES, Api, DISPLAY_NAME_LENGTH, LONGEST_WINDOW, Limit, Manifest, OWN, Objects, PORTS, RESERVED,
	RESERVED_LABELS, Rollout, SHAPES, SIDES, Schedule, TIMEOUTS, VERSION, is_bucket, is_cron,
	is_every, is_home,
};
use crate::sidecar::Driver;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum Invalid {
	#[error("the declaration is not readable: {0}")]
	Malformed(String),
	#[error("declaration version {0} is not one this host reads; it reads {VERSION}")]
	Version(u32),
	#[error("`{0}` is not a name: lowercase letters, digits and inner hyphens, at most 63")]
	Name(String),
	#[error("`{0}` is not a label: lowercase letters, digits and inner hyphens, at most 63")]
	Label(String),
	#[error("`{0}` is reserved")]
	Reserved(String),
	#[error(
		"display name `{0}` is not 1 to {DISPLAY_NAME_LENGTH} characters with no space at either end"
	)]
	DisplayName(String),
	#[error("the declaration says `{declared}` but it was sent as `{requested}`")]
	Mismatch { declared: String, requested: String },
	#[error("`{name}` is not placed on `{node}`")]
	NotPlaced { name: String, node: String },
	#[error("`{0}` declares no container for a node to run")]
	NoContainer(String),
	#[error("port {0} is outside {start}-{end}", start = PORTS.start(), end = PORTS.end())]
	Port(u16),
	#[error("a container answers on a port or on a socket, exactly one of the two")]
	Answer,
	#[error("a socket is a file name in the app's own directory, which `[data]` has to mount")]
	Socket,
	#[error("an app on a socket has no port for an API or an interface to reach")]
	Unroutable,
	#[error("the health path has to start with `/`")]
	Health,
	#[error("the data path has to be absolute")]
	DataPath,
	#[error("a home is a path on the app's own site other than `/`")]
	Home,
	#[error("an API prefix is served on Workers alone, never by a node")]
	Prefix,
	#[error(
		"a limit names HTTP methods and a path from /, and allows at least once in 1 to 86400 seconds"
	)]
	Limit,
	#[error("an API's sides are one or both of `private` and `tunnel`")]
	Sides,
	#[error("`{0}` is not a bucket name S3 takes")]
	Bucket(String),
	#[error("`[objects]` names at least one bucket, each once")]
	Buckets,
	#[error("`{0}` is longer than the 63 characters a container's name on a network may be")]
	SidecarName(String),
	#[error("`[{0}]` asks for no memory at all, which Docker would read as no ceiling")]
	SidecarMemory(String),
	#[error(
		"schedule `{0}` needs exactly one of `cron` (five fields) or `every` (like `30s`), a path \
		 from /, and a timeout from 1 to 86400 seconds"
	)]
	Schedule(String),
	#[error("`{0}` declares `[[schedules]]` but answers through neither `[api]` nor a socket")]
	Unscheduled(String),
	#[error(
		"`[shape]` asks for `{0}`, and a role is one of scheduler, steward, reporter, peer and proxy"
	)]
	Shape(String),
	#[error("a proxy answers the apps it joins on a port, never on a socket")]
	ProxyPort,
	#[error("`[shape]` names a port for a peer alone, and `{0}` is not one")]
	ShapePort(String),
	#[error("a peer's `[shape]` port {0} is 0 or its container's own, which it publishes anyway")]
	PeerPort(u16),
	#[error(
		"a peer's `[shape]` ports are one or more, none 0, each once, and in place of `port`, not \
		 beside it"
	)]
	PeerPorts,
	#[error("`[driver]` provides `{0}`, and a driver is one of objects and postgres")]
	Driver(String),
	#[error(
		"`rollout = \"beside\"` runs two versions at once, and `[data]` would have both write it"
	)]
	BesideData,
	#[error(
		"`rollout = \"beside\"` runs two versions at once, and one socket cannot answer for both"
	)]
	BesideSocket,
	#[error(
		"`rollout = \"beside\"` runs two versions at once, and `[{0}]` runs a sidecar over the app's own directory"
	)]
	BesideSidecar(String),
	#[error("`rollout = \"beside\"` is for a plain sandboxed app, and this one asks to be `{0}`")]
	BesideRole(String),
	#[error("`{0}` is infra's own, shaped by its name, and is replaced in place")]
	BesideOwn(String),
	#[error("`arch` asks for `{0}`, and the one architecture an app may ask for is arm64")]
	Arch(String),
	#[error("`{0}` is too long to run beside itself under a name of its own, at most 63 characters")]
	BesideName(String),
}

impl Manifest {
	/// Read a declaration, refusing a version this host does not know before reading the rest.
	pub fn parse(text: &str) -> Result<Self, Invalid> {
		let table: toml::Table =
			text.parse().map_err(|e: toml::de::Error| Invalid::Malformed(e.to_string()))?;
		let version = table
			.get("version")
			.and_then(toml::Value::as_integer)
			.ok_or_else(|| Invalid::Malformed("no `version`".into()))?;
		let version = u32::try_from(version)
			.map_err(|_| Invalid::Malformed("`version` is not a version".into()))?;
		if version != VERSION {
			return Err(Invalid::Version(version));
		}
		toml::from_str(text).map_err(|e: toml::de::Error| Invalid::Malformed(e.to_string()))
	}

	/// Whether this node may run it under the name it was sent as.
	pub fn check(&self, requested: &str, node: &str) -> Result<(), Invalid> {
		check_name(requested)?;
		self.check_rest(requested, node)
	}

	/// The same, for one of the platform's own, whose names are otherwise reserved.
	pub fn check_own(&self, requested: &str, node: &str) -> Result<(), Invalid> {
		if !OWN.contains(&requested) {
			return Err(Invalid::Name(requested.into()));
		}
		self.check_rest(requested, node)
	}

	fn check_rest(&self, requested: &str, node: &str) -> Result<(), Invalid> {
		if self.name != requested {
			return Err(Invalid::Mismatch { declared: self.name.clone(), requested: requested.into() });
		}
		if let Some(shown) = self.display_name.as_ref().filter(|shown| !sound_display_name(shown)) {
			return Err(Invalid::DisplayName(shown.clone()));
		}
		if let Some(arch) = self.arch.as_ref().filter(|arch| !ARCHES.contains(&arch.as_str())) {
			return Err(Invalid::Arch(arch.clone()));
		}
		if !self.placements.iter().any(|placement| placement == node) {
			return Err(Invalid::NotPlaced { name: self.name.clone(), node: node.into() });
		}
		let Some(container) = &self.container else {
			return Err(Invalid::NoContainer(self.name.clone()));
		};
		match (container.port, &container.socket) {
			// A driver's port is its sidecars', each on its own app's network, where no service's
			// port can meet it: Postgres keeps 5432. See platform's spec/architecture/databases.md.
			(Some(port), None) if !PORTS.contains(&port) && self.driver.is_none() => {
				return Err(Invalid::Port(port));
			}
			(Some(_), None) => {}
			(None, Some(socket)) => {
				let file = !socket.is_empty() && !socket.contains('/') && socket != "." && socket != "..";
				if !file || self.data.is_none() {
					return Err(Invalid::Socket);
				}
				if self.api.is_some() || self.interface.is_some() {
					return Err(Invalid::Unroutable);
				}
			}
			_ => return Err(Invalid::Answer),
		}
		if !container.health.starts_with('/') {
			return Err(Invalid::Health);
		}
		if self.data.as_ref().is_some_and(|data| !data.path.starts_with('/')) {
			return Err(Invalid::DataPath);
		}
		let home = self.interface.as_ref().and_then(|interface| interface.home.as_deref());
		if home.is_some_and(|home| !is_home(home)) {
			return Err(Invalid::Home);
		}
		if let Some(domain) = self.interface.as_ref().and_then(|interface| interface.domain.as_deref())
		{
			check_domain(domain)?;
		}
		if self.api.as_ref().is_some_and(|api| api.prefix.is_some()) {
			return Err(Invalid::Prefix);
		}
		let methods = ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];
		let sound = |limit: &Limit| {
			!limit.methods.is_empty()
				&& limit.methods.iter().all(|method| methods.contains(&method.as_str()))
				&& limit.path.starts_with('/')
				&& !limit.path.trim_end_matches("/*").contains('*')
				&& limit.count > 0
				&& (1..=LONGEST_WINDOW).contains(&limit.seconds)
				&& limit.burst != Some(0)
				&& limit.subject() == "address"
		};
		// A call is counted under one row of each kind of subject and Caddy counts every row, so no
		// call may be covered twice by one kind: no two rows of a subject on one method where either
		// path covers the other, a prefix ending in `/*` covering everything under it.
		let covers = |outer: &str, inner: &str| match outer.strip_suffix('*') {
			Some(stem) => inner.starts_with(stem),
			None => outer == inner,
		};
		let covered = |api: &Api| {
			api.limits.iter().enumerate().all(|(index, limit)| {
				api.limits[index + 1..].iter().all(|other| {
					limit.subject() != other.subject()
						|| !limit.methods.iter().any(|method| other.methods.contains(method))
						|| !(covers(&limit.path, &other.path) || covers(&other.path, &limit.path))
				})
			})
		};
		if self.api.as_ref().is_some_and(|api| !api.limits.iter().all(sound) || !covered(api)) {
			return Err(Invalid::Limit);
		}
		let sided = |sides: &Vec<String>| {
			!sides.is_empty() && sides.iter().all(|side| SIDES.contains(&side.as_str()))
		};
		if self.api.as_ref().and_then(|api| api.sides.as_ref()).is_some_and(|sides| !sided(sides)) {
			return Err(Invalid::Sides);
		}
		if let Some(objects) = &self.objects {
			check_objects(objects)?;
		}
		for driver in self.drivers() {
			if !is_label(&driver.sidecar_of(&self.name)) {
				return Err(Invalid::SidecarName(driver.sidecar_of(&self.name)));
			}
			if driver.memory_mb(self) == Some(0) {
				return Err(Invalid::SidecarMemory(driver.name().into()));
			}
		}
		if let Some(shape) = self.shape.as_ref().filter(|shape| !SHAPES.contains(&shape.kind.as_str()))
		{
			return Err(Invalid::Shape(shape.kind.clone()));
		}
		if let Some(shape) = &self.shape
			&& (shape.port.is_some() || shape.ports.is_some())
			&& shape.kind != "peer"
		{
			return Err(Invalid::ShapePort(shape.kind.clone()));
		}
		if self.shape.as_ref().is_some_and(|shape| shape.kind == "proxy") && container.port.is_none() {
			return Err(Invalid::ProxyPort);
		}
		if let Some(port) = self.shape.as_ref().and_then(|shape| shape.port) {
			if self.shape.as_ref().is_some_and(|shape| shape.ports.is_some()) {
				return Err(Invalid::PeerPorts);
			}
			if port == 0 || container.port == Some(port) {
				return Err(Invalid::PeerPort(port));
			}
		}
		if let Some(ports) = self.shape.as_ref().and_then(|shape| shape.ports.as_ref()) {
			let distinct: std::collections::HashSet<&u16> = ports.iter().collect();
			if ports.is_empty() || ports.contains(&0) || distinct.len() != ports.len() {
				return Err(Invalid::PeerPorts);
			}
		}
		let provides = self.driver.as_ref().map(|driver| driver.provides.as_str());
		if let Some(provides) = provides.filter(|provides| Driver::named(provides).is_none()) {
			return Err(Invalid::Driver(provides.to_owned()));
		}
		if !self.schedules.is_empty() {
			if let Some(bad) = self.schedules.iter().find(|schedule| !sound_schedule(schedule)) {
				return Err(Invalid::Schedule(bad.name.clone()));
			}
			if self.api.is_none() && container.socket.is_none() {
				return Err(Invalid::Unscheduled(self.name.clone()));
			}
		}
		if self.rollout == Rollout::Beside {
			self.check_beside()?;
		}
		Ok(())
	}

	/// Whether two of its containers can run at once, which `beside` does for a moment: nothing on
	/// its node to write twice, nothing published or shared that one container holds, and the plain
	/// sandbox. See spec/architecture/host.md, "An app chooses how it is rolled out, and keeping
	/// nothing earns a gapless one".
	pub fn check_beside(&self) -> Result<(), Invalid> {
		if OWN.contains(&self.name.as_str()) {
			return Err(Invalid::BesideOwn(self.name.clone()));
		}
		if self.container.as_ref().is_some_and(|container| container.socket.is_some()) {
			return Err(Invalid::BesideSocket);
		}
		if self.data.is_some() {
			return Err(Invalid::BesideData);
		}
		if let Some(driver) = self.drivers().next() {
			return Err(Invalid::BesideSidecar(driver.name().into()));
		}
		if let Some(shape) = &self.shape {
			return Err(Invalid::BesideRole(shape.kind.clone()));
		}
		if let Some(driver) = &self.driver {
			return Err(Invalid::BesideRole(format!("the {} driver", driver.provides)));
		}
		// A container's name on a network is a DNS label, 63 characters at most.
		if crate::beside::beside_of(&self.name).len() > 63 {
			return Err(Invalid::BesideName(self.name.clone()));
		}
		Ok(())
	}
}

/// Whether a display name is free text people can read: not blank, trimmed, and short.
fn sound_display_name(shown: &str) -> bool {
	!shown.is_empty() && shown.trim() == shown && shown.chars().count() <= DISPLAY_NAME_LENGTH
}

/// Whether a schedule is shaped so `cron` could run it: exactly one clock, a path from `/`, and a
/// timeout in range. See platform's spec/architecture/cron.md.
fn sound_schedule(schedule: &Schedule) -> bool {
	let clocked = match (&schedule.cron, &schedule.every) {
		(Some(cron), None) => is_cron(cron),
		(None, Some(every)) => is_every(every),
		_ => false,
	};
	clocked && schedule.path.starts_with('/') && TIMEOUTS.contains(&schedule.timeout)
}

/// Buckets S3 takes, each once. See platform's spec/architecture/objects.md, "A sidecar per app,
/// over the app's own directory".
fn check_objects(objects: &Objects) -> Result<(), Invalid> {
	if let Some(bucket) = objects.buckets.iter().find(|bucket| !is_bucket(bucket)) {
		return Err(Invalid::Bucket(bucket.clone()));
	}
	let distinct: std::collections::HashSet<&String> = objects.buckets.iter().collect();
	if objects.buckets.is_empty() || distinct.len() != objects.buckets.len() {
		return Err(Invalid::Buckets);
	}
	Ok(())
}

/// The shape of any DNS label this format uses, name or domain alike.
pub(super) fn is_label(value: &str) -> bool {
	!value.is_empty()
		&& value.len() <= 63
		&& !value.starts_with('-')
		&& !value.ends_with('-')
		&& value.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// One DNS label, and not one of the platform's own. Every place an app appears is this name, so
/// it has to be valid in all of them at once.
pub fn check_name(name: &str) -> Result<(), Invalid> {
	if !is_label(name) {
		return Err(Invalid::Name(name.into()));
	}
	let sidecar = Driver::ALL.iter().any(|driver| name.ends_with(driver.suffix()));
	if RESERVED.contains(&name) || RESERVED_LABELS.contains(&name) || sidecar {
		return Err(Invalid::Reserved(name.into()));
	}
	Ok(())
}

/// A label an interface or a route answers on: `domain`, or a route's own name. Apps and routes
/// share this one namespace, on top of a smaller reserved list than a name's -- see
/// spec/architecture/host.md, "One name inside, and a domain label outside". Whether it collides
/// with another app or route already answering on it is checked where both are known, in host's
/// store.
pub fn check_domain(domain: &str) -> Result<(), Invalid> {
	if !is_label(domain) {
		return Err(Invalid::Label(domain.into()));
	}
	if RESERVED.contains(&domain) || RESERVED_LABELS.contains(&domain) {
		return Err(Invalid::Reserved(domain.into()));
	}
	Ok(())
}
