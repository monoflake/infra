//! A changed declaration with no new image: applied over the image the app already runs, as
//! lightly as what changed allows -- recorded, rendered into Caddy and cron, a memory ceiling set
//! live, or the container made again on the same image. See spec/architecture/host.md, "The machine
//! pulls; nothing pushes into it".

use super::Error;
use super::Outcome;
use super::admit::admit;
use super::version::{deploy, settle, staged};
use crate::Host;
use crate::store::{self, Deployed, Stage};
use deploy::Version;
use deploy::engine::DEFAULT_MEMORY_MB;
use deploy::manifest::Manifest;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Fields stored and nothing more: what people see, where the app is placed, how a later version
/// is rolled out, and how a deploy checks it.
const RECORDED: [&str; 5] =
	["display_name", "placements", "rollout", "container.health", "container.health_timeout"];

/// Fields host renders into Caddy and cron, which the app itself never reads.
const RENDERED: [&str; 3] = ["interface", "api", "schedules"];

/// The field Docker changes on a running container.
const MEMORY: &str = "container.memory_mb";

/// What a changed declaration takes, each with the fields that changed.
#[derive(Debug, PartialEq)]
pub(super) enum Apply {
	/// Stored alone.
	Record(Vec<String>),
	/// Stored, and Caddy and cron rendered again.
	Render(Vec<String>),
	/// Stored, rendered, and the memory ceiling, in MiB, set on the running container.
	Memory(u32, Vec<String>),
	/// The container made again on the same image, as a deploy is: any other field, one this host
	/// does not know among them.
	Recreate(Vec<String>),
}

impl Apply {
	/// Whether it restarts the app.
	pub(super) fn restarts(&self) -> bool {
		matches!(self, Apply::Recreate(_))
	}
}

/// A declaration's fields by name, `container`'s each as `container.<field>`; a field absent is
/// not among them.
fn fields(manifest: &Manifest) -> BTreeMap<String, Value> {
	let Ok(Value::Object(top)) = serde_json::to_value(manifest) else { return BTreeMap::new() };
	let mut found = BTreeMap::new();
	for (key, value) in top {
		match (key.as_str(), value) {
			("container", Value::Object(inner)) => {
				found.extend(inner.into_iter().map(|(field, value)| (format!("container.{field}"), value)));
			}
			(_, value) => {
				found.insert(key, value);
			}
		}
	}
	found
}

/// What going from `old` to `new` takes: the strongest of what each changed field needs, a field
/// in no list needing the container made again.
pub(super) fn classify(old: &Manifest, new: &Manifest) -> Apply {
	let (before, after) = (fields(old), fields(new));
	let keys: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
	let changed: Vec<String> =
		keys.into_iter().filter(|key| before.get(*key) != after.get(*key)).cloned().collect();
	let known = |field: &String| {
		RECORDED.contains(&field.as_str()) || RENDERED.contains(&field.as_str()) || field == MEMORY
	};
	if !changed.iter().all(known) {
		return Apply::Recreate(changed);
	}
	if changed.iter().any(|field| field == MEMORY) {
		let ceiling = new.container.as_ref().and_then(|container| container.memory_mb);
		return Apply::Memory(ceiling.unwrap_or(DEFAULT_MEMORY_MB), changed);
	}
	if changed.iter().any(|field| RENDERED.contains(&field.as_str())) {
		return Apply::Render(changed);
	}
	Apply::Record(changed)
}

/// What a declaration takes on this node: `None` when the app runs no image here in the
/// architecture it asks for -- not deployed, or asking for another -- which an image has to come
/// for. Read from the state; nothing is changed.
pub(super) fn planned(host: &Host, manifest: &Manifest) -> Result<Option<Apply>, Error> {
	let current = host.store.app(&manifest.name)?;
	let same = current.filter(|current| current.manifest.arch == manifest.arch);
	Ok(same.map(|current| classify(&current.manifest, manifest)))
}

/// How a re-declaration went.
pub(super) enum Redeclared {
	Applied(Outcome),
	/// No image of it runs here to keep; one has to be fetched.
	NeedsImage,
}

