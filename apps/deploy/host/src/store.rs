//! What host knows, and the only thing Caddy's configuration is derived from: every app with the
//! version it runs and the one before it, every route to something that is not a container, and
//! every event. Three files by subject; see spec/architecture/host.md, "What host keeps, and
//! where".

use deploy::Manifest;
pub use deploy::Version;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

/// An app as it runs now.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Deployed {
	pub manifest: Manifest,
	pub image: String,
	/// What a failed deploy puts back, and what a rollback offers.
	pub previous: Option<Version>,
	pub deployed_at: String,
	/// Stopped from the panel, and kept stopped until started from it; see spec/architecture/host.md,
	/// "A stop holds until a start".
	#[serde(default)]
	pub held: bool,
}

/// A name that reaches something host does not run: the NAS, or a container another compose
/// project owns until it is taken over.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Route {
	pub name: String,
	/// `host:port`, dialed by Caddy as plain HTTP, or `https://host[:port]` for a device on the LAN
	/// that speaks only TLS under its own certificate.
	pub upstream: String,
	pub private: bool,
	pub public: bool,
	/// Where a request for `/` is sent, for an application whose interface does not live at its
	/// root: gemini's panel is under `/admin`, and its name alone should open it.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub home: Option<String>,
}

/// The label an app answers on from outside, if it has an interface at all: its own `domain`, or
/// its name. See spec/architecture/host.md, "One name inside, and a domain label outside".
fn label(manifest: &Manifest) -> Option<&str> {
	manifest.interface.as_ref().map(|interface| interface.label(&manifest.name))
}

fn route_from(row: &rusqlite::Row<'_>) -> rusqlite::Result<Route> {
	Ok(Route {
		name: row.get(0)?,
		upstream: row.get(1)?,
		private: row.get(2)?,
		public: row.get(3)?,
		home: row.get(4)?,
	})
}

/// What was done to an app.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Action {
	Deploy,
	Redeploy,
	Rollback,
	RollbackWithData,
	Start,
	Stop,
	Restart,
	/// Taken off the node by hand; its history stays.
	Remove,
}

/// How an event ended, or that it has not yet.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
	Running,
	Succeeded,
	Failed,
	/// Not attempted: a deploy that arrived while the app was held stopped.
	Skipped,
}

/// How far a deploy has come and, once it has ended, the last step it reached: a failure names
/// where it failed by this and the outcome together. An act on a container as it is has none.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
	/// Fetching a CI run's artifact and checking it against the digest GitHub recorded.
	Downloading,
	/// Reading the declaration and taking it or passing it over: placed here, not held, runnable.
	Admitting,
	/// Loading the archive into Docker.
	Loading,
	/// Replacing what runs, through the new version's health check; for an app rolled out beside
	/// its predecessor, starting the new version under its own name.
	Starting,
	/// Beside: the new version answering its health check, the running one untouched.
	Checking,
	/// Beside: Caddy's route moving to the new version.
	Switching,
	/// Beside: the version before finishing what it answers, then removed.
	Draining,
	/// Held until the canary has taken the run: a new host, keeper or Caddy reaches one node first.
	Waiting,
}

/// What started an event: a CI run, an upload, or the panel.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Source {
	/// `run`, `upload` or `panel`.
	pub kind: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub run: Option<u64>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub commit: Option<String>,
	/// The repository the run is numbered in, as `owner/name`; absent from a row from before.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub repository: Option<String>,
}

impl Source {
	pub fn panel() -> Self {
		Self { kind: "panel".into(), ..Self::default() }
	}

	pub fn upload() -> Self {
		Self { kind: "upload".into(), ..Self::default() }
	}

	/// keeper, acting on host: recreating it, or taking an upload of it.
	pub fn keeper() -> Self {
		Self { kind: "keeper".into(), ..Self::default() }
	}

	pub fn run(run: u64, commit: Option<String>) -> Self {
		Self { kind: "run".into(), run: Some(run), commit, repository: None }
	}

	/// The same, saying which repository's run it is.
	pub fn of(self, repository: &str) -> Self {
		Self { repository: Some(repository.to_owned()), ..self }
	}
}

/// How a deploy passed over for a newer run's build of the same app ends its reason, after `run N`;
/// the one pass-over that settles an app. See rollout/catch_up.rs.
pub const BUILT_AGAIN: &str = "built it again, and is the one this node runs";

/// How a deploy passed over because its image is the one the app already runs, with the same
/// declaration, begins its reason; as good as deployed. See rollout/version.rs, `unchanged`.
pub const UNCHANGED: &str = "unchanged: ";

/// An event that is over, recorded whole.
pub struct Finished {
	pub app: String,
	pub action: Action,
	pub source: Source,
	pub image: Option<String>,
	pub outcome: Outcome,
	pub detail: Option<String>,
	pub started_at: String,
	pub finished_at: String,
}

/// One app's deploy row from a CI run.
#[derive(Debug, Clone, PartialEq)]
pub struct Taken {
	pub run: u64,
	pub repository: Option<String>,
	pub app: String,
	pub outcome: Outcome,
}

