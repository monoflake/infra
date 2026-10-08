//! keeper: the one program that deploys host, so that host never replaces itself. It takes a new
//! host, runs it in place of the old one through the same procedure host uses for every app, and
//! puts the old one back if the new one does not become healthy. It keeps no state: what runs is
//! read back from Docker. See spec/architecture/host.md, "host never updates itself; keeper
//! updates host".

use axum::extract::{DefaultBodyLimit, Multipart, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use deploy::canary::{Canary, Held};
use deploy::replace::{Error as Failed, replace, replace_back};
use deploy::{Engine, Manifest, Shape, Version, Volumes};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError};

/// musl's allocator is slow under many small allocations, and images are built for speed; see
/// spec/architecture/host.md, "An image is built for speed, and for any node of its architecture".
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// keeper's own port, one below host's. `service.toml` states it for host, and the test below
/// holds the two together.
const PORT: u16 = 11010;

/// host's port, which keeper passes a run on to; host's `service.toml` states it.
const HOST_PORT: u16 = 11011;

/// host as keeper dials it, on host's own network: keeper shares its own with host as well, where
/// host does not listen. See `deploy::engine::on_own_network`.
fn host_address() -> String {
	deploy::engine::on_own_network("host", HOST_PORT)
}

struct Keeper {
	node: String,
	token: String,
	own_container: String,
	/// The node's one `.env`, which host is started with.
	platform_env: PathBuf,
	incoming: PathBuf,
	engine: Engine,
	volumes: Volumes,
	replacing: tokio::sync::Mutex<()>,
	/// Absent without a GITHUB_ACTIONS_TOKEN and DEPLOY_SOURCES, and then CI's notices are refused.
	github: Option<deploy::github::GitHub>,
	/// The runs a notice has been taken for, and whether it was sent by hand, past the canary.
	notices: std::sync::Mutex<std::collections::HashSet<(String, u64, bool)>>,
	/// The runs deployed by hand past the canary, which a notice holding the same run gives up.
	released: std::sync::Mutex<std::collections::HashSet<(String, u64)>>,
	/// Where the canary is, from `CANARY`: this node, another at its tailnet address, or none.
	canary: Canary,
	/// What the canary is asked with: the private suffix its Caddy answers under, and the read token.
	private_suffix: String,
	read_token: String,
}

fn setting(key: &str, default: &str) -> String {
	std::env::var(key).ok().filter(|value| !value.is_empty()).unwrap_or_else(|| default.into())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	let apps = PathBuf::from(setting("APPS_ROOT", "/data/apps"));
	let keeper = Arc::new(Keeper {
		node: std::env::var("NODE")?,
		token: std::env::var("HOST_TOKEN")?,
		own_container: setting("OWN_CONTAINER", "keeper"),
		platform_env: apps.join("host").join(".env"),
		incoming: apps.join("keeper").join("data").join("incoming"),
		engine: Engine::connect()?,
		volumes: Volumes::new(
			apps,
			PathBuf::from(setting("SNAPSHOTS_ROOT", "/data/.snapshots")),
			PathBuf::from(setting("LOGS_ROOT", "/data/logs")),
		),
		replacing: tokio::sync::Mutex::new(()),
		github: deploy::github::GitHub::from_env(),
		notices: std::sync::Mutex::default(),
		released: std::sync::Mutex::default(),
		canary: Canary::parse(&setting("CANARY", ""))
			.map_err(|value| anyhow::anyhow!("CANARY `{value}` is neither self nor an IPv4"))?,
		private_suffix: setting("PRIVATE_SUFFIX", ""),
		read_token: setting("HOST_READ_TOKEN", ""),
	});
	deploy::clear_arrivals(&keeper.incoming)?;
	// On host's own network every time it starts, as Caddy is: a keeper that host redeploys comes
	// back on its own network alone, and replacing host joins it only once that begins.
	let own = deploy::engine::network_of("host");
	if let Err(error) = keeper.engine.join(&own, &[keeper.own_container.as_str()], false).await {
		eprintln!("keeper: joining {own}: {error}");
	}
	let listen = setting("LISTEN", &format!("0.0.0.0:{PORT}"));
	let guarded = Router::new()
		.route("/apps/host", post(upload).layer(DefaultBodyLimit::disable()))
		.route("/host/redeploy", post(redeploy_host))
		.layer(middleware::from_fn_with_state(keeper.clone(), admit));
	let router = Router::new()
		.route("/health", get(health))
		.route("/notice", post(notice))
		.merge(guarded)
		.with_state(keeper);
	let listener = tokio::net::TcpListener::bind(&listen).await?;
	eprintln!("keeper: listening on {listen}");
	axum::serve(listener, router).with_graceful_shutdown(stopped()).await?;
	Ok(())
}

