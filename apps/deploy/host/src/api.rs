//! host's HTTP surface. One token admits everything but `/health`, on the LAN as much as through
//! the tunnel -- see spec/architecture/host.md, "One token, behind two doors" -- and a second,
//! optional one admits reading alone.

use crate::Host;
use crate::environment;
use crate::images;
use crate::inspect;
use crate::node;
use crate::rollout::{self, Error as DeployError};
use crate::store::{Action, Deployed, Route, Source, Store};
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, RawQuery, Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use deploy::Manifest;
use deploy::manifest::{self, Rollout, is_home};
use deploy::replace::Error as Failed;
use serde::Deserialize;
use std::sync::Arc;

pub fn router(host: Arc<Host>) -> Router {
	let guarded = Router::new()
		.route("/apps", get(apps))
		.route("/apps/{name}", get(app).post(upload).delete(remove).layer(DefaultBodyLimit::disable()))
		.route("/apps/{name}/history", get(history).post(keeper_report))
		.route("/apps/{name}/health", get(app_health))
		.route("/events", get(events))
		.route("/runs/{owner}/{name}/{run}", get(run_verdict))
		.route("/apps/{name}/redeploy", post(redeploy))
		.route("/apps/{name}/rollback", post(rollback))
		.route("/apps/{name}/start", post(start))
		.route("/apps/{name}/stop", post(stop))
		.route("/apps/{name}/restart", post(restart))
		.route("/apps/{name}/logs", get(logs))
		.route("/apps/{name}/logs/archive", get(archived))
		.route("/apps/{name}/logs/archive/{file}", get(archived_file))
		.route("/apps/{name}/environment", get(environment))
		.route("/apps/{name}/environment/{kind}/{key}", put(set_variable).delete(unset_variable))
		.route("/images", get(images))
		.route("/images/collect", post(collect_images))
		.route("/images/scan", post(scan_images))
		.route("/images/{id}", axum::routing::delete(remove_image))
		.route("/routes", get(routes))
		.route("/routes/{name}", put(put_route).delete(delete_route))
		.route("/caddy", get(caddy).post(reapply))
		.route("/node/now", get(node_now))
		.route("/node/series", get(node_series))
		.route("/apps/{name}/metrics/now", get(app_now))
		.route("/apps/{name}/metrics/series", get(app_series))
		// Read-only, and never a command; see spec/architecture/inspect.md.
		.route("/inspect/containers", get(inspect::containers::list))
		.route("/inspect/networks", get(inspect::networks::list))
		.route("/inspect/disk", get(inspect::disk::get))
		.route("/inspect/files/{app}", get(inspect::files::root))
		.route("/inspect/files/{app}/{*path}", get(inspect::files::nested))
		.route("/inspect/kernel", get(inspect::kernel::get))
		.layer(middleware::from_fn_with_state(host.clone(), admit));
	// Everything is under `/api`, and `/notice` beside it; `/health` is keeper's. Caddy passes on
	// the notice and the reads alone, and the rest is asked over the tailnet. See
	// spec/architecture/host.md, "host has no interface on the node, and a door Caddy keeps".
	let api = Router::new()
		.merge(guarded)
		.fallback(|| async { response::failure(StatusCode::NOT_FOUND, "no_such_route") });
	Router::new()
		.route("/health", get(health))
		.route("/notice", post(notice))
		.nest("/api", api)
		.fallback(|| async { response::failure(StatusCode::NOT_FOUND, "no_such_route") })
		.with_state(host)
}

/// What keeper asks before it lets a new host stay: that it reads its own state and reaches
/// Docker, which are what every other request needs. Open, since it says nothing about either.
async fn health(State(host): State<Arc<Host>>) -> Response {
	if host.store.apps().is_err() {
		return response::failure(StatusCode::SERVICE_UNAVAILABLE, "store_unavailable");
	}
	if host.engine.ping().await.is_err() {
		return response::failure(StatusCode::SERVICE_UNAVAILABLE, "docker_unavailable");
	}
	response::success(StatusCode::OK, ())
}

#[derive(Deserialize)]
struct Notice {
	run: u64,
	/// Whose run, as `owner/name`. A notice from before they named one means the one source.
	#[serde(default)]
	repository: Option<String>,
	/// Set by keeper when it passes a run on, having dealt with the host that run built. Also read
	/// under the name a keeper built before the rename sends.
	#[serde(default, alias = "host_done")]
	host_replaced: bool,
	/// The one app of the run to deploy, sent by the operator: an app rolled out by hand is deployed
	/// this way and no other. See rollout/run.rs.
	#[serde(default)]
	app: Option<String>,
	/// Sent by `mise run node deploy`: deployed now, without waiting for the canary, and a notice
	/// of the same run still waiting gives up. See deploy::canary.
	#[serde(default)]
	by_hand: bool,
	/// What keeper did with the host the run built, said when it passes the run on: the canary's
	/// record of host for its verdict.
	#[serde(default)]
	host: Option<Replaced>,
}

/// What keeper did with a run's host.
#[derive(Deserialize)]
struct Replaced {
	outcome: crate::store::Outcome,
	#[serde(default)]
	detail: Option<String>,
}

/// A CI run has finished. Open, since it can only ask host to look: the run is checked against
/// GitHub before anything is fetched. See spec/architecture/host.md, "The machine pulls; nothing
/// pushes into it". Each run is taken once, and again only if taking it failed.
async fn notice(State(host): State<Arc<Host>>, Json(notice): Json<Notice>) -> Response {
	let Some(github) = host.github.as_ref() else {
		return response::failure(StatusCode::SERVICE_UNAVAILABLE, "github_unavailable");
	};
	let repository = notice.repository.or_else(|| github.only_source().map(str::to_owned));
	let Some(repository) = repository else {
		return response::failure(StatusCode::BAD_REQUEST, "invalid_repository");
	};
	if let Some(app) = notice.app.as_deref()
		&& let Err(error) = rollout::deployable(app)
	{
		return failed(StatusCode::UNPROCESSABLE_ENTITY, "invalid_name", error);
	}
	let key = (repository.clone(), notice.run, notice.app.clone(), notice.by_hand);
	let fresh =
		host.notices.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(key.clone());
	let answer =
		serde_json::json!({ "run": notice.run, "repository": repository, "app": notice.app });
	if !fresh {
		return response::success(StatusCode::OK, answer);
	}
	if notice.by_hand {
		let mut released = host.released.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		released.insert((repository.clone(), notice.run));
	}
	if let Some(replaced) = notice.host.as_ref().filter(|_| notice.host_replaced) {
		record_host(&host.store, &repository, notice.run, replaced);
	}
	let taker = host.clone();
	tokio::spawn(async move {
		let (run, replaced, only) = (notice.run, notice.host_replaced, notice.app.as_deref());
		if !rollout::from_run(taker.clone(), &repository, run, replaced, only, notice.by_hand).await {
			let mut notices = taker.notices.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
			notices.remove(&key);
		}
	});
	response::success(StatusCode::ACCEPTED, answer)
}