/// One row of an app's history.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Event {
	pub id: i64,
	pub app: String,
	pub action: Action,
	pub source: Source,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub image: Option<String>,
	/// The snapshot a deploy took before starting, which a rollback with data restores.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub snapshot: Option<String>,
	pub outcome: Outcome,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub stage: Option<Stage>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub detail: Option<String>,
	pub started_at: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub finished_at: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
	#[error("the state database: {0}")]
	Database(#[from] rusqlite::Error),
	#[error("a stored record is unreadable: {0}")]
	Record(#[from] serde_json::Error),
	#[error("reading the state from before the split: {0}")]
	Split(#[from] std::io::Error),
	#[error("`{0}` is already an app")]
	TakenByApp(String),
	#[error("`{0}` is already a route")]
	TakenByRoute(String),
}

const APPS: &str = "CREATE TABLE IF NOT EXISTS apps (
	name TEXT PRIMARY KEY,
	manifest TEXT NOT NULL,
	image TEXT NOT NULL,
	previous TEXT,
	deployed_at TEXT NOT NULL,
	held INTEGER NOT NULL DEFAULT 0
);";

const ROUTES: &str = "CREATE TABLE IF NOT EXISTS routes (
	name TEXT PRIMARY KEY,
	upstream TEXT NOT NULL,
	private INTEGER NOT NULL,
	public INTEGER NOT NULL,
	home TEXT
);";

const HISTORY: &str = "CREATE TABLE IF NOT EXISTS events (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	app TEXT NOT NULL,
	action TEXT NOT NULL,
	source TEXT NOT NULL,
	image TEXT,
	snapshot TEXT,
	outcome TEXT NOT NULL,
	detail TEXT,
	started_at TEXT NOT NULL,
	finished_at TEXT,
	stage TEXT
);
CREATE INDEX IF NOT EXISTS events_by_app ON events (app, id);";

/// A `history.db` from before stages gains the column, each row reading as the step it ended at: a
/// skip was decided while admitting, and any other deploy was recorded only once its image loaded.
const STAGES: &str = "BEGIN;
ALTER TABLE events ADD COLUMN stage TEXT;
UPDATE events SET stage = CASE
	WHEN action IN ('start', 'stop', 'restart') THEN NULL
	WHEN outcome = 'skipped' THEN 'admitting'
	ELSE 'starting'
END;
COMMIT;";

/// Why an event still running when host starts is closed failed.
pub const RESTARTED: &str = "host restarted before it finished";

/// Each image nothing could run again, and since when: it is removed an hour after. See
/// spec/architecture/host.md, "An image is kept while something could run it".
const IMAGES: &str = "CREATE TABLE IF NOT EXISTS flagged (
	id TEXT PRIMARY KEY,
	since TEXT NOT NULL
);";

/// The history with its `stage` column, added once to a file from before it.
fn staged(connection: Connection) -> Result<Connection, Error> {
	let query = "SELECT COUNT(*) FROM pragma_table_info('events') WHERE name = 'stage'";
	let columns: i64 = connection.query_row(query, [], |row| row.get(0))?;
	if columns == 0 {
		connection.execute_batch(STAGES)?;
	}
	Ok(connection)
}

fn open(path: &Path, schema: &str) -> Result<Connection, Error> {
	let connection = Connection::open(path)?;
	connection.execute_batch("PRAGMA journal_mode = WAL;")?;
	connection.execute_batch(schema)?;
	Ok(connection)
}

/// A value as the text column it is stored in.
fn text<T: Serialize>(value: &T) -> Result<String, Error> {
	let json = serde_json::to_value(value)?;
	Ok(json.as_str().map_or_else(|| json.to_string(), str::to_owned))
}

fn parsed<T: for<'a> Deserialize<'a>>(text: &str) -> Result<T, Error> {
	let json = serde_json::from_str(text).unwrap_or_else(|_| serde_json::Value::String(text.into()));
	Ok(serde_json::from_value(json)?)
}

fn now() -> String {
	jiff::Timestamp::now().to_string()
}

pub struct Store {
	apps: Mutex<Connection>,
	routes: Mutex<Connection>,
	history: Mutex<Connection>,
	images: Mutex<Connection>,
}