async fn health(State(keeper): State<Arc<Keeper>>) -> Response {
	match keeper.engine.ping().await {
		Ok(()) => response::success(StatusCode::OK, ()),
		Err(_) => response::failure(StatusCode::SERVICE_UNAVAILABLE, "docker_unavailable"),
	}
}

/// Compared in time independent of where the first difference is.
fn same(given: &[u8], expected: &[u8]) -> bool {
	given.len() == expected.len()
		&& given.iter().zip(expected).fold(0, |acc, (a, b)| acc | (a ^ b)) == 0
}

async fn admit(State(keeper): State<Arc<Keeper>>, request: Request, next: Next) -> Response {
	let given = request
		.headers()
		.get(header::AUTHORIZATION)
		.and_then(|value| value.to_str().ok())
		.and_then(|value| value.strip_prefix("Bearer "))
		.unwrap_or_default();
	if !same(given.as_bytes(), keeper.token.as_bytes()) {
		return response::failure(StatusCode::UNAUTHORIZED, "invalid_token");
	}
	next.run(request).await
}

/// The same request host takes for any app: the declaration as `service`, the archive as `image`,
/// written to disk whole before the running host is touched.
async fn upload(State(keeper): State<Arc<Keeper>>, mut parts: Multipart) -> Response {
	let archive = deploy::arrival(&keeper.incoming);
	let mut declared = None;
	let mut received = false;
	loop {
		let mut part = match parts.next_field().await {
			Ok(Some(part)) => part,
			Ok(None) => break,
			Err(error) => return upload_refused(error),
		};
		match part.name() {
			Some("service") => match part.text().await.map(|text| Manifest::parse(&text)) {
				Ok(Ok(manifest)) => declared = Some(manifest),
				Ok(Err(error)) => {
					return response::failure_with(
						StatusCode::UNPROCESSABLE_ENTITY,
						"invalid_declaration",
						error,
					);
				}
				Err(error) => return upload_refused(error),
			},
			Some("image") => {
				let saved = async {
					tokio::fs::create_dir_all(&keeper.incoming).await?;
					let mut file = deploy::uncached::AsyncWriter::create(&archive).await?;
					while let Some(chunk) = part.chunk().await? {
						file.write(&chunk).await?;
					}
					file.finish().await?;
					anyhow::Ok(())
				};
				if let Err(error) = saved.await {
					return upload_refused(error);
				}
				received = true;
			}
			_ => {}
		}
	}
	let Some(manifest) = declared else {
		return response::failure_with(StatusCode::BAD_REQUEST, "invalid_upload", "No service part");
	};
	if !received {
		return response::failure_with(StatusCode::BAD_REQUEST, "invalid_upload", "No image part");
	}
	let started = jiff::Timestamp::now();
	let placed = from_archive(&keeper, manifest, &archive).await;
	let done = match &placed {
		Ok(Placed { image, unchanged: true }) => {
			let why = "unchanged: the upload is the image host already runs";
			Done::succeeded(Some(image), Some(why.into()))
		}
		Ok(Placed { image, .. }) => Done::succeeded(Some(image), Some("uploaded to keeper".into())),
		Err(Reply(_, _, message)) => Done::failed(message),
	};
	tokio::spawn(report(keeper.token.clone(), "deploy", done, started));
	match placed {
		Ok(Placed { image, unchanged }) => response::success(
			StatusCode::OK,
			serde_json::json!({ "name": "host", "image": image, "unchanged": unchanged }),
		),
		Err(Reply(status, code, message)) => response::failure_with(status, code, message),
	}
}