/// Record keeper's replacement of host by `run`, as host's own history holds any app's deploy.
fn record_host(store: &Store, repository: &str, run: u64, replaced: &Replaced) {
	let source = Source::run(run, None).of(repository);
	let starting = Some(crate::store::Stage::Starting);
	let opened = store.record("host", Action::Deploy, &source, None, replaced.outcome, starting);
	let recorded =
		opened.and_then(|id| store.finish(id, replaced.outcome, None, replaced.detail.as_deref()));
	if let Err(error) = recorded {
		eprintln!("host: recording run {run}'s host: {error}");
	}
}

/// Compared in time independent of where the first difference is.
fn same(given: &[u8], expected: &[u8]) -> bool {
	given.len() == expected.len()
		&& given.iter().zip(expected).fold(0, |acc, (a, b)| acc | (a ^ b)) == 0
}

/// The token in a request's `Authorization` header.
fn bearer(headers: &header::HeaderMap) -> Option<&str> {
	headers
		.get(header::AUTHORIZATION)
		.and_then(|value| value.to_str().ok())
		.and_then(|value| value.strip_prefix("Bearer "))
}

/// Why a request's token does not admit it.
#[derive(Debug, PartialEq)]
enum Refusal {
	/// No token, or one host does not know.
	Unknown,
	/// The read token, on a request that would act.
	ReadOnly,
}

/// Whether a request is admitted: anything with the token, and a GET with the read token. Both come
/// in the `Authorization` header and nowhere else.
fn admission(
	headers: &header::HeaderMap,
	method: &Method,
	token: &str,
	read_token: Option<&str>,
) -> Result<(), Refusal> {
	let given = bearer(headers).unwrap_or_default();
	if same(given.as_bytes(), token.as_bytes()) {
		return Ok(());
	}
	let read = read_token
		.zip(bearer(headers))
		.is_some_and(|(expected, given)| same(given.as_bytes(), expected.as_bytes()));
	match (read, method == Method::GET) {
		(false, _) => Err(Refusal::Unknown),
		(true, true) => Ok(()),
		(true, false) => Err(Refusal::ReadOnly),
	}
}

async fn admit(State(host): State<Arc<Host>>, request: Request, next: Next) -> Response {
	let config = &host.config;
	let admitted =
		admission(request.headers(), request.method(), &config.token, config.read_token.as_deref());
	match admitted {
		Ok(()) => next.run(request).await,
		Err(Refusal::Unknown) => response::failure(StatusCode::UNAUTHORIZED, "invalid_token"),
		Err(Refusal::ReadOnly) => {
			failed(StatusCode::FORBIDDEN, "invalid_token", "This token reads and does not act")
		}
	}
}

/// host is reached from the LAN and the tailnet alone, so a refusal may say exactly what went
/// wrong; see platform's spec/architecture/services.md, "Every answer is one envelope".
fn failed(status: StatusCode, code: &str, error: impl ToString) -> Response {
	response::failure_with(status, code, error)
}

fn stored<T: serde::Serialize>(read: Result<T, impl ToString>) -> Response {
	match read {
		Ok(value) => response::success(StatusCode::OK, value),
		Err(error) => failed(StatusCode::INTERNAL_SERVER_ERROR, "store_unavailable", error),
	}
}

/// An app as the panel shows it: what the store holds, and what only Docker and the snapshots know.
#[derive(serde::Serialize)]
struct Shown {
	#[serde(flatten)]
	app: Deployed,
	running: bool,
	/// Whether a rollback with data can be offered.
	restorable: bool,
	/// One of the platform's own four, which the panel restarts and never stops.
	platform: bool,
	/// A driver, which has no container of its own for the panel to act on.
	driver: bool,
	/// How a new version takes its place, `replace` when its declaration says nothing.
	rollout: Rollout,
	/// The architecture its image runs as: the one it asks for, or the node's own.
	arch: Option<String>,
	/// Whether that is not the node's own, so it runs emulated.
	emulated: bool,
}

async fn shown(host: &Host, app: Deployed) -> Shown {
	let running = host.engine.running(&app.manifest.name).await.unwrap_or(false);
	let restorable = rollout::restorable(host, &app).ok().flatten().is_some();
	let platform = rollout::PLATFORM.contains(&app.manifest.name.as_str());
	let driver = host.config.grants.driver_of(&app.manifest).is_some();
	let rollout = app.manifest.rollout;
	let native = host.config.native;
	let arch = app.manifest.arch.clone().or_else(|| native.map(str::to_owned));
	let emulated = arch.as_deref() != native;
	Shown { app, running, restorable, platform, driver, rollout, arch, emulated }
}

/// Every app the store holds, and host itself among them, read from its container.
async fn apps(State(host): State<Arc<Host>>) -> Response {
	let mut apps = match host.store.apps() {
		Ok(apps) => apps,
		Err(error) => return failed(StatusCode::INTERNAL_SERVER_ERROR, "store_unavailable", error),
	};
	match rollout::itself(&host).await {
		Ok(itself) => apps.extend(itself),
		Err(error) => eprintln!("host: reading its own container: {error}"),
	}
	apps.sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
	let mut all = Vec::with_capacity(apps.len());
	for app in apps {
		all.push(shown(&host, app).await);
	}
	response::success(StatusCode::OK, all)
}

async fn app(State(host): State<Arc<Host>>, Path(name): Path<String>) -> Response {
	if name == "host" {
		return match rollout::itself(&host).await {
			Ok(Some(app)) => response::success(StatusCode::OK, shown(&host, app).await),
			Ok(None) => response::failure(StatusCode::NOT_FOUND, "no_such_app"),
			Err(error) => refused(error),
		};
	}
	match host.store.app(&name) {
		Ok(Some(app)) => response::success(StatusCode::OK, shown(&host, app).await),
		Ok(None) => response::failure(StatusCode::NOT_FOUND, "no_such_app"),
		Err(error) => failed(StatusCode::INTERNAL_SERVER_ERROR, "store_unavailable", error),
	}
}

#[derive(Deserialize)]
struct Built {
	/// The gated apps the run built, comma-separated.
	#[serde(default)]
	built: String,
}