/// A panic while holding one leaves nothing half-written: every write is one statement.
fn lock(connection: &Mutex<Connection>) -> MutexGuard<'_, Connection> {
	connection.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Store {
	/// The four files in `directory`, made from a `host.db` there if it is the state from before
	/// the split and they do not exist yet.
	pub fn open(directory: &Path) -> Result<Self, Error> {
		std::fs::create_dir_all(directory)?;
		let legacy = directory.join("host.db");
		let split = legacy.exists() && !directory.join("apps.db").exists();
		let store = Self {
			apps: Mutex::new(open(&directory.join("apps.db"), APPS)?),
			routes: Mutex::new(open(&directory.join("routes.db"), ROUTES)?),
			history: Mutex::new(staged(open(&directory.join("history.db"), HISTORY)?)?),
			images: Mutex::new(open(&directory.join("images.db"), IMAGES)?),
		};
		if split {
			store.read_legacy(&legacy)?;
		}
		Ok(store)
	}

	/// Copy a single `host.db`'s apps and routes into the split files, then rename it aside.
	fn read_legacy(&self, legacy: &Path) -> Result<(), Error> {
		let old = Connection::open(legacy)?;
		let columns: Vec<String> = old
			.prepare("SELECT name FROM pragma_table_info('routes')")?
			.query_map([], |row| row.get(0))?
			.collect::<Result<_, _>>()?;
		let home = if columns.iter().any(|column| column == "home") { "home" } else { "NULL" };
		{
			let apps = lock(&self.apps);
			let mut rows =
				old.prepare("SELECT name, manifest, image, previous, deployed_at FROM apps")?;
			for row in rows.query_map([], |row| {
				Ok((
					row.get::<_, String>(0)?,
					row.get::<_, String>(1)?,
					row.get::<_, String>(2)?,
					row.get::<_, Option<String>>(3)?,
					row.get::<_, String>(4)?,
				))
			})? {
				let (name, manifest, image, previous, deployed_at) = row?;
				apps.execute(
					"INSERT OR IGNORE INTO apps (name, manifest, image, previous, deployed_at)
					VALUES (?1, ?2, ?3, ?4, ?5)",
					params![name, manifest, image, previous, deployed_at],
				)?;
			}
		}
		{
			let routes = lock(&self.routes);
			let query = format!("SELECT name, upstream, private, public, {home} FROM routes");
			let mut rows = old.prepare(&query)?;
			for route in rows.query_map([], route_from)? {
				let route = route?;
				routes.execute(
					"INSERT OR IGNORE INTO routes (name, upstream, private, public, home)
					VALUES (?1, ?2, ?3, ?4, ?5)",
					params![route.name, route.upstream, route.private, route.public, route.home],
				)?;
			}
		}
		old.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
		drop(old);
		std::fs::rename(legacy, legacy.with_extension("db.split"))?;
		for suffix in ["db-wal", "db-shm"] {
			let _ = std::fs::remove_file(legacy.with_extension(suffix));
		}
		Ok(())
	}

	/// Every flagged image and since when.
	pub fn flagged(&self) -> Result<std::collections::HashMap<String, jiff::Timestamp>, Error> {
		let connection = lock(&self.images);
		let mut statement = connection.prepare("SELECT id, since FROM flagged")?;
		let rows =
			statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
		let mut flagged = std::collections::HashMap::new();
		for row in rows {
			let (id, since) = row?;
			if let Ok(since) = since.parse() {
				flagged.insert(id, since);
			}
		}
		Ok(flagged)
	}

	pub fn flag(&self, id: &str, since: jiff::Timestamp) -> Result<(), Error> {
		lock(&self.images).execute(
			"INSERT OR IGNORE INTO flagged (id, since) VALUES (?1, ?2)",
			params![id, since.to_string()],
		)?;
		Ok(())
	}

	pub fn unflag(&self, id: &str) -> Result<(), Error> {
		lock(&self.images).execute("DELETE FROM flagged WHERE id = ?1", params![id])?;
		Ok(())
	}

	pub fn apps(&self) -> Result<Vec<Deployed>, Error> {
		let connection = lock(&self.apps);
		let mut statement = connection
			.prepare("SELECT manifest, image, previous, deployed_at, held FROM apps ORDER BY name")?;
		let rows = statement.query_map([], |row| {
			Ok((
				row.get::<_, String>(0)?,
				row.get(1)?,
				row.get::<_, Option<String>>(2)?,
				row.get(3)?,
				row.get(4)?,
			))
		})?;
		rows
			.map(|row| {
				let (manifest, image, previous, deployed_at, held) = row?;
				Ok(Deployed {
					manifest: serde_json::from_str(&manifest)?,
					image,
					previous: previous.map(|text| serde_json::from_str(&text)).transpose()?,
					deployed_at,
					held,
				})
			})
			.collect()
	}

	pub fn app(&self, name: &str) -> Result<Option<Deployed>, Error> {
		Ok(self.apps()?.into_iter().find(|app| app.manifest.name == name))
	}

	/// Store what runs now. The hold is not this call's to change; see `hold`.
	pub fn put_app(&self, app: &Deployed) -> Result<(), Error> {
		let name = &app.manifest.name;
		if let Some(this_label) = label(&app.manifest) {
			if self.route(this_label)?.is_some() {
				return Err(Error::TakenByRoute(this_label.to_owned()));
			}
			for other in self.apps()? {
				if other.manifest.name != *name && label(&other.manifest) == Some(this_label) {
					return Err(Error::TakenByApp(this_label.to_owned()));
				}
			}
		}
		let previous = app.previous.as_ref().map(serde_json::to_string).transpose()?;
		lock(&self.apps).execute(
			"INSERT INTO apps (name, manifest, image, previous, deployed_at) VALUES (?1, ?2, ?3, ?4, ?5)
			ON CONFLICT (name) DO UPDATE SET manifest = ?2, image = ?3, previous = ?4, deployed_at = ?5",
			params![name, serde_json::to_string(&app.manifest)?, app.image, previous, app.deployed_at],
		)?;
		Ok(())
	}

	/// Take an app out of what runs now, its history left as it is. True when there was one.
	pub fn forget_app(&self, name: &str) -> Result<bool, Error> {
		Ok(lock(&self.apps).execute("DELETE FROM apps WHERE name = ?1", params![name])? > 0)
	}

	/// Hold an app stopped, or release it.
	pub fn hold(&self, name: &str, held: bool) -> Result<(), Error> {
		lock(&self.apps).execute("UPDATE apps SET held = ?2 WHERE name = ?1", params![name, held])?;
		Ok(())
	}

	pub fn routes(&self) -> Result<Vec<Route>, Error> {
		let connection = lock(&self.routes);
		let mut statement = connection
			.prepare("SELECT name, upstream, private, public, home FROM routes ORDER BY name")?;
		let rows = statement.query_map([], route_from)?;
		Ok(rows.collect::<Result<_, _>>()?)
	}

	fn route(&self, name: &str) -> Result<Option<Route>, Error> {
		let connection = lock(&self.routes);
		let query = "SELECT name, upstream, private, public, home FROM routes WHERE name = ?1";
		Ok(connection.query_row(query, [name], route_from).optional()?)
	}

	/// One namespace of labels for apps and routes: no two things answer on one. See
	/// spec/architecture/host.md, "One name inside, and a domain label outside".
	pub fn put_route(&self, route: &Route) -> Result<(), Error> {
		for app in self.apps()? {
			if label(&app.manifest) == Some(route.name.as_str()) {
				return Err(Error::TakenByApp(route.name.clone()));
			}
		}
		lock(&self.routes).execute(
			"INSERT INTO routes (name, upstream, private, public, home) VALUES (?1, ?2, ?3, ?4, ?5)
			ON CONFLICT (name) DO UPDATE SET upstream = ?2, private = ?3, public = ?4, home = ?5",
			params![route.name, route.upstream, route.private, route.public, route.home],
		)?;
		Ok(())
	}

	pub fn delete_route(&self, name: &str) -> Result<bool, Error> {
		Ok(lock(&self.routes).execute("DELETE FROM routes WHERE name = ?1", [name])? > 0)
	}

	/// Open an event, returning its id for `finish`. One that is over already -- a skip -- is opened
	/// finished.
	pub fn record(
		&self,
		app: &str,
		action: Action,
		source: &Source,
		image: Option<&str>,
		outcome: Outcome,
		stage: Option<Stage>,
	) -> Result<i64, Error> {
		let started = now();
		let finished = (outcome != Outcome::Running).then(|| started.clone());
		let stage = stage.as_ref().map(text).transpose()?;
		let connection = lock(&self.history);
		connection.execute(
			"INSERT INTO events (app, action, source, image, outcome, started_at, finished_at, stage)
			VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
			params![app, text(&action)?, text(source)?, image, text(&outcome)?, started, finished, stage],
		)?;
		Ok(connection.last_insert_rowid())
	}

	/// Record an event that is over already, with the times it began and ended: what keeper did to
	/// host while host was down to record it.
	pub fn recorded(&self, event: &Finished) -> Result<i64, Error> {
		let connection = lock(&self.history);
		connection.execute(
			"INSERT INTO events (app, action, source, image, outcome, detail, started_at, finished_at,
			stage) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
			params![
				event.app,
				text(&event.action)?,
				text(&event.source)?,
				event.image,
				text(&event.outcome)?,
				event.detail,
				event.started_at,
				event.finished_at,
				text(&Stage::Starting)?
			],
		)?;
		Ok(connection.last_insert_rowid())
	}

	/// Move an open event on to `stage`, naming its image once it is known.
	pub fn advance(&self, id: i64, stage: Stage, image: Option<&str>) -> Result<(), Error> {
		lock(&self.history).execute(
			"UPDATE events SET stage = ?2, image = COALESCE(?3, image) WHERE id = ?1",
			params![id, text(&stage)?, image],
		)?;
		Ok(())
	}

	/// Say something of an open event that its close keeps unless it fails: a deploy that went
	/// another way than its app asked.
	pub fn note(&self, id: i64, detail: &str) -> Result<(), Error> {
		lock(&self.history)
			.execute("UPDATE events SET detail = ?2 WHERE id = ?1", params![id, detail])?;
		Ok(())
	}

	/// Close every event still running, which nothing is left to finish once host has restarted,
	/// answering how many there were.
	pub fn sweep(&self) -> Result<usize, Error> {
		let closed = lock(&self.history).execute(
			"UPDATE events SET outcome = ?1, detail = ?2, finished_at = ?3 WHERE outcome = ?4",
			params![text(&Outcome::Failed)?, RESTARTED, now(), text(&Outcome::Running)?],
		)?;
		Ok(closed)
	}

	/// Close an event with how it ended. Without a `detail` of its own it keeps what `note` said.
	pub fn finish(
		&self,
		id: i64,
		outcome: Outcome,
		snapshot: Option<&str>,
		detail: Option<&str>,
	) -> Result<(), Error> {
		lock(&self.history).execute(
			"UPDATE events SET outcome = ?2, snapshot = COALESCE(?3, snapshot),
			detail = COALESCE(?4, detail), finished_at = ?5 WHERE id = ?1",
			params![id, text(&outcome)?, snapshot, detail, now()],
		)?;
		Ok(())
	}

	/// An app's events, the newest first, `limit` of them before the id `before`; every app's
	/// when `app` is none.
	pub fn events(
		&self,
		app: Option<&str>,
		before: Option<i64>,
		limit: u32,
	) -> Result<Vec<Event>, Error> {
		let connection = lock(&self.history);
		let mut statement = connection.prepare(
			"SELECT id, app, action, source, image, snapshot, outcome, detail, started_at, finished_at,
			stage FROM events WHERE (?1 IS NULL OR app = ?1) AND (?2 IS NULL OR id < ?2)
			ORDER BY id DESC LIMIT ?3",
		)?;
		let rows = statement.query_map(params![app, before, limit], |row| {
			Ok((
				row.get::<_, i64>(0)?,
				row.get::<_, String>(1)?,
				row.get::<_, String>(2)?,
				row.get::<_, String>(3)?,
				row.get::<_, Option<String>>(4)?,
				row.get::<_, Option<String>>(5)?,
				row.get::<_, String>(6)?,
				row.get::<_, Option<String>>(7)?,
				row.get::<_, String>(8)?,
				row.get::<_, Option<String>>(9)?,
				row.get::<_, Option<String>>(10)?,
			))
		})?;
		rows
			.map(|row| {
				let (
					id,
					app,
					action,
					source,
					image,
					snapshot,
					outcome,
					detail,
					started_at,
					finished_at,
					stage,
				) = row?;
				Ok(Event {
					id,
					app,
					action: parsed(&action)?,
					source: parsed(&source)?,
					image,
					snapshot,
					outcome: parsed(&outcome)?,
					stage: stage.as_deref().map(parsed).transpose()?,
					detail,
					started_at,
					finished_at,
				})
			})
			.collect()
	}

	/// Every app's deploy row from a CI run, with the run, its repository when the row names one, and
	/// how it ended: what a node has taken of each run. See rollout/catch_up.rs.
	pub fn taken(&self) -> Result<Vec<Taken>, Error> {
		let connection = lock(&self.history);
		let mut statement = connection.prepare(
			"SELECT json_extract(source, '$.run'), json_extract(source, '$.repository'), app, outcome
			FROM events WHERE action = 'deploy' AND json_extract(source, '$.kind') = 'run'
			AND json_extract(source, '$.run') IS NOT NULL",
		)?;
		let rows = statement.query_map([], |row| {
			Ok((row.get::<_, i64>(0)?, row.get(1)?, row.get(2)?, row.get::<_, String>(3)?))
		})?;
		rows
			.map(|row| {
				let (run, repository, app, outcome) = row?;
				let run = u64::try_from(run).unwrap_or_default();
				Ok(Taken { run, repository, app, outcome: parsed(&outcome)? })
			})
			.collect()
	}

	/// The apps whose deploy from `run` is settled, and so is not done again when a notice of the
	/// run is taken again: deployed, passed over because a newer run built it again, or passed over
	/// as unchanged. A row passed over for any other reason -- rolled out by hand, held, placed
	/// elsewhere -- or one that failed settles nothing, so the operator, or the notice taken again,
	/// still deploys it.
	pub fn settled(&self, run: u64) -> Result<std::collections::HashSet<String>, Error> {
		let connection = lock(&self.history);
		let mut statement = connection.prepare(
			"SELECT app FROM events WHERE action = 'deploy' AND json_extract(source, '$.kind') = 'run'
			AND json_extract(source, '$.run') = ?1
			AND (outcome = 'succeeded' OR (outcome = 'skipped'
				AND (detail LIKE '% ' || ?2 OR substr(detail, 1, length(?3)) = ?3)))",
		)?;
		let run = i64::try_from(run).unwrap_or(i64::MAX);
		let rows =
			statement.query_map(params![run, BUILT_AGAIN, UNCHANGED], |row| row.get::<_, String>(0))?;
		Ok(rows.collect::<Result<_, _>>()?)
	}

	/// The snapshot taken before `image` was deployed: what the directory held before the version
	/// an app runs now first ran. What a rollback with data restores.
	pub fn snapshot_before(&self, app: &str, image: &str) -> Result<Option<String>, Error> {
		let connection = lock(&self.history);
		let query = "SELECT snapshot FROM events WHERE app = ?1 AND image = ?2
			AND snapshot IS NOT NULL AND action = 'deploy' AND outcome = 'succeeded'
			ORDER BY id DESC LIMIT 1";
		Ok(connection.query_row(query, [app, image], |row| row.get(0)).optional()?)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn geo() -> Manifest {
		Manifest::parse(include_str!("../../../../libs/deploy/fixtures/geo.toml")).unwrap()
	}

	#[test]
	fn a_flag_keeps_the_moment_it_was_first_set_until_it_is_taken_off() {
		let directory = tempfile::tempdir().unwrap();
		let store = Store::open(directory.path()).unwrap();
		let first: jiff::Timestamp = "2026-09-28T12:00:00Z".parse().unwrap();
		store.flag("sha256:a", first).unwrap();
		store.flag("sha256:a", "2026-09-28T13:00:00Z".parse().unwrap()).unwrap();
		assert_eq!(store.flagged().unwrap()["sha256:a"], first);
		store.unflag("sha256:a").unwrap();
		assert!(store.flagged().unwrap().is_empty());
	}

	fn deployed(image: &str, previous: Option<Version>) -> Deployed {
		Deployed {
			manifest: geo(),
			image: image.into(),
			previous,
			deployed_at: "2026-09-27T00:00:00Z".into(),
			held: false,
		}
	}

	#[test]
	fn an_app_keeps_the_version_before_it() {
		let directory = tempfile::tempdir().unwrap();
		let store = Store::open(directory.path()).unwrap();
		store.put_app(&deployed("sha256:a", None)).unwrap();
		let first = Version { manifest: geo(), image: "sha256:a".into() };
		store.put_app(&deployed("sha256:b", Some(first.clone()))).unwrap();
		let app = store.app("geo").unwrap().unwrap();
		assert_eq!(app.image, "sha256:b");
		assert_eq!(app.previous, Some(first));
	}

	#[test]
	fn a_removed_app_leaves_what_runs_and_keeps_its_history() {
		let directory = tempfile::tempdir().unwrap();
		let store = Store::open(directory.path()).unwrap();
		store.put_app(&deployed("sha256:a", None)).unwrap();
		let (run, done) = (Source::run(7, None), Outcome::Succeeded);
		store.record("geo", Action::Deploy, &run, Some("sha256:a"), done, None).unwrap();
		assert!(store.forget_app("geo").unwrap());
		assert!(store.app("geo").unwrap().is_none());
		assert!(!store.forget_app("geo").unwrap());
		assert_eq!(store.events(Some("geo"), None, 10).unwrap().len(), 1);
	}

	#[test]
	fn a_label_is_an_app_or_a_route_and_never_both() {
		let directory = tempfile::tempdir().unwrap();
		let store = Store::open(directory.path()).unwrap();
		// geo declares no interface, so it takes no label at all: a route may share its name.
		store.put_app(&deployed("sha256:a", None)).unwrap();
		let same_name = Route {
			name: "geo".into(),
			upstream: "x.test:1".into(),
			private: true,
			public: true,
			home: None,
		};
		store.put_route(&same_name).unwrap();
		store.delete_route("geo").unwrap();

		// An app with an interface does take one, and a route cannot then share it.
		let mut interfaced = deployed("sha256:a", None);
		interfaced.manifest.interface =
			Some(deploy::manifest::Interface { domain: None, lan: true, home: None });
		store.put_app(&interfaced).unwrap();
		let route = Route {
			name: "geo".into(),
			upstream: "x.test:1".into(),
			private: true,
			public: true,
			home: None,
		};
		assert!(matches!(store.put_route(&route), Err(Error::TakenByApp(_))));
		let nas = Route {
			name: "nas".into(),
			upstream: "nas.test:80".into(),
			private: false,
			public: true,
			home: None,
		};
		store.put_route(&nas).unwrap();
		assert_eq!(store.routes().unwrap(), vec![nas]);
		assert!(store.delete_route("nas").unwrap());
		assert!(!store.delete_route("nas").unwrap());
	}

	#[test]
	fn a_domain_that_is_already_a_route_or_another_apps_label_is_refused() {
		let directory = tempfile::tempdir().unwrap();
		let store = Store::open(directory.path()).unwrap();
		let route = Route {
			name: "capture".into(),
			upstream: "x.test:1".into(),
			private: true,
			public: true,
			home: None,
		};
		store.put_route(&route).unwrap();

		let labeled = |image: &str, domain: &str| {
			let mut app = deployed(image, None);
			app.manifest.name = image.trim_start_matches("sha256:").to_owned();
			app.manifest.interface =
				Some(deploy::manifest::Interface { domain: Some(domain.into()), lan: true, home: None });
			app
		};
		// The route's label is already taken.
		assert!(matches!(
			store.put_app(&labeled("sha256:a", "capture")),
			Err(Error::TakenByRoute(label)) if label == "capture"
		));

		// Two different apps cannot share a label either.
		store.put_app(&labeled("sha256:a", "shot")).unwrap();
		assert!(matches!(
			store.put_app(&labeled("sha256:b", "shot")),
			Err(Error::TakenByApp(label)) if label == "shot"
		));
		// Redeploying the same app under the label it already holds is not a collision with itself.
		let mut app = deployed("sha256:a", None);
		app.manifest.name = "a".into();
		app.manifest.interface =
			Some(deploy::manifest::Interface { domain: Some("shot".into()), lan: true, home: None });
		store.put_app(&app).unwrap();
	}
}