/// Recreate host from the version and image it runs, on the node's `.env` as it now is: how a
/// changed `.env` reaches host without a run that built it. See spec/architecture/host.md, "A
/// changed `.env` reaches host through keeper".
async fn redeploy_host(State(keeper): State<Arc<Keeper>>) -> Response {
	let started = jiff::Timestamp::now();
	let recreated = recreate_host(&keeper).await;
	// A refusal while host was busy did nothing, and leaves no row.
	if !matches!(&recreated, Err(Reply(StatusCode::CONFLICT, ..))) {
		let done = match &recreated {
			Ok(image) => Done::succeeded(Some(image), Some(RECREATED.into())),
			Err(Reply(_, _, message)) => Done::failed(message),
		};
		tokio::spawn(report(keeper.token.clone(), "redeploy", done, started));
	}
	match recreated {
		Ok(image) => {
			response::success(StatusCode::OK, serde_json::json!({ "name": "host", "image": image }))
		}
		Err(Reply(status, code, message)) => response::failure_with(status, code, message),
	}
}

/// What a recreate of host is recorded as having done.
const RECREATED: &str = "recreated from the image it runs, on the node's .env as it now is";

/// How keeper's act on host ended, as host's history records it.
struct Done {
	outcome: &'static str,
	image: Option<String>,
	detail: Option<String>,
}

impl Done {
	fn succeeded(image: Option<&String>, detail: Option<String>) -> Self {
		Self { outcome: "succeeded", image: image.cloned(), detail }
	}

	fn failed(message: &str) -> Self {
		Self { outcome: "failed", image: None, detail: Some(message.to_owned()) }
	}
}

/// How long keeper keeps offering host the record: a host put back after a failed one is started
/// without waiting for its health, so it may take a while to answer.
const REPORTING: std::time::Duration = std::time::Duration::from_secs(60);

/// The row host's history holds for keeper's `action` on host, begun at `started`: sent once the
/// host that now runs answers, since none answered while it was being replaced. See host's
/// `keeper_report`.
async fn report(token: String, action: &'static str, done: Done, started: jiff::Timestamp) {
	let body = record(action, &done, started, jiff::Timestamp::now()).to_string().into_bytes();
	let address = host_address();
	let deadline = tokio::time::Instant::now() + REPORTING;
	loop {
		let sent = deploy::http::post_as(
			&address,
			&address,
			"/api/apps/host/history",
			&token,
			body.clone(),
			std::time::Duration::from_secs(5),
		)
		.await;
		match sent {
			Ok((status, _)) if (200..300).contains(&status) => return,
			Ok((status, answer)) if status < 500 => {
				return eprintln!("keeper: host refused the record of its {action}: {status} {answer}");
			}
			_ if tokio::time::Instant::now() >= deadline => {
				return eprintln!("keeper: host took no record of its {action} within a minute");
			}
			_ => tokio::time::sleep(std::time::Duration::from_secs(2)).await,
		}
	}
}

/// The record host keeps of keeper's `action`, as host's `keeper_report` reads it.
fn record(
	action: &str,
	done: &Done,
	started: jiff::Timestamp,
	finished: jiff::Timestamp,
) -> serde_json::Value {
	serde_json::json!({
		"action": action,
		"outcome": done.outcome,
		"image": done.image,
		"detail": done.detail,
		"started_at": started.to_string(),
		"finished_at": finished.to_string(),
	})
}