/// The canary's verdict on a run, which every other node asks before it takes a new host, keeper
/// or Caddy. Answered on the canary alone. See canary.rs and deploy::canary.
async fn run_verdict(
	State(host): State<Arc<Host>>,
	Path((owner, name, run)): Path<(String, String, u64)>,
	Query(asked): Query<Built>,
) -> Response {
	if host.config.canary != deploy::canary::Canary::Itself {
		return failed(StatusCode::NOT_FOUND, "no_such_route", "This node is not the canary");
	}
	let built: Vec<&str> = asked.built.split(',').filter(|app| !app.is_empty()).collect();
	if built.is_empty() || !built.iter().all(|app| deploy::canary::GATED.contains(app)) {
		let why = "`built` names one or more of host, keeper and caddy";
		return failed(StatusCode::BAD_REQUEST, "invalid_name", why);
	}
	let repository = format!("{owner}/{name}");
	stored(crate::canary::verdict(&host, &repository, run, &built).await)
}

/// How long an app has to answer when its health is asked from the console.
const HEALTH_WITHIN: std::time::Duration = std::time::Duration::from_secs(3);

/// What an app answered its own health path with, asked the moment the console asked.
#[derive(Debug, serde::Serialize)]
struct Health {
	app: String,
	/// Whether it answered at all, with any status.
	answered: bool,
	code: Option<u16>,
	/// Its answer as JSON, or null when it was not JSON or there was none.
	body: Option<serde_json::Value>,
	checked_at: String,
	/// Why there was no answer.
	#[serde(skip_serializing_if = "Option::is_none")]
	error: Option<String>,
}

/// Ask `app` at `asked` for `path`, once.
async fn ask_health(app: &str, asked: Option<deploy::replace::Asked>, path: &str) -> Health {
	let answer = match asked {
		Some(asked) => asked.get(path, HEALTH_WITHIN).await.map_err(|error| error.to_string()),
		None => Err("it runs no container of its own to ask".to_owned()),
	};
	let checked_at = jiff::Timestamp::now().to_string();
	match answer {
		Ok((code, body)) => Health {
			app: app.to_owned(),
			answered: true,
			code: Some(code),
			body: serde_json::from_str(&body).ok(),
			checked_at,
			error: None,
		},
		Err(error) => Health {
			app: app.to_owned(),
			answered: false,
			code: None,
			body: None,
			checked_at,
			error: Some(error),
		},
	}
}

/// The app's health now, asked exactly as its deploy's check asks it: on its own network or its
/// socket. Whatever it answers, the answer is a success; that it did not answer is in the data.
async fn app_health(State(host): State<Arc<Host>>, Path(name): Path<String>) -> Response {
	let deployed = match name.as_str() {
		"host" => rollout::itself(&host).await.map_err(|error| error.to_string()),
		_ => host.store.app(&name).map_err(|error| error.to_string()),
	};
	let manifest = match deployed {
		Ok(Some(app)) => app.manifest,
		Ok(None) => return response::failure(StatusCode::NOT_FOUND, "no_such_app"),
		Err(error) => return failed(StatusCode::INTERNAL_SERVER_ERROR, "store_unavailable", error),
	};
	let path = manifest.container.as_ref().map_or("/health", |container| &container.health);
	let health = ask_health(&name, rollout::asked(&host, &manifest), path).await;
	response::success(StatusCode::OK, health)
}

#[derive(Deserialize)]
struct Page {
	before: Option<i64>,
	limit: Option<u32>,
}

/// How many events a page holds when the panel names no number, and the most it may ask for.
const EVENTS: u32 = 50;
const MOST_EVENTS: u32 = 500;

/// Events, the newest first, a page at a time: the next page is the one `before` the last id of
/// this one. One app's, or every app's when `app` is none.
fn paged(store: &Store, app: Option<&str>, page: &Page) -> Response {
	let limit = page.limit.unwrap_or(EVENTS).clamp(1, MOST_EVENTS);
	stored(store.events(app, page.before, limit))
}

async fn history(
	State(host): State<Arc<Host>>,
	Path(name): Path<String>,
	Query(page): Query<Page>,
) -> Response {
	paged(&host.store, Some(&name), &page)
}

/// What keeper did to host, as keeper reports it once host answers again.
#[derive(Deserialize)]
struct Report {
	action: Action,
	outcome: crate::store::Outcome,
	#[serde(default)]
	image: Option<String>,
	#[serde(default)]
	detail: Option<String>,
	started_at: String,
	finished_at: String,
}

/// keeper's record of recreating host, or of taking an upload of it: a finished row in host's own
/// history, since host was down while it happened. The full token alone, as any write; host's own
/// rows alone. See keeper's `report`.
async fn keeper_report(
	State(host): State<Arc<Host>>,
	Path(name): Path<String>,
	Json(report): Json<Report>,
) -> Response {
	if name != "host" {
		return failed(StatusCode::FORBIDDEN, "invalid_target", "keeper records host's events alone");
	}
	match reported(&host.store, report) {
		Ok(id) => response::success(StatusCode::OK, serde_json::json!({ "id": id })),
		Err(Reported::Invalid(why)) => failed(StatusCode::BAD_REQUEST, "invalid_body", why),
		Err(Reported::Store(error)) => {
			failed(StatusCode::INTERNAL_SERVER_ERROR, "store_unavailable", error)
		}
	}
}