#[cfg(test)]
mod split {
	use super::*;

	#[test]
	fn a_single_file_from_before_the_split_is_read_into_three_and_set_aside() {
		let directory = tempfile::tempdir().unwrap();
		let legacy = directory.path().join("host.db");
		let old = Connection::open(&legacy).unwrap();
		let manifest = serde_json::to_string(
			&Manifest::parse(include_str!("../../../../libs/deploy/fixtures/geo.toml")).unwrap(),
		)
		.unwrap();
		old
			.execute_batch(
				"CREATE TABLE apps (name TEXT PRIMARY KEY, manifest TEXT NOT NULL, image TEXT NOT NULL,
				previous TEXT, deployed_at TEXT NOT NULL);
				CREATE TABLE routes (name TEXT PRIMARY KEY, upstream TEXT NOT NULL,
				private INTEGER NOT NULL, public INTEGER NOT NULL);
				INSERT INTO routes VALUES ('nas', 'nas.test:80', 0, 1);",
			)
			.unwrap();
		old
			.execute(
				"INSERT INTO apps VALUES ('geo', ?1, 'sha256:a', NULL, '2026-09-27T00:00:00Z')",
				[manifest],
			)
			.unwrap();
		drop(old);

		let store = Store::open(directory.path()).unwrap();
		assert_eq!(store.routes().unwrap()[0].home, None);
		let geo = store.app("geo").unwrap().unwrap();
		assert_eq!((geo.image.as_str(), geo.held), ("sha256:a", false));
		assert!(!legacy.exists());
		assert!(directory.path().join("host.db.split").exists());
		// Opened again, it reads the split files and leaves the set-aside one alone.
		assert_eq!(Store::open(directory.path()).unwrap().routes().unwrap().len(), 1);
	}

	#[test]
	fn a_history_from_before_stages_reads_each_row_as_the_step_it_ended_at() {
		let directory = tempfile::tempdir().unwrap();
		let old = Connection::open(directory.path().join("history.db")).unwrap();
		old
			.execute_batch(
				"CREATE TABLE events (id INTEGER PRIMARY KEY AUTOINCREMENT, app TEXT NOT NULL,
				action TEXT NOT NULL, source TEXT NOT NULL, image TEXT, snapshot TEXT,
				outcome TEXT NOT NULL, detail TEXT, started_at TEXT NOT NULL, finished_at TEXT);
				INSERT INTO events (app, action, source, outcome, started_at) VALUES
				('geo', 'deploy', '{\"kind\":\"run\",\"run\":7}', 'succeeded', 't'),
				('geo', 'deploy', '{\"kind\":\"run\",\"run\":8}', 'skipped', 't'),
				('geo', 'rollback', '{\"kind\":\"panel\"}', 'failed', 't'),
				('geo', 'stop', '{\"kind\":\"panel\"}', 'succeeded', 't');",
			)
			.unwrap();
		drop(old);

		let stages = || -> Vec<Option<Stage>> {
			let store = Store::open(directory.path()).unwrap();
			store.events(None, None, 50).unwrap().into_iter().rev().map(|event| event.stage).collect()
		};
		let expected = [Some(Stage::Starting), Some(Stage::Admitting), Some(Stage::Starting), None];
		assert_eq!(stages(), expected);
		// Opened again, the column is there and nothing is added twice.
		assert_eq!(stages(), expected);
	}
}