/// Refused while host has an event running, since replacing it mid-deploy would leave that app
/// half replaced; a new host that fails its check is put back on the environment it ran with.
async fn recreate_host(keeper: &Keeper) -> Result<String, Reply> {
	let internal = |error: &dyn std::fmt::Display| {
		Reply(StatusCode::INTERNAL_SERVER_ERROR, "service_unavailable", error.to_string())
	};
	let _one = keeper.replacing.lock().await;
	if let Some(running) = host_busy(keeper).await {
		let message = format!("host has an event running, {running}; ask again once it ends");
		return Err(Reply(StatusCode::CONFLICT, "job_running", message));
	}
	let fallback =
		Manifest::parse(include_str!("../../host/service.toml")).map_err(|e| internal(&e))?;
	let current = keeper.engine.current("host", &fallback).await.map_err(|e| internal(&e))?;
	let Some(current) = current else {
		return Err(Reply(StatusCode::NOT_FOUND, "no_such_app", "No host runs on this node".into()));
	};
	let before = keeper.engine.env_of("host").await.map_err(|e| internal(&e))?.unwrap_or_default();
	let env = deploy::read_env(&keeper.platform_env).map_err(|e| internal(&e))?;
	let members = [keeper.own_container.as_str()];
	let (shape, back) = (Shape::Platform { env }, Shape::Platform { env: before });
	let replaced =
		replace_back(&keeper.engine, &keeper.volumes, &members, &shape, &back, &current, &current);
	match replaced.await {
		Ok(_) => Ok(current.image),
		Err(error @ (Failed::Unhealthy { .. } | Failed::FirstFailed { .. })) => {
			Err(Reply(StatusCode::BAD_GATEWAY, "app_unavailable", error.to_string()))
		}
		Err(error) => Err(internal(&error)),
	}
}

/// The event host says is still running, if any, as `<action> of <app>`. A host that does not
/// answer is not busy deploying anything, and is what recreating it is for.
async fn host_busy(keeper: &Keeper) -> Option<String> {
	let address = host_address();
	let path = "/api/events?limit=50";
	let asked = deploy::http::get_as(&address, &address, path, &keeper.token, ASKING).await;
	match asked {
		Ok((status, body)) if (200..300).contains(&status) => running_event(&body),
		Ok((status, _)) => {
			eprintln!("keeper: host answered {status} to its events; recreating it anyway");
			None
		}
		Err(error) => {
			eprintln!("keeper: asking host for its events: {error}; recreating it anyway");
			None
		}
	}
}

/// How long host has to say what it is doing.
const ASKING: std::time::Duration = std::time::Duration::from_secs(5);

/// The first running event in host's answer to `/api/events`, as `<action> of <app>`.
fn running_event(body: &str) -> Option<String> {
	let envelope: serde_json::Value = serde_json::from_str(body).ok()?;
	envelope["data"].as_array()?.iter().find(|event| event["outcome"] == "running").map(|event| {
		let (action, app) = (event["action"].as_str(), event["app"].as_str());
		format!("{} of {}", action.unwrap_or("an action"), app.unwrap_or("an app"))
	})
}

fn upload_refused(error: impl ToString) -> Response {
	response::failure_with(StatusCode::BAD_REQUEST, "invalid_upload", error)
}

/// Load a host archive and put it in place, as an upload or a notice brings one. The archive is
/// gone afterwards whatever happened.
/// What a host archive left running: its image, and whether it was the one host already ran.
struct Placed {
	image: String,
	unchanged: bool,
}

/// Whether host runs the image an archive holds, by one of its `ids`, under `manifest`: then it is
/// not replaced, which would restart it for nothing. A host that is not running is replaced.
fn unchanged(ids: &[String], running: Option<&Version>, manifest: &Manifest) -> bool {
	running.is_some_and(|host| ids.contains(&host.image) && host.manifest == *manifest)
}