enum Reported {
	Invalid(&'static str),
	Store(crate::store::Error),
}

/// `report` recorded, when it is a deploy or a redeploy that ended, with times that are times.
fn reported(store: &Store, report: Report) -> Result<i64, Reported> {
	use crate::store::Outcome;
	if !matches!(report.action, Action::Deploy | Action::Redeploy) {
		return Err(Reported::Invalid("keeper reports a deploy or a redeploy of host"));
	}
	if !matches!(report.outcome, Outcome::Succeeded | Outcome::Failed) {
		return Err(Reported::Invalid("keeper reports what ended, succeeded or failed"));
	}
	let times = [&report.started_at, &report.finished_at];
	if times.iter().any(|time| time.parse::<jiff::Timestamp>().is_err()) {
		return Err(Reported::Invalid("started_at and finished_at are RFC 3339 times"));
	}
	let event = crate::store::Finished {
		app: "host".into(),
		action: report.action,
		source: Source::keeper(),
		image: report.image,
		outcome: report.outcome,
		detail: report.detail,
		started_at: report.started_at,
		finished_at: report.finished_at,
	};
	store.recorded(&event).map_err(Reported::Store)
}

/// Every app's events on this node.
async fn events(State(host): State<Arc<Host>>, Query(page): Query<Page>) -> Response {
	paged(&host.store, None, &page)
}

/// A panel action's refusal, by what went wrong.
fn refused(error: DeployError) -> Response {
	let (status, code) = match &error {
		DeployError::Itself | DeployError::Platform(_) | DeployError::Kept(_) => {
			(StatusCode::FORBIDDEN, "invalid_target")
		}
		DeployError::NoSuchApp(_) => (StatusCode::NOT_FOUND, "no_such_app"),
		DeployError::NoPrevious(_) => (StatusCode::CONFLICT, "no_such_version"),
		DeployError::NoSnapshot(_) => (StatusCode::CONFLICT, "no_such_snapshot"),
		DeployError::Replace(
			Failed::Unhealthy { .. } | Failed::FirstFailed { .. } | Failed::Beside { .. },
		) => (StatusCode::BAD_GATEWAY, "app_unavailable"),
		DeployError::Engine(_) | DeployError::Replace(_) => {
			(StatusCode::BAD_GATEWAY, "docker_unavailable")
		}
		DeployError::Store(_) => (StatusCode::INTERNAL_SERVER_ERROR, "store_unavailable"),
		DeployError::Unrunnable { .. } => (StatusCode::UNPROCESSABLE_ENTITY, "invalid_declaration"),
		_ => (StatusCode::INTERNAL_SERVER_ERROR, "service_unavailable"),
	};
	failed(status, code, error)
}

fn done(result: Result<impl serde::Serialize, DeployError>) -> Response {
	match result {
		Ok(value) => response::success(StatusCode::OK, value),
		Err(error) => refused(error),
	}
}

async fn redeploy(State(host): State<Arc<Host>>, Path(name): Path<String>) -> Response {
	done(rollout::redeploy(&host, &name).await)
}

#[derive(Deserialize)]
struct Rollback {
	#[serde(default)]
	with_data: bool,
}

async fn rollback(
	State(host): State<Arc<Host>>,
	Path(name): Path<String>,
	Json(asked): Json<Rollback>,
) -> Response {
	done(rollout::rollback(&host, &name, asked.with_data).await)
}

#[derive(Deserialize)]
struct Removal {
	/// `drop` deletes the app's directory and its snapshots too; anything else keeps them.
	#[serde(default)]
	data: Option<String>,
}

/// Take an app off the node, keeping its data unless `?data=drop`. See rollout/remove.rs.
async fn remove(
	State(host): State<Arc<Host>>,
	Path(name): Path<String>,
	Query(asked): Query<Removal>,
) -> Response {
	done(rollout::remove(&host, &name, asked.data.as_deref() == Some("drop")).await)
}

async fn start(State(host): State<Arc<Host>>, Path(name): Path<String>) -> Response {
	done(rollout::act(&host, &name, Action::Start).await)
}

async fn stop(State(host): State<Arc<Host>>, Path(name): Path<String>) -> Response {
	done(rollout::act(&host, &name, Action::Stop).await)
}

async fn restart(State(host): State<Arc<Host>>, Path(name): Path<String>) -> Response {
	done(rollout::act(&host, &name, Action::Restart).await)
}

/// A deploy: the declaration as the part `service`, then the image archive as the part `image`.
/// The archive is written to disk before anything is stopped, so a transfer cut short never
/// leaves the app down.
async fn upload(
	State(host): State<Arc<Host>>,
	Path(name): Path<String>,
	mut parts: Multipart,
) -> Response {
	if let Err(error) = rollout::deployable(&name) {
		return failed(StatusCode::UNPROCESSABLE_ENTITY, "invalid_name", error);
	}
	let mut declared: Option<Manifest> = None;
	let archive = deploy::arrival(&host.config.incoming);
	let mut received = false;
	loop {
		let part = match parts.next_field().await {
			Ok(Some(part)) => part,
			Ok(None) => break,
			Err(error) => return failed(StatusCode::BAD_REQUEST, "invalid_upload", error),
		};
		match part.name() {
			Some("service") => {
				let text = match part.text().await {
					Ok(text) => text,
					Err(error) => return failed(StatusCode::BAD_REQUEST, "invalid_upload", error),
				};
				match Manifest::parse(&text) {
					Ok(manifest) => declared = Some(manifest),
					Err(error) => {
						return failed(StatusCode::UNPROCESSABLE_ENTITY, "invalid_declaration", error);
					}
				}
			}
			Some("image") => {
				if let Err(error) = save(&archive, part).await {
					return failed(StatusCode::BAD_REQUEST, "invalid_upload", error);
				}
				received = true;
			}
			_ => {}
		}
	}
	let Some(manifest) = declared else {
		return failed(StatusCode::BAD_REQUEST, "invalid_upload", "No service part");
	};
	if !received {
		return failed(StatusCode::BAD_REQUEST, "invalid_upload", "No image part");
	}
	match rollout::from_archive(&host, &name, manifest, &archive, &Source::upload()).await {
		Ok(outcome) => response::success(StatusCode::OK, outcome),
		Err(error @ (DeployError::Invalid(_) | DeployError::Unrunnable { .. })) => {
			failed(StatusCode::UNPROCESSABLE_ENTITY, "invalid_declaration", error)
		}
		Err(error @ DeployError::PortTaken { .. }) => {
			failed(StatusCode::CONFLICT, "invalid_port", error)
		}
		Err(error @ DeployError::Load(_)) => {
			failed(StatusCode::UNPROCESSABLE_ENTITY, "invalid_image", error)
		}
		Err(
			error @ DeployError::Replace(
				Failed::Unhealthy { .. } | Failed::FirstFailed { .. } | Failed::Beside { .. },
			),
		) => failed(StatusCode::BAD_GATEWAY, "app_unavailable", error),
		Err(error) => failed(StatusCode::INTERNAL_SERVER_ERROR, "service_unavailable", error),
	}
}

async fn save(
	path: &std::path::Path,
	mut part: axum::extract::multipart::Field<'_>,
) -> anyhow::Result<()> {
	if let Some(parent) = path.parent() {
		tokio::fs::create_dir_all(parent).await?;
	}
	let mut file = deploy::uncached::AsyncWriter::create(path).await?;
	while let Some(chunk) = part.chunk().await? {
		file.write(&chunk).await?;
	}
	file.finish().await?;
	Ok(())
}

#[derive(Deserialize)]
struct Lines {
	lines: Option<u32>,
}

/// How many of the running container's lines are sent when the panel names no number, and the
/// most it may ask for; older lines are in the archive.
const LINES: u32 = 500;
const MOST_LINES: u32 = 10_000;

async fn logs(
	State(host): State<Arc<Host>>,
	Path(name): Path<String>,
	Query(asked): Query<Lines>,
) -> Response {
	if let Some(refused) = refusal(&host, &name) {
		return refused;
	}
	let count = asked.lines.unwrap_or(LINES).clamp(1, MOST_LINES);
	match host.engine.lines(&name, count).await {
		Ok(lines) => response::success(StatusCode::OK, serde_json::json!({ "lines": lines })),
		Err(error) => failed(StatusCode::BAD_GATEWAY, "docker_unavailable", error),
	}
}

#[derive(serde::Serialize)]
struct Archived {
	file: String,
	bytes: u64,
}

/// Every archived log of the app, the newest first.
async fn archived(State(host): State<Arc<Host>>, Path(name): Path<String>) -> Response {
	if let Some(refused) = refusal(&host, &name) {
		return refused;
	}
	let directory = host.volumes.logs(&name);
	let mut files = Vec::new();
	if let Ok(mut entries) = tokio::fs::read_dir(&directory).await {
		while let Ok(Some(entry)) = entries.next_entry().await {
			let file = entry.file_name().to_string_lossy().into_owned();
			let bytes = entry.metadata().await.map(|meta| meta.len()).unwrap_or_default();
			if archive_name(&file) {
				files.push(Archived { file, bytes });
			}
		}
	}
	files.sort_by(|a, b| b.file.cmp(&a.file));
	response::success(StatusCode::OK, files)
}

/// One archived log, as the text it is.
async fn archived_file(
	State(host): State<Arc<Host>>,
	Path((name, file)): Path<(String, String)>,
) -> Response {
	if let Some(refused) = refusal(&host, &name) {
		return refused;
	}
	if !archive_name(&file) {
		return response::failure(StatusCode::NOT_FOUND, "no_such_object");
	}
	match tokio::fs::read(host.volumes.logs(&name).join(&file)).await {
		Ok(bytes) => ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], bytes).into_response(),
		Err(_) => response::failure(StatusCode::NOT_FOUND, "no_such_object"),
	}
}