#[cfg(test)]
mod history {
	use super::*;

	#[test]
	fn events_page_backwards_from_the_newest() {
		let directory = tempfile::tempdir().unwrap();
		let store = Store::open(directory.path()).unwrap();
		let ids: Vec<i64> = (0..5)
			.map(|_| {
				let running = Outcome::Running;
				store.record("geo", Action::Deploy, &Source::panel(), None, running, None).unwrap()
			})
			.collect();
		store.record("nas", Action::Stop, &Source::panel(), None, Outcome::Succeeded, None).unwrap();
		let first = store.events(Some("geo"), None, 2).unwrap();
		assert_eq!(first.iter().map(|event| event.id).collect::<Vec<_>>(), [ids[4], ids[3]]);
		let next = store.events(Some("geo"), Some(ids[3]), 2).unwrap();
		assert_eq!(next.iter().map(|event| event.id).collect::<Vec<_>>(), [ids[2], ids[1]]);
		assert_eq!(store.events(None, None, 50).unwrap().len(), 6);
	}

	#[test]
	fn an_event_closes_with_its_outcome_and_the_snapshot_a_rollback_would_restore() {
		let directory = tempfile::tempdir().unwrap();
		let store = Store::open(directory.path()).unwrap();
		let source = Source::run(7, Some("abc".into()));
		let starting = Some(Stage::Starting);
		let image = Some("sha256:b");
		let id = store.record("geo", Action::Deploy, &source, image, Outcome::Running, starting);
		let id = id.unwrap();
		assert_eq!(store.snapshot_before("geo", "sha256:b").unwrap(), None);
		store.finish(id, Outcome::Succeeded, Some("/snapshots/geo/1"), None).unwrap();
		let failed = store.record("geo", Action::Deploy, &source, image, Outcome::Running, starting);
		let failed = failed.unwrap();
		store.finish(failed, Outcome::Failed, Some("/snapshots/geo/2"), Some("unhealthy")).unwrap();
		// A redeploy of the same image is not what first ran it.
		let again = store.record("geo", Action::Redeploy, &source, image, Outcome::Running, starting);
		store.finish(again.unwrap(), Outcome::Succeeded, Some("/snapshots/geo/3"), None).unwrap();
		let before = store.snapshot_before("geo", "sha256:b").unwrap();
		assert_eq!(before.as_deref(), Some("/snapshots/geo/1"));
		assert_eq!(store.snapshot_before("geo", "sha256:c").unwrap(), None);
		let [_, newest, oldest] = store.events(Some("geo"), None, 50).unwrap().try_into().unwrap();
		assert_eq!((newest.outcome, newest.detail.as_deref()), (Outcome::Failed, Some("unhealthy")));
		assert_eq!((oldest.source, oldest.outcome), (source, Outcome::Succeeded));
		assert!(oldest.finished_at.is_some());
	}