async fn from_archive(
	keeper: &Keeper,
	manifest: Manifest,
	archive: &Path,
) -> Result<Placed, Reply> {
	let replaced = async {
		if let Err(error) = manifest.check_own("host", &keeper.node) {
			return Err(Reply(
				StatusCode::UNPROCESSABLE_ENTITY,
				"invalid_declaration",
				error.to_string(),
			));
		}
		let _one = keeper.replacing.lock().await;
		let path = archive.to_path_buf();
		let ids = tokio::task::spawn_blocking(move || deploy::archive::identities(&path));
		let ids = ids.await.ok().and_then(Result::ok).unwrap_or_default();
		let current = keeper.engine.current("host", &manifest).await.ok().flatten();
		let running = keeper.engine.running("host").await.unwrap_or(false);
		if let Some(current) =
			current.filter(|current| running && unchanged(&ids, Some(current), &manifest))
		{
			return Ok(Placed { image: current.image, unchanged: true });
		}
		let file = deploy::uncached::read(archive).await.map_err(|e| {
			Reply(StatusCode::INTERNAL_SERVER_ERROR, "service_unavailable", e.to_string())
		})?;
		let loaded = keeper.engine.load("host", file).await;
		let image = loaded
			.map_err(|e| Reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_image", e.to_string()))?;
		let image = replace_host(keeper, Version { manifest, image }).await?;
		Ok(Placed { image, unchanged: false })
	}
	.await;
	let _ = tokio::fs::remove_file(archive).await;
	replaced
}

#[derive(serde::Deserialize)]
struct Notice {
	run: u64,
	/// Whose run, as `owner/name`. A notice from before they named one means the one source.
	#[serde(default)]
	repository: Option<String>,
	/// Sent by `mise run node deploy`: host is replaced now, without waiting for the canary.
	#[serde(default)]
	by_hand: bool,
}

/// A CI run has finished; if it built host, host is replaced. Open, since it can only ask keeper
/// to look: the run is checked against GitHub first. Each run is taken once, and again only if
/// taking it failed. See spec/architecture/host.md, "keeper has its own intake".
async fn notice(State(keeper): State<Arc<Keeper>>, Json(notice): Json<Notice>) -> Response {
	let Some(github) = keeper.github.as_ref() else {
		return response::failure(StatusCode::SERVICE_UNAVAILABLE, "github_unavailable");
	};
	let repository = notice.repository.or_else(|| github.only_source().map(str::to_owned));
	let Some(repository) = repository else {
		return response::failure(StatusCode::BAD_REQUEST, "invalid_repository");
	};
	let key = (repository.clone(), notice.run, notice.by_hand);
	let notices = || keeper.notices.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
	let answer = serde_json::json!({ "run": notice.run, "repository": repository });
	if !notices().insert(key.clone()) {
		return response::success(StatusCode::OK, answer);
	}
	if notice.by_hand {
		let mut released = keeper.released.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		released.insert((repository.clone(), notice.run));
	}
	let taker = keeper.clone();
	tokio::spawn(async move {
		if !from_run(&taker, &repository, notice.run, notice.by_hand).await {
			taker.notices.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&key);
		}
	});
	response::success(StatusCode::ACCEPTED, answer)
}