/// A name `Engine::archive` gives, and so one that cannot reach outside the app's directory.
fn archive_name(file: &str) -> bool {
	file.ends_with(".log")
		&& file.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
		&& !file.starts_with('.')
}

/// The app's environment as the panel may see it: configuration in full, secrets by name.
async fn environment(State(host): State<Arc<Host>>, Path(name): Path<String>) -> Response {
	if let Some(refused) = refusal(&host, &name) {
		return refused;
	}
	match environment::shown(&host.volumes.root(&name)) {
		Ok(shown) => response::success(StatusCode::OK, shown),
		Err(error) => failed(StatusCode::INTERNAL_SERVER_ERROR, "store_unavailable", error),
	}
}

#[derive(Deserialize)]
struct Variable {
	value: String,
}

async fn set_variable(
	State(host): State<Arc<Host>>,
	Path((name, kind, key)): Path<(String, String, String)>,
	Json(variable): Json<Variable>,
) -> Response {
	change_variable(&host, &name, &kind, &key, Some(&variable.value))
}

async fn unset_variable(
	State(host): State<Arc<Host>>,
	Path((name, kind, key)): Path<(String, String, String)>,
) -> Response {
	change_variable(&host, &name, &kind, &key, None)
}

/// A change is written now and applies when the container is next started from its version; the
/// answer says whether anything changed, for the panel to offer that.
fn change_variable(
	host: &Host,
	name: &str,
	kind: &str,
	key: &str,
	value: Option<&str>,
) -> Response {
	if let Some(refused) = refusal(host, name) {
		return refused;
	}
	// host reads its `.env` beside the compose file, not the two files an app's environment is.
	if name == "host" {
		return failed(StatusCode::FORBIDDEN, "invalid_target", DeployError::Itself);
	}
	let Some(kind) = environment::Kind::from_segment(kind) else {
		return response::failure(StatusCode::NOT_FOUND, "no_such_route");
	};
	match environment::set(&host.volumes.root(name), kind, key, value) {
		Ok(changed) => response::success(StatusCode::OK, serde_json::json!({ "changed": changed })),
		Err(error @ (environment::Error::Name(_) | environment::Error::Value)) => {
			failed(StatusCode::UNPROCESSABLE_ENTITY, "invalid_variable", error)
		}
		Err(error) => failed(StatusCode::INTERNAL_SERVER_ERROR, "store_unavailable", error),
	}
}

/// The refusal to answer for an app, or none for one whose logs, history, environment and files
/// may be read: one the store holds, or host itself.
pub(crate) fn refusal(host: &Host, name: &str) -> Option<Response> {
	if name == "host" {
		return None;
	}
	match host.store.app(name) {
		Ok(Some(_)) => None,
		Ok(None) => Some(response::failure(StatusCode::NOT_FOUND, "no_such_app")),
		Err(error) => Some(failed(StatusCode::INTERNAL_SERVER_ERROR, "store_unavailable", error)),
	}
}

async fn routes(State(host): State<Arc<Host>>) -> Response {
	stored(host.store.routes())
}

#[derive(Deserialize)]
struct RouteBody {
	upstream: String,
	private: bool,
	public: bool,
	#[serde(default)]
	home: Option<String>,
}

async fn put_route(
	State(host): State<Arc<Host>>,
	Path(name): Path<String>,
	Json(body): Json<RouteBody>,
) -> Response {
	// A route's name is the label it answers on; see spec/architecture/host.md, "One name inside,
	// and a domain label outside".
	if let Err(error) = manifest::check_domain(&name) {
		return failed(StatusCode::UNPROCESSABLE_ENTITY, "invalid_name", error);
	}
	if body.home.as_deref().is_some_and(|home| !is_home(home)) {
		return response::failure(StatusCode::UNPROCESSABLE_ENTITY, "invalid_home");
	}
	// A route is always on `.app`; `public = false` is refused rather than silently ignored, so a
	// caller that meant to keep it off the tunnel is told rather than getting the opposite. See
	// spec/architecture/host.md, "One name inside, and a domain label outside".
	if !body.public {
		return response::failure(StatusCode::UNPROCESSABLE_ENTITY, "invalid_route");
	}
	let route = Route {
		name,
		upstream: body.upstream,
		private: body.private,
		public: body.public,
		home: body.home,
	};
	if let Err(error) = host.store.put_route(&route) {
		return failed(StatusCode::CONFLICT, "invalid_route", error);
	}
	routed(&host).await
}

async fn delete_route(State(host): State<Arc<Host>>, Path(name): Path<String>) -> Response {
	match host.store.delete_route(&name) {
		Ok(true) => routed(&host).await,
		Ok(false) => failed(StatusCode::NOT_FOUND, "no_such_route", "No route has this name"),
		Err(error) => failed(StatusCode::INTERNAL_SERVER_ERROR, "store_unavailable", error),
	}
}