	#[test]
	fn a_deploy_moves_through_its_stages_and_names_its_image_once_loaded() {
		let directory = tempfile::tempdir().unwrap();
		let store = Store::open(directory.path()).unwrap();
		let source = Source::run(7, Some("abc".into()));
		let id = store.record("geo", Action::Deploy, &source, None, Outcome::Running, None).unwrap();
		store.advance(id, Stage::Loading, None).unwrap();
		store.advance(id, Stage::Starting, Some("sha256:b")).unwrap();
		let [event] = store.events(None, None, 50).unwrap().try_into().unwrap();
		assert_eq!((event.stage, event.image.as_deref()), (Some(Stage::Starting), Some("sha256:b")));
		let json = serde_json::to_value(&event).unwrap();
		assert_eq!((&json["stage"], &json["source"]["run"]), (&"starting".into(), &7.into()));
	}

	#[test]
	fn a_note_is_kept_by_a_success_and_replaced_by_a_failure() {
		let directory = tempfile::tempdir().unwrap();
		let store = Store::open(directory.path()).unwrap();
		let (panel, starting) = (Source::panel(), Some(Stage::Starting));
		let open = || store.record("geo", Action::Deploy, &panel, None, Outcome::Running, starting);
		let (kept, replaced) = (open().unwrap(), open().unwrap());
		for (id, stage) in [(kept, Stage::Checking), (replaced, Stage::Switching)] {
			store.note(id, "replaced: 300 MiB available").unwrap();
			store.advance(id, stage, None).unwrap();
		}
		store.finish(kept, Outcome::Succeeded, None, None).unwrap();
		store.finish(replaced, Outcome::Failed, None, Some("not healthy")).unwrap();
		let [failed, succeeded] = store.events(None, None, 50).unwrap().try_into().unwrap();
		assert_eq!(succeeded.detail.as_deref(), Some("replaced: 300 MiB available"));
		assert_eq!(succeeded.stage, Some(Stage::Checking));
		assert_eq!(
			(failed.detail.as_deref(), failed.stage),
			(Some("not healthy"), Some(Stage::Switching))
		);
		let json = serde_json::to_value(&failed).unwrap();
		assert_eq!(json["stage"], "switching");
	}