/// The host a run built, if it built one, put in place -- on any node but the canary only once the
/// canary passed the run, unless it is sent by hand. True when nothing failed.
async fn from_run(keeper: &Keeper, repository: &str, run: u64, by_hand: bool) -> bool {
	let Some(github) = keeper.github.as_ref() else { return false };
	// host is infra's own, and only infra's repository deploys it, whatever another's run built.
	if github.scope_of(repository) != Some(deploy::github::INFRA_SCOPE) {
		return true;
	}
	let artifacts = match github.artifacts(repository, run).await {
		Ok(built) => built.artifacts,
		Err(error) => {
			eprintln!("keeper: run {run}: {error}");
			return false;
		}
	};
	let Some(artifact) = artifacts.iter().find(|artifact| artifact.app == "host") else {
		return true;
	};
	if let Canary::At(canary) = keeper.canary
		&& !by_hand
	{
		let gated: Vec<&str> = artifacts
			.iter()
			.map(|artifact| artifact.app.as_str())
			.filter(|app| deploy::canary::GATED.contains(app))
			.collect();
		eprintln!("keeper: run {run}: it built host, so it waits for the canary");
		let (suffix, token) = (&keeper.private_suffix, &keeper.read_token);
		let ask = || deploy::canary::ask(canary, suffix, token, repository, run, &gated);
		let key = (repository.to_owned(), run);
		let released = || keeper.released.lock().unwrap_or_else(PoisonError::into_inner).contains(&key);
		let (deadline, waits) = (deploy::canary::DEADLINE, deploy::canary::waits());
		let why = match deploy::canary::hold(ask, released, deadline, waits, tokio::time::sleep).await {
			Held::Passed => None,
			Held::Failed(why) => Some(format!("the canary failed the run: {why}")),
			Held::Expired(last) => Some(format!("the canary gave no verdict in time: {last}")),
			// The notice sent by hand replaces host and passes the run on itself.
			Held::Released => {
				keeper.released.lock().unwrap_or_else(PoisonError::into_inner).remove(&key);
				eprintln!("keeper: run {run}: deployed by hand instead");
				return true;
			}
		};
		if let Some(why) = why {
			eprintln!("keeper: run {run}: host not replaced: {why}");
			pass_on(repository, run, ("skipped", Some(why))).await;
			return true;
		}
	}
	let fetched = match github.fetch(artifact, &keeper.incoming).await {
		Ok(fetched) => fetched,
		Err(error) => {
			eprintln!("keeper: run {run}: {error}");
			return false;
		}
	};
	let manifest = match Manifest::parse(&fetched.declaration) {
		Ok(manifest) => manifest,
		Err(error) => {
			eprintln!("keeper: run {run}: {error}");
			let _ = tokio::fs::remove_file(&fetched.image).await;
			return true;
		}
	};
	let (replaced, outcome) = match from_archive(keeper, manifest, &fetched.image).await {
		Ok(Placed { image, unchanged: true }) => {
			eprintln!("keeper: run {run}: host already runs {image}; not replaced");
			let why = format!("unchanged: run {run} built the image host already runs");
			(true, ("succeeded", Some(why)))
		}
		Ok(Placed { image, .. }) => {
			eprintln!("keeper: run {run}: host is {image}");
			(true, ("succeeded", None))
		}
		Err(Reply(_, _, message)) => {
			eprintln!("keeper: run {run}: {message}");
			(false, ("failed", Some(message)))
		}
	};
	// Whether the new host stayed or the old one is back, the run's other images are host's now.
	pass_on(repository, run, outcome).await;
	replaced
}

/// Hand `run` to host with host's part done, over the network the replacement joined keeper to,
/// saying how it went: host records it, and the canary's verdict reads it.
async fn pass_on(repository: &str, run: u64, (outcome, detail): (&str, Option<String>)) {
	let host = serde_json::json!({ "outcome": outcome, "detail": detail });
	let notice = serde_json::json!({ "run": run, "repository": repository, "host_replaced": true, "host": host });
	let body = notice.to_string().into_bytes();
	match deploy::http::post(&host_address(), "/notice", body).await {
		Ok(status) if (200..300).contains(&status) => {}
		Ok(status) => eprintln!("keeper: run {run}: host answered {status} to the run passed on"),
		Err(error) => eprintln!("keeper: run {run}: passing it on to host: {error}"),
	}
}