/// The meter's socket, as the meter deployed on this node declares it; none without one.
fn meter_socket(host: &Host) -> Option<std::path::PathBuf> {
	let meter = host.store.apps().ok()?.into_iter().find(|app| app.manifest.name == "meter")?;
	Some(host.volumes.data("meter").join(meter.manifest.container?.socket?))
}

/// The machine as it is this second, with what does not change beside it.
async fn node_now(State(host): State<Arc<Host>>) -> Response {
	node::relay(meter_socket(&host).as_deref(), "/now").await
}

/// The machine over time, at the grain asked for; the query is the meter's to read.
async fn node_series(State(host): State<Arc<Host>>, RawQuery(query): RawQuery) -> Response {
	let path = query.map_or_else(|| "/series".to_owned(), |query| format!("/series?{query}"));
	node::relay(meter_socket(&host).as_deref(), &path).await
}

#[derive(Deserialize)]
struct Span {
	grain: Option<String>,
	since: Option<String>,
	until: Option<String>,
}

/// A container's name as the meter's metrics begin with it; anything else asks for nothing.
fn container_name(name: &str) -> Option<&str> {
	let fits = !name.is_empty()
		&& name.len() <= 128
		&& name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
	fits.then_some(name)
}

/// One container now, as the meter last sampled it: `<name>.cpu`, `.memory` and the rest.
async fn app_now(State(host): State<Arc<Host>>, Path(name): Path<String>) -> Response {
	let Some(name) = container_name(&name) else {
		return response::failure(StatusCode::NOT_FOUND, "no_such_app");
	};
	node::relay(meter_socket(&host).as_deref(), &format!("/containers/now?metrics={name}")).await
}

/// One container over time, at the grain asked for.
async fn app_series(
	State(host): State<Arc<Host>>,
	Path(name): Path<String>,
	Query(span): Query<Span>,
) -> Response {
	let Some(name) = container_name(&name) else {
		return response::failure(StatusCode::NOT_FOUND, "no_such_app");
	};
	// The serializer is not `Send`, so it is done with before anything is awaited.
	let query = {
		let mut query = url::form_urlencoded::Serializer::new(String::new());
		query.append_pair("metrics", name);
		for (key, value) in [("grain", &span.grain), ("since", &span.since), ("until", &span.until)] {
			if let Some(value) = value {
				query.append_pair(key, value);
			}
		}
		query.finish()
	};
	let path = format!("/containers/series?{query}");
	node::relay(meter_socket(&host).as_deref(), &path).await
}

/// The images as the background last found them, and the tasks asked of it: answered from memory,
/// so the page never waits on Docker. Before the first scan there are no images to show yet.
async fn images(State(host): State<Arc<Host>>) -> Response {
	let body = serde_json::json!({
		"scan": host.images.scan(),
		"tasks": host.images.tasks(),
		"grace_seconds": images::GRACE.as_secs(),
	});
	response::success(StatusCode::OK, body)
}

/// Queue one image's removal; the task says how it went.
async fn remove_image(State(host): State<Arc<Host>>, Path(id): Path<String>) -> Response {
	response::success(StatusCode::ACCEPTED, host.images.ask(images::Kind::Remove { image: id }))
}

/// Queue removing every image nothing could run again.
async fn collect_images(State(host): State<Arc<Host>>) -> Response {
	response::success(StatusCode::ACCEPTED, host.images.ask(images::Kind::Collect))
}

/// Look at the images again now.
async fn scan_images(State(host): State<Arc<Host>>) -> Response {
	host.images.rescan();
	response::success(StatusCode::ACCEPTED, ())
}

/// What Caddy would be given now, without giving it. What to read before switching Caddy over.
async fn caddy(State(host): State<Arc<Host>>) -> Response {
	match rollout::render(&host) {
		Ok(rendered) => response::success(StatusCode::OK, rendered),
		Err(error) => failed(StatusCode::INTERNAL_SERVER_ERROR, "store_unavailable", error),
	}
}

async fn reapply(State(host): State<Arc<Host>>) -> Response {
	if let Err(error) = rollout::attach(&host).await {
		return failed(StatusCode::INTERNAL_SERVER_ERROR, "docker_unavailable", error);
	}
	routed(&host).await
}

/// The state changed; Caddy follows. Stored even when Caddy cannot be reached, so the answer says
/// which half happened.
async fn routed(host: &Host) -> Response {
	match rollout::route(host).await {
		Ok(()) => StatusCode::NO_CONTENT.into_response(),
		Err(error) => {
			let message = format!("Stored, but Caddy was not updated: {error}");
			failed(StatusCode::BAD_GATEWAY, "caddy_unavailable", message)
		}
	}
}

#[cfg(test)]
mod tests {
	#[test]
	fn a_home_stays_on_its_own_site() {
		use deploy::manifest::is_home;
		assert!(is_home("/admin"));
		assert!(is_home("/admin/"));
		assert!(!is_home("/"));
		assert!(!is_home("admin"));
		assert!(!is_home("//evil.example"));
		assert!(!is_home("/\\evil.example"));
		assert!(!is_home("/admin\r\nSet-Cookie: x=1"));
	}

	#[test]
	fn a_token_matches_only_itself() {
		assert!(super::same(b"secret", b"secret"));
		assert!(!super::same(b"secreT", b"secret"));
		assert!(!super::same(b"secret-and-more", b"secret"));
		assert!(!super::same(b"", b"secret"));
	}