	#[test]
	fn what_was_still_running_when_host_stopped_is_closed_failed_and_the_rest_left_alone() {
		let directory = tempfile::tempdir().unwrap();
		let store = Store::open(directory.path()).unwrap();
		let panel = Source::panel();
		let loading = Some(Stage::Loading);
		let cut = store.record("geo", Action::Deploy, &panel, None, Outcome::Running, loading);
		let cut = cut.unwrap();
		let done = store.record("geo", Action::Stop, &panel, None, Outcome::Running, None).unwrap();
		store.finish(done, Outcome::Succeeded, None, None).unwrap();
		assert_eq!(store.sweep().unwrap(), 1);
		assert_eq!(store.sweep().unwrap(), 0);
		let [stop, deploy] = store.events(None, None, 50).unwrap().try_into().unwrap();
		assert_eq!((deploy.id, deploy.outcome, deploy.stage), (cut, Outcome::Failed, loading));
		assert_eq!(deploy.detail.as_deref(), Some(RESTARTED));
		assert!(deploy.finished_at.is_some());
		assert_eq!((stop.outcome, stop.detail), (Outcome::Succeeded, None));
	}

	#[test]
	fn a_hold_is_kept_until_released_and_survives_a_new_version() {
		let directory = tempfile::tempdir().unwrap();
		let store = Store::open(directory.path()).unwrap();
		let manifest =
			Manifest::parse(include_str!("../../../../libs/deploy/fixtures/geo.toml")).unwrap();
		let app = |image: &str| Deployed {
			manifest: manifest.clone(),
			image: image.into(),
			previous: None,
			deployed_at: String::new(),
			held: false,
		};
		store.put_app(&app("sha256:a")).unwrap();
		store.hold("geo", true).unwrap();
		store.put_app(&app("sha256:b")).unwrap();
		assert!(store.app("geo").unwrap().unwrap().held);
		store.hold("geo", false).unwrap();
		assert!(!store.app("geo").unwrap().unwrap().held);
	}
}