/// A refusal on its way to the envelope: the status, the code, and what exactly went wrong.
struct Reply(StatusCode, &'static str, String);

async fn replace_host(keeper: &Keeper, next: Version) -> Result<String, Reply> {
	let internal = |error: &dyn std::fmt::Display| {
		Reply(StatusCode::INTERNAL_SERVER_ERROR, "service_unavailable", error.to_string())
	};
	// A host started by hand carries no recorded version; the one just sent stands in for its
	// declaration, beside the image it really runs.
	let current = keeper.engine.current("host", &next.manifest).await.map_err(|e| internal(&e))?;
	let env = deploy::read_env(&keeper.platform_env).map_err(|e| internal(&e))?;
	// keeper joins host's network; Caddy is attached by host when it starts, for its door alone.
	let members = [keeper.own_container.as_str()];
	let shape = Shape::Platform { env };
	let replaced =
		replace(&keeper.engine, &keeper.volumes, &members, &shape, &next, current.as_ref(), None);
	match replaced.await {
		Ok(_) => {}
		Err(error @ (Failed::Unhealthy { .. } | Failed::FirstFailed { .. })) => {
			return Err(Reply(StatusCode::BAD_GATEWAY, "app_unavailable", error.to_string()));
		}
		Err(error) => return Err(internal(&error)),
	}
	// host's images are keeper's to collect: the one now running, and the one it would go back to.
	let keep: HashSet<String> = [Some(next.image.clone()), current.map(|current| current.image)]
		.into_iter()
		.flatten()
		.collect();
	keeper.engine.collect(&["host"], &keep).await.map_err(|e| internal(&e))?;
	Ok(next.image)
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

#[cfg(test)]
mod tests {
	#[test]
	fn the_port_is_the_one_the_declaration_states() {
		let declaration = include_str!("../service.toml");
		assert!(declaration.lines().any(|line| line.trim() == format!("port = {}", super::PORT)));
	}

	#[test]
	fn host_is_where_host_declares_it() {
		let declaration = include_str!("../../host/service.toml");
		assert!(declaration.lines().any(|line| line.trim() == format!("port = {}", super::HOST_PORT)));
	}

	#[test]
	fn host_is_busy_while_any_event_it_lists_is_running() {
		let events = |rows: &str| format!(r#"{{"status":"success","data":[{rows}]}}"#);
		let done = r#"{"app":"geo","action":"deploy","outcome":"succeeded"}"#;
		let running = r#"{"app":"caddy","action":"redeploy","outcome":"running"}"#;
		assert_eq!(super::running_event(&events(done)), None);
		assert_eq!(super::running_event(&events("")), None);
		let busy = super::running_event(&events(&format!("{done},{running}")));
		assert_eq!(busy.as_deref(), Some("redeploy of caddy"));
		assert_eq!(super::running_event("not json"), None);
	}

	#[test]
	fn host_is_not_replaced_by_the_image_it_runs_under_the_same_declaration() {
		use deploy::{Manifest, Version};
		let declared = Manifest::parse(include_str!("../../host/service.toml")).unwrap();
		let running = Version { manifest: declared.clone(), image: "sha256:aa".into() };
		let ids = ["sha256:aa".to_owned(), "sha256:cc".to_owned()];
		assert!(super::unchanged(&ids, Some(&running), &declared));
		assert!(!super::unchanged(&["sha256:bb".to_owned()], Some(&running), &declared));
		let mut changed = declared.clone();
		changed.display_name = Some("Host again".into());
		assert!(!super::unchanged(&ids, Some(&running), &changed));
		assert!(!super::unchanged(&ids, None, &declared));
	}

	#[test]
	fn the_record_of_an_act_on_host_is_what_host_reads() {
		use super::{Done, record};
		let started: jiff::Timestamp = "2026-10-08T03:00:00Z".parse().unwrap();
		let finished: jiff::Timestamp = "2026-10-08T03:00:41Z".parse().unwrap();
		let image = "sha256:aa".to_owned();
		let done = Done::succeeded(Some(&image), Some(super::RECREATED.into()));
		let written = record("redeploy", &done, started, finished);
		assert_eq!(
			written,
			serde_json::json!({
				"action": "redeploy",
				"outcome": "succeeded",
				"image": "sha256:aa",
				"detail": super::RECREATED,
				"started_at": "2026-10-08T03:00:00Z",
				"finished_at": "2026-10-08T03:00:41Z",
			})
		);
		let failed =
			record("deploy", &Done::failed("not healthy within 60 seconds"), started, finished);
		assert_eq!(
			(&failed["outcome"], &failed["image"]),
			(&"failed".into(), &serde_json::Value::Null)
		);
	}

	#[test]
	fn host_is_reached_on_its_own_network() {
		assert_eq!(super::host_address(), "host.app-host:11011");
	}

	#[test]
	fn a_token_matches_only_itself() {
		assert!(super::same(b"secret", b"secret"));
		assert!(!super::same(b"secreT", b"secret"));
		assert!(!super::same(b"", b"secret"));
	}

	#[test]
	fn every_code_it_answers_with_is_in_the_catalogue() {
		for code in response::codes_named(include_str!("main.rs")) {
			assert!(
				response::message_of(code).is_some(),
				"`{code}` is not in the response crate's codes.json"
			);
		}
	}
}