	#[test]
	fn the_read_token_reads_from_the_header_alone_and_never_acts() {
		use super::{Refusal, admission};
		use axum::http::{HeaderMap, HeaderValue, Method, header};
		let with = |name, value: &'static str| {
			let mut headers = HeaderMap::new();
			headers.insert(name, HeaderValue::from_static(value));
			headers
		};
		let read = with(header::AUTHORIZATION, "Bearer reader");
		let admitted =
			|headers: &HeaderMap, method| admission(headers, &method, "full", Some("reader"));
		assert_eq!(admitted(&read, Method::GET), Ok(()));
		assert_eq!(admitted(&read, Method::POST), Err(Refusal::ReadOnly));
		assert_eq!(admitted(&read, Method::DELETE), Err(Refusal::ReadOnly));
		let full = with(header::AUTHORIZATION, "Bearer full");
		assert_eq!(admitted(&full, Method::POST), Ok(()));
		assert_eq!(admitted(&full, Method::DELETE), Ok(()));
		// A cookie is no token: the panel that signed in with one has retired.
		assert_eq!(
			admitted(&with(header::COOKIE, "host_token=full"), Method::GET),
			Err(Refusal::Unknown)
		);
		// Unset, there is no read token at all.
		assert_eq!(admission(&read, &Method::GET, "full", None), Err(Refusal::Unknown));
		assert_eq!(admission(&HeaderMap::new(), &Method::GET, "full", None), Err(Refusal::Unknown));
	}

	#[tokio::test]
	async fn every_apps_events_page_backwards_from_the_newest() {
		use super::{Page, paged};
		use crate::store::{Action, Outcome, Source, Stage, Store};
		use http_body_util::BodyExt;
		let directory = tempfile::tempdir().unwrap();
		let store = Store::open(directory.path()).unwrap();
		let run = Source::run(7, Some("abc".into()));
		let ids: Vec<i64> = ["geo", "nas", "geo", "cron"]
			.into_iter()
			.map(|app| {
				let (running, loading) = (Outcome::Running, Some(Stage::Loading));
				store.record(app, Action::Deploy, &run, None, running, loading).unwrap()
			})
			.collect();
		let read = async |page: Page| {
			let body = paged(&store, None, &page).into_body().collect().await.unwrap().to_bytes();
			serde_json::from_slice::<serde_json::Value>(&body).unwrap()["data"].clone()
		};
		let ids_of = |data: &serde_json::Value| -> Vec<i64> {
			data.as_array().unwrap().iter().map(|event| event["id"].as_i64().unwrap()).collect()
		};
		let first = read(Page { before: None, limit: Some(2) }).await;
		assert_eq!(ids_of(&first), [ids[3], ids[2]]);
		assert_eq!(first[0]["app"], "cron");
		assert_eq!((&first[0]["stage"], &first[0]["source"]["run"]), (&"loading".into(), &7.into()));
		assert_eq!(first[0]["source"]["commit"], "abc");
		let next = read(Page { before: Some(ids[2]), limit: Some(2) }).await;
		assert_eq!(ids_of(&next), [ids[1], ids[0]]);
		// No number is fifty, and none is less than one.
		assert_eq!(ids_of(&read(Page { before: None, limit: None }).await).len(), 4);
		assert_eq!(ids_of(&read(Page { before: None, limit: Some(0) }).await), [ids[3]]);
	}

	/// The envelope `path` is answered with by a host over `root`, asked with the read token.
	async fn read(host: std::sync::Arc<crate::Host>, path: &str) -> (u16, serde_json::Value) {
		use http_body_util::BodyExt;
		use tower::ServiceExt;
		let request = axum::http::Request::get(path)
			.header("authorization", "Bearer reader")
			.body(axum::body::Body::empty())
			.unwrap();
		let answer = super::router(host).oneshot(request).await.unwrap();
		let status = answer.status().as_u16();
		let body = answer.into_body().collect().await.unwrap().to_bytes();
		(status, serde_json::from_slice(&body).unwrap())
	}

	/// An app answering `/health` with `body` at a port on this machine, which is where it is asked.
	async fn answering(body: &'static str) -> String {
		let app = axum::Router::new().route("/health", axum::routing::get(move || async move { body }));
		let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let address = listener.local_addr().unwrap().to_string();
		tokio::spawn(async move { axum::serve(listener, app).await });
		address
	}

	#[tokio::test]
	async fn an_apps_health_is_its_answer_parsed_or_why_there_was_none() {
		use deploy::replace::Asked;
		let address = answering(r#"{"status":"success","data":{"ok":true}}"#).await;
		let health = super::ask_health("geo", Some(Asked::At(address)), "/health").await;
		let json = serde_json::to_value(&health).unwrap();
		assert_eq!(
			(&json["app"], &json["answered"], &json["code"]),
			(&"geo".into(), &true.into(), &200.into())
		);
		assert_eq!(json["body"]["data"]["ok"], true);
		assert!(json.get("error").is_none());
		assert!(json["checked_at"].as_str().unwrap().parse::<jiff::Timestamp>().is_ok());

		let plain = super::ask_health("geo", Some(Asked::At(answering("ok").await)), "/health").await;
		let json = serde_json::to_value(&plain).unwrap();
		assert_eq!((&json["answered"], &json["body"]), (&true.into(), &serde_json::Value::Null));

		// A port nothing listens on any more.
		let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let gone = closed.local_addr().unwrap().to_string();
		drop(closed);
		let silent = super::ask_health("geo", Some(Asked::At(gone)), "/health").await;
		let json = serde_json::to_value(&silent).unwrap();
		assert_eq!(json["answered"], false);
		assert_eq!(
			(&json["code"], &json["body"]),
			(&serde_json::Value::Null, &serde_json::Value::Null)
		);
		assert!(json["error"].as_str().is_some_and(|error| !error.is_empty()));
	}

	#[tokio::test]
	async fn an_apps_health_is_read_with_the_read_token_and_a_missing_app_is_not_found() {
		use crate::store::Deployed;
		let directory = tempfile::tempdir().unwrap();
		let host = crate::testing(directory.path());
		let (status, envelope) = read(host.clone(), "/api/apps/geo/health").await;
		assert_eq!((status, &envelope["status"]), (404, &"error".into()));
		assert_eq!(envelope["code"], "no_such_app");

		// An app on a socket in its own directory, asked there as its deploy's check asks it.
		let manifest = deploy::Manifest::parse(
			"version = 1\nname = \"probe\"\nplacements = [\"rdu\"]\n[container]\n\
			 socket = \"probe.sock\"\nhealth = \"/health\"\n[data]\npath = \"/data\"\n",
		)
		.unwrap();
		let data = host.volumes.data("probe");
		std::fs::create_dir_all(&data).unwrap();
		let socket = tokio::net::UnixListener::bind(data.join("probe.sock")).unwrap();
		let app = axum::Router::new()
			.route("/health", axum::routing::get(|| async { r#"{"status":"success","data":null}"# }));
		tokio::spawn(async move { axum::serve(socket, app).await });
		let deployed = Deployed {
			manifest,
			image: "sha256:a".into(),
			previous: None,
			deployed_at: String::new(),
			held: false,
		};
		host.store.put_app(&deployed).unwrap();
		let (status, envelope) = read(host, "/api/apps/probe/health").await;
		assert_eq!((status, &envelope["status"]), (200, &"success".into()));
		let data = &envelope["data"];
		assert_eq!(
			(&data["app"], &data["answered"], &data["code"]),
			(&"probe".into(), &true.into(), &200.into())
		);
		assert_eq!(data["body"]["status"], "success");
	}

	#[tokio::test]
	async fn keeper_records_what_it_did_to_host_with_the_full_token_alone() {
		use http_body_util::BodyExt;
		use tower::ServiceExt;
		let directory = tempfile::tempdir().unwrap();
		let host = crate::testing(directory.path());
		let post = |token: &str, app: &str, body: serde_json::Value| {
			axum::http::Request::post(format!("/api/apps/{app}/history"))
				.header("authorization", format!("Bearer {token}"))
				.header("content-type", "application/json")
				.body(axum::body::Body::from(body.to_string()))
				.unwrap()
		};
		let recreated = serde_json::json!({
			"action": "redeploy",
			"outcome": "succeeded",
			"image": "sha256:aa",
			"detail": "recreated from the image it runs, on the node's .env as it now is",
			"started_at": "2026-10-08T03:00:00Z",
			"finished_at": "2026-10-08T03:00:41Z",
		});
		let answer = |request| async { super::router(host.clone()).oneshot(request).await.unwrap() };
		assert_eq!(answer(post("reader", "host", recreated.clone())).await.status(), 403);
		assert_eq!(answer(post("full", "geo", recreated.clone())).await.status(), 403);
		let taken = answer(post("full", "host", recreated.clone())).await;
		assert_eq!(taken.status(), 200);
		let body = taken.into_body().collect().await.unwrap().to_bytes();
		assert!(serde_json::from_slice::<serde_json::Value>(&body).unwrap()["data"]["id"].is_i64());
		let [event] = host.store.events(Some("host"), None, 10).unwrap().try_into().unwrap();
		let json = serde_json::to_value(&event).unwrap();
		assert_eq!(json["source"], serde_json::json!({ "kind": "keeper" }));
		assert_eq!((&json["action"], &json["outcome"]), (&"redeploy".into(), &"succeeded".into()));
		assert_eq!(
			(&json["started_at"], &json["finished_at"]),
			(&recreated["started_at"], &recreated["finished_at"])
		);
		assert_eq!((&json["image"], &json["detail"]), (&recreated["image"], &recreated["detail"]));
		// Only what ended, a deploy or a redeploy, at times that are times.
		for (field, value) in [("action", "rollback"), ("outcome", "running"), ("started_at", "soon")] {
			let mut bad = recreated.clone();
			bad[field] = value.into();
			assert_eq!(answer(post("full", "host", bad)).await.status(), 400, "{field}");
		}
	}

	#[test]
	fn what_keeper_did_with_a_runs_host_is_in_hosts_history() {
		use crate::store::{Outcome, Stage, Store};
		let directory = tempfile::tempdir().unwrap();
		let store = Store::open(directory.path()).unwrap();
		let reported: super::Replaced =
			serde_json::from_str(r#"{"outcome":"skipped","detail":"the canary failed the run"}"#)
				.unwrap();
		super::record_host(&store, "monoflake/infra", 42, &reported);
		let [event] = store.events(Some("host"), None, 50).unwrap().try_into().unwrap();
		assert_eq!((event.outcome, event.stage), (Outcome::Skipped, Some(Stage::Starting)));
		assert_eq!(event.detail.as_deref(), Some("the canary failed the run"));
		assert_eq!(event.source.repository.as_deref(), Some("monoflake/infra"));
		assert!(event.finished_at.is_some());
	}

	#[tokio::test]
	async fn the_canary_alone_answers_a_runs_verdict_once_deployed_and_healthy() {
		use crate::store::{Action, Deployed, Outcome, Source, Stage};
		use deploy::canary::Canary;
		let path = "/api/runs/monoflake/infra/42?built=caddy";
		let directory = tempfile::tempdir().unwrap();
		let elsewhere = crate::testing(directory.path());
		let (status, envelope) = read(elsewhere, path).await;
		assert_eq!((status, &envelope["code"]), (404, &"no_such_route".into()));

		let directory = tempfile::tempdir().unwrap();
		let canary = crate::testing_with(directory.path(), |config| config.canary = Canary::Itself);
		let (status, envelope) = read(canary.clone(), "/api/runs/monoflake/infra/42?built=geo").await;
		assert_eq!((status, &envelope["code"]), (400, &"invalid_name".into()));
		let state =
			|envelope: &serde_json::Value| envelope["data"]["state"].as_str().unwrap().to_owned();
		let (status, envelope) = read(canary.clone(), path).await;
		assert_eq!((status, state(&envelope).as_str()), (200, "pending"));

		// Deployed by the run, and answering its health on its socket, as Caddy's is asked.
		let source = Source::run(42, None).of("monoflake/infra");
		let starting = Some(Stage::Starting);
		let id =
			canary.store.record("caddy", Action::Deploy, &source, None, Outcome::Running, starting);
		canary.store.finish(id.unwrap(), Outcome::Succeeded, None, None).unwrap();
		let manifest = deploy::Manifest::parse(
			"version = 1\nname = \"caddy\"\nplacements = [\"rdu\"]\n[container]\n\
			 socket = \"admin.sock\"\nhealth = \"/config/\"\n[data]\npath = \"/data\"\n",
		)
		.unwrap();
		let deployed = Deployed {
			manifest,
			image: "sha256:a".into(),
			previous: None,
			deployed_at: String::new(),
			held: false,
		};
		canary.store.put_app(&deployed).unwrap();
		let (_, envelope) = read(canary.clone(), path).await;
		assert_eq!(state(&envelope), "pending");
		assert!(envelope["data"]["why"].as_str().unwrap().contains("does not answer its health"));
		let data = canary.volumes.data("caddy");
		std::fs::create_dir_all(&data).unwrap();
		let socket = tokio::net::UnixListener::bind(data.join("admin.sock")).unwrap();
		let admin = axum::Router::new().route("/config/", axum::routing::get(|| async { "{}" }));
		tokio::spawn(async move { axum::serve(socket, admin).await });
		let (_, envelope) = read(canary, path).await;
		assert_eq!(envelope["data"], serde_json::json!({ "state": "passed" }));
	}

	#[test]
	fn an_archived_log_is_named_only_as_the_engine_names_one() {
		assert!(super::archive_name("20260928T051000Z-1b10fb0cc52f.log"));
		for outside in ["../apps/host/.env", "x/../y.log", ".log", "a.env", "a b.log", "..log"] {
			assert!(!super::archive_name(outside), "{outside}");
		}
	}

	#[test]
	fn every_code_it_answers_with_is_in_the_catalogue() {
		for code in response::codes_named(include_str!("api.rs")) {
			assert!(
				response::message_of(code).is_some(),
				"`{code}` is not in the response crate's codes.json"
			);
		}
	}
}