/// Apply `manifest` over the image its app runs, into event `id`, which it closes.
pub(super) async fn redeclare(
	host: &Arc<Host>,
	id: i64,
	manifest: Manifest,
) -> Result<Redeclared, Error> {
	let name = manifest.name.clone();
	staged(&host.store, id, Stage::Admitting, async { admit(host, &name, &manifest) }).await?;
	let _one = host.deploying.lock().await;
	let Some(current) = host.store.app(&name)?.filter(|app| app.manifest.arch == manifest.arch)
	else {
		return Ok(Redeclared::NeedsImage);
	};
	let apply = classify(&current.manifest, &manifest);
	let detail = match &apply {
		Apply::Record(changed) | Apply::Render(changed) => {
			format!("declaration applied, no restart: {}", changed.join(", "))
		}
		Apply::Memory(mb, _) => match host.engine.update_memory(&name, *mb).await {
			Ok(()) => format!("declaration applied, memory updated live to {mb} MiB"),
			Err(error) => {
				let why = format!("{MEMORY} needed a restart, the live update refused: {error}");
				return recreate(host, id, manifest, current, &why).await;
			}
		},
		Apply::Recreate(changed) => {
			let why = format!("{} needed a restart", changed.join(", "));
			return recreate(host, id, manifest, current, &why).await;
		}
	};
	let image = current.image.clone();
	let previous = Version { manifest: current.manifest, image: current.image };
	let stored = host.store.put_app(&Deployed {
		manifest,
		image: image.clone(),
		previous: Some(previous),
		deployed_at: jiff::Timestamp::now().to_string(),
		held: current.held,
	});
	let finished = match stored {
		Ok(()) => host.store.finish(id, store::Outcome::Succeeded, None, Some(&detail)),
		Err(error) => {
			let failed = host.store.finish(id, store::Outcome::Failed, None, Some(&error.to_string()));
			failed.and(Err(error))
		}
	};
	finished?;
	Ok(Redeclared::Applied(settle(host, name, image).await?))
}

/// The container made again on the image it runs, under `manifest`, as any deploy is.
async fn recreate(
	host: &Arc<Host>,
	id: i64,
	manifest: Manifest,
	current: Deployed,
	why: &str,
) -> Result<Redeclared, Error> {
	if let Err(error) = host.store.note(id, &format!("declaration applied; {why}")) {
		eprintln!("host: recording event {id}: {error}");
	}
	Ok(Redeclared::Applied(deploy(host, id, manifest, current.image).await?))
}

#[cfg(test)]
mod tests {
	use super::{Apply, classify};
	use deploy::manifest::Manifest;

	fn geo(extra: &str, container: &str) -> Manifest {
		Manifest::parse(&format!(
			"version = 1\nname = \"geo\"\nplacements = [\"rdu\"]\n{extra}\n[container]\nport = 23440\n\
			 health = \"/health\"\n{container}\n"
		))
		.unwrap()
	}

	fn named(fields: &[&str]) -> Vec<String> {
		fields.iter().map(|field| (*field).to_owned()).collect()
	}

	#[test]
	fn what_people_see_where_it_runs_and_how_it_is_checked_are_recorded_alone() {
		let old = geo("", "");
		let shown = geo("display_name = \"Geolocation\"", "");
		assert_eq!(classify(&old, &shown), Apply::Record(named(&["display_name"])));
		let mut placed = old.clone();
		placed.placements.push("tyo".into());
		assert_eq!(classify(&old, &placed), Apply::Record(named(&["placements"])));
		assert_eq!(
			classify(&old, &geo("rollout = \"beside\"", "")),
			Apply::Record(named(&["rollout"]))
		);
		let checked = geo("", "health_timeout = 120");
		assert_eq!(classify(&old, &checked), Apply::Record(named(&["container.health_timeout"])));
		assert!(!classify(&old, &checked).restarts());
	}

	#[test]
	fn names_routes_limits_and_schedules_are_rendered_without_a_restart() {
		let old = geo("", "");
		let routed = geo("", "");
		let mut routed = routed;
		routed.interface =
			Some(deploy::manifest::Interface { domain: Some("where".into()), lan: true, home: None });
		assert_eq!(classify(&old, &routed), Apply::Render(named(&["interface"])));
		let both = Manifest { display_name: Some("Geo".into()), ..routed.clone() };
		assert_eq!(classify(&old, &both), Apply::Render(named(&["display_name", "interface"])));
		assert!(!classify(&old, &both).restarts());
	}

	#[test]
	fn memory_is_set_live_and_a_ceiling_taken_away_is_the_default() {
		let old = geo("", "memory_mb = 64");
		let more = geo("", "memory_mb = 128");
		assert_eq!(classify(&old, &more), Apply::Memory(128, named(&["container.memory_mb"])));
		assert_eq!(classify(&old, &geo("", "")), Apply::Memory(512, named(&["container.memory_mb"])));
	}

	#[test]
	fn anything_else_makes_the_container_again_a_field_unknown_here_too() {
		let old = geo("", "memory_mb = 64");
		let ported = geo("", "memory_mb = 128");
		let mut ported = ported;
		ported.container.as_mut().unwrap().port = Some(23441);
		let changed = named(&["container.memory_mb", "container.port"]);
		assert_eq!(classify(&old, &ported), Apply::Recreate(changed));
		let stored = geo("[objects]\nbuckets = [\"photos\"]", "");
		assert!(classify(&geo("", ""), &stored).restarts());
		let shaped = geo("[shape]\nkind = \"peer\"", "");
		assert!(classify(&geo("", ""), &shaped).restarts());
		// A field this host has no list for is a restart, the safe side.
		let mut unknown = old.clone();
		unknown.arch = Some("arm64".into());
		assert_eq!(classify(&old, &unknown), Apply::Recreate(named(&["arch"])));
		assert_eq!(classify(&old, &old), Apply::Record(vec![]));
	}
}
