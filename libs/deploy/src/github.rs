//! What CI built, fetched: a run's record held to what a deploy requires, and its artifacts
//! downloaded and held to the digest GitHub recorded for them. The notice that names a run is a
//! hint; this is where it becomes a decision. See spec/architecture/host.md, "The machine pulls;
//! nothing pushes into it".

use crate::egress::{self, Egress};
use crate::uncached::{AsyncWriter, Writer};
use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::Request;
use hyper::body::Incoming;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The workflow that builds images; a run of any other builds nothing to deploy.
pub const WORKFLOW: &str = ".github/workflows/deploy.yml";
/// What may have started it. A pull request never does, and would not count if it did.
const EVENTS: [&str; 3] = ["push", "schedule", "workflow_dispatch"];
/// How an artifact carrying an app's image is named: `deploy-geo-arm64`.
const PREFIX: &str = "deploy-";
/// The one artifact holding every declaration the run built an image for, each `<app>.toml`.
const DECLARATIONS: &str = "declarations";
/// The most a declarations artifact may hold, and the most one declaration may weigh: a
/// declaration is a page of TOML, and anything bigger is not what CI uploads.
const MOST_DECLARATIONS: usize = 256;
const LARGEST_DECLARATION: u64 = 64 * 1024;
/// The architectures CI builds for, spelled as Docker's `TARGETARCH` spells them.
const ARCHES: [&str; 2] = ["arm64", "amd64"];

#[derive(Debug, thiserror::Error)]
pub enum Error {
	#[error("GitHub answered {status} for {what}")]
	Status { status: u16, what: String },
	#[error("reaching GitHub: {0}")]
	Http(String),
	#[error("run {run} is not one to deploy: {why}")]
	NotDeployable { run: u64, why: String },
	#[error("artifact `{0}` does not match the digest GitHub recorded for it")]
	Digest(String),
	#[error("artifact `{name}` is not what CI uploads: {why}")]
	Shape { name: String, why: String },
	#[error("{what}: {source}")]
	Io { what: String, source: std::io::Error },
}

fn io(what: impl Into<String>) -> impl FnOnce(std::io::Error) -> Error {
	let what = what.into();
	move |source| Error::Io { what, source }
}

#[derive(Debug, Deserialize)]
pub struct Run {
	pub repository: Repository,
	pub path: String,
	pub head_branch: Option<String>,
	pub event: String,
	pub status: String,
	pub conclusion: Option<String>,
	/// The commit the run built, recorded beside what it deployed.
	#[serde(default)]
	pub head_sha: Option<String>,
}

/// What a run built: its commit, the artifacts it left for this node's architecture and for the
/// others, and its declarations apart when it uploaded them.
#[derive(Debug)]
pub struct Built {
	pub commit: Option<String>,
	pub artifacts: Vec<Artifact>,
	pub others: Vec<Artifact>,
	pub declarations: Option<Declarations>,
}

/// The run's declarations, uploaded apart from its images.
#[derive(Debug, Clone)]
pub struct Declarations {
	repository: String,
	id: u64,
	digest: String,
}

impl Built {
	/// The run's image of `app` for `arch`, whichever architecture this node runs. An app that asks
	/// for an architecture is fetched in it. See spec/architecture/host.md, "An app may ask for one
	/// architecture".
	pub fn built_for(&self, app: &str, arch: &str) -> Option<&Artifact> {
		self.artifacts.iter().chain(&self.others).find(|built| built.app == app && built.arch == arch)
	}
}

#[derive(Debug, Deserialize)]
pub struct Repository {
	pub full_name: String,
}

#[derive(Deserialize)]
struct Listed {
	artifacts: Vec<Record>,
}

#[derive(Deserialize)]
struct Record {
	id: u64,
	name: String,
	expired: bool,
	digest: Option<String>,
}

/// One app's image as a run left it, and the repository whose run left it.
#[derive(Debug, Clone)]
pub struct Artifact {
	pub app: String,
	pub arch: &'static str,
	repository: String,
	id: u64,
	digest: String,
}

/// The repositories a node deploys from, as `DEPLOY_SOURCES` names them: `owner/name` pairs
/// separated by whitespace. See spec/architecture/host.md, "The machine pulls; nothing pushes into
/// it".
pub fn sources(value: &str) -> Vec<String> {
	value
		.split_whitespace()
		.filter(|source| source.split('/').count() == 2)
		.map(str::to_owned)
		.collect()
}

/// An artifact on disk: the image archive, and the declaration built beside it.
pub struct Fetched {
	pub image: PathBuf,
	pub declaration: String,
}

/// The architecture this node runs, spelled as an artifact name spells it. None for any other,
/// which then matches no artifact.
pub fn node_arch() -> Option<&'static str> {
	arch_of(std::env::consts::ARCH)
}

fn arch_of(arch: &str) -> Option<&'static str> {
	match arch {
		"aarch64" => Some("arm64"),
		"x86_64" => Some("amd64"),
		_ => None,
	}
}

/// The app and architecture an artifact name carries, when the name is one CI gives and the app a
/// name an app can have. The name comes from GitHub's answer, not from this repository, so it is
/// held to the same rule as any other name before anything is done with it.
pub fn app_of(artifact: &str) -> Option<(String, &'static str)> {
	let rest = artifact.strip_prefix(PREFIX)?;
	let (app, arch) = ARCHES
		.iter()
		.find_map(|arch| Some((rest.strip_suffix(arch)?.strip_suffix('-')?, *arch)))
		// The legacy name, `deploy-geo`; it goes once CI names every artifact with its arch.
		.unwrap_or((rest, "arm64"));
	let named = crate::manifest::check_name(app).is_ok() || crate::manifest::OWN.contains(&app);
	named.then(|| (app.to_owned(), arch))
}

/// Whether `record` is a finished, successful run of `repository`'s deploy workflow on `main`.
pub fn check(run: u64, record: &Run, repository: &str) -> Result<(), Error> {
	let refuse = |why: String| Err(Error::NotDeployable { run, why });
	if record.repository.full_name != repository {
		return refuse(format!("it belongs to {}", record.repository.full_name));
	}
	if record.path != WORKFLOW {
		return refuse(format!("it ran {}", record.path));
	}
	if record.head_branch.as_deref() != Some("main") {
		return refuse(format!("it ran on {:?}", record.head_branch));
	}
	if !EVENTS.contains(&record.event.as_str()) {
		return refuse(format!("a {} started it", record.event));
	}
	if record.status != "completed" || record.conclusion.as_deref() != Some("success") {
		return refuse(format!("it is {} with {:?}", record.status, record.conclusion));
	}
	Ok(())
}

pub struct GitHub {
	client: Client<hyper_rustls::HttpsConnector<Egress>, Empty<Bytes>>,
	/// The repositories this node deploys from; a run of any other is refused before it is read.
	sources: Vec<String>,
	token: String,
}

impl GitHub {
	/// Roots compiled in rather than read from the system: an image built from scratch has none.
	pub fn new(token: String, sources: Vec<String>, egress: Egress) -> Self {
		let https = hyper_rustls::HttpsConnectorBuilder::new()
			.with_webpki_roots()
			.https_only()
			.enable_http1()
			.wrap_connector(egress);
		Self { client: Client::builder(TokioExecutor::new()).build(https), sources, token }
	}

	/// The client a node's environment configures: its token, the sources it deploys from, and the
	/// egress proxies it leaves through. None without the first two or with a proxy it cannot read,
	/// and then the node refuses CI's notices rather than guessing a repository or a way out.
	pub fn from_env() -> Option<Self> {
		let token = std::env::var("GITHUB_ACTIONS_TOKEN").ok().filter(|token| !token.is_empty())?;
		let sources = sources(&std::env::var("DEPLOY_SOURCES").unwrap_or_default());
		if sources.is_empty() {
			eprintln!("deploy: DEPLOY_SOURCES names no repository, so no run is deployed");
			return None;
		}
		let proxies = egress::proxies(&std::env::var("EGRESS_PROXIES").unwrap_or_default());
		let proxies =
			proxies.inspect_err(|error| eprintln!("deploy: {error}, so no run is deployed")).ok()?;
		Some(Self::new(token, sources, Egress::new(proxies)))
	}

	/// The repository a notice that names none is about: the one source, when there is one. A
	/// notice from before notices named their repository can mean nothing else.
	pub fn only_source(&self) -> Option<&str> {
		match self.sources.as_slice() {
			[only] => Some(only),
			_ => None,
		}
	}

	/// A GET, with the token only when it is GitHub's own API being asked.
	async fn get(&self, uri: &str, authorized: bool) -> Result<hyper::Response<Incoming>, Error> {
		let mut request = Request::get(uri)
			.header("user-agent", "canmi-host")
			.header("accept", "application/vnd.github+json")
			.header("x-github-api-version", "2022-11-28");
		if authorized {
			request = request.header("authorization", format!("Bearer {}", self.token));
		}
		let request = request.body(Empty::new()).map_err(|e| Error::Http(e.to_string()))?;
		self.client.request(request).await.map_err(|e| Error::Http(egress::chain(&e)))
	}

	async fn json<T: DeserializeOwned>(&self, repository: &str, path: &str) -> Result<T, Error> {
		let uri = format!("{}/repos/{repository}{path}", canmi::EXTERNAL_GITHUB_API);
		let response = self.get(&uri, true).await?;
		let status = response.status().as_u16();
		let body = response.into_body().collect().await.map_err(|e| Error::Http(e.to_string()))?;
		if !(200..300).contains(&status) {
			return Err(Error::Status { status, what: path.to_owned() });
		}
		serde_json::from_slice(&body.to_bytes()).map_err(|e| Error::Http(e.to_string()))
	}

	/// The deploy artifacts of `repository`'s `run`, once its record says it is one to deploy.
	pub async fn artifacts(&self, repository: &str, run: u64) -> Result<Built, Error> {
		if !self.sources.iter().any(|source| source == repository) {
			let why = format!("{repository} is not a repository this node deploys from");
			return Err(Error::NotDeployable { run, why });
		}
		let record: Run = self.json(repository, &format!("/actions/runs/{run}")).await?;
		check(run, &record, repository)?;
		let path = format!("/actions/runs/{run}/artifacts?per_page=100");
		let listed: Listed = self.json(repository, &path).await?;
		let fresh: Vec<Record> =
			listed.artifacts.into_iter().filter(|record| !record.expired).collect();
		let declarations = fresh.iter().find(|record| record.name == DECLARATIONS).and_then(|record| {
			let digest = record.digest.clone()?;
			Some(Declarations { repository: repository.to_owned(), id: record.id, digest })
		});
		let (artifacts, others) = fresh
			.into_iter()
			.filter_map(|record| {
				let (app, arch) = app_of(&record.name)?;
				Some(Artifact {
					app,
					arch,
					repository: repository.to_owned(),
					id: record.id,
					digest: record.digest?,
				})
			})
			.partition(|artifact| Some(artifact.arch) == node_arch());
		Ok(Built { commit: record.head_sha, artifacts, others, declarations })
	}

	/// Download `artifact` into `directory`, hold it to its digest, and take out what CI put in it.
	pub async fn fetch(&self, artifact: &Artifact, directory: &Path) -> Result<Fetched, Error> {
		let at = (artifact.repository.as_str(), artifact.id, artifact.digest.as_str());
		let zip = self.download(at, &artifact.app, directory).await?;
		let image = crate::arrival(directory);
		let (name, from, to) = (artifact.app.clone(), zip.clone(), image.clone());
		let unpacked = tokio::task::spawn_blocking(move || unpack(&name, &from, &to))
			.await
			.map_err(|e| Error::Http(e.to_string()))?;
		let _ = tokio::fs::remove_file(&zip).await;
		Ok(Fetched { image, declaration: unpacked? })
	}

	/// Every declaration the run uploaded apart, by app: what a node reads before it downloads any
	/// image. See spec/architecture/host.md, "The machine pulls; nothing pushes into it".
	pub async fn declarations(
		&self,
		declared: &Declarations,
		directory: &Path,
	) -> Result<HashMap<String, String>, Error> {
		let at = (declared.repository.as_str(), declared.id, declared.digest.as_str());
		let zip = self.download(at, DECLARATIONS, directory).await?;
		let from = zip.clone();
		let read = tokio::task::spawn_blocking(move || read_declarations(&from))
			.await
			.map_err(|e| Error::Http(e.to_string()));
		let _ = tokio::fs::remove_file(&zip).await;
		read?
	}

	/// The artifact `id` of `repository`, downloaded into `directory` as a zip and held to `digest`;
	/// `name` says which in an error.
	async fn download(
		&self,
		(repository, id, digest): (&str, u64, &str),
		name: &str,
		directory: &Path,
	) -> Result<PathBuf, Error> {
		tokio::fs::create_dir_all(directory).await.map_err(io(directory.display().to_string()))?;
		// GitHub answers with a redirect to storage, which is signed and must not be sent the token.
		let uri =
			format!("{}/repos/{repository}/actions/artifacts/{id}/zip", canmi::EXTERNAL_GITHUB_API);
		let redirect = self.get(&uri, true).await?;
		let status = redirect.status().as_u16();
		let location = redirect
			.headers()
			.get(hyper::header::LOCATION)
			.and_then(|value| value.to_str().ok())
			.map(str::to_owned)
			.ok_or(Error::Status { status, what: format!("artifact {id}") })?;
		let mut response = self.get(&location, false).await?;
		let status = response.status().as_u16();
		if !(200..300).contains(&status) {
			return Err(Error::Status { status, what: format!("artifact {id} from storage") });
		}

		// Named by a counter, like an upload, so nothing GitHub answered becomes part of a path.
		let zip = crate::arrival(directory);
		let mut file = AsyncWriter::create(&zip).await.map_err(io(zip.display().to_string()))?;
		let mut hasher = Sha256::new();
		while let Some(frame) = response.body_mut().frame().await {
			let frame = frame.map_err(|e| Error::Http(e.to_string()))?;
			if let Some(data) = frame.data_ref() {
				hasher.update(data);
				file.write(data).await.map_err(io(zip.display().to_string()))?;
			}
		}
		file.finish().await.map_err(io(zip.display().to_string()))?;
		// GitHub's digest is the SHA-256 of the zip as it is downloaded; measured on run 36368010996.
		let sum = format!("sha256:{}", hex(&hasher.finalize()));
		if !sum.eq_ignore_ascii_case(digest) {
			let _ = tokio::fs::remove_file(&zip).await;
			return Err(Error::Digest(name.to_owned()));
		}
		Ok(zip)
	}
}

/// Each `<app>.toml` in the declarations zip, by app. A name no app may have is not read, and an
/// archive that is not what CI uploads is refused whole.
fn read_declarations(zip: &Path) -> Result<HashMap<String, String>, Error> {
	let shape = |why: String| Error::Shape { name: DECLARATIONS.to_owned(), why };
	let file = std::fs::File::open(zip).map_err(io(zip.display().to_string()))?;
	let mut archive = zip::ZipArchive::new(file).map_err(|e| shape(e.to_string()))?;
	if archive.len() > MOST_DECLARATIONS {
		return Err(shape(format!("{} entries, past {MOST_DECLARATIONS}", archive.len())));
	}
	let mut declared = HashMap::new();
	for index in 0..archive.len() {
		let entry = archive.by_index(index).map_err(|e| shape(e.to_string()))?;
		let Some(app) = entry.name().strip_suffix(".toml").map(str::to_owned) else { continue };
		if !(crate::manifest::check_name(&app).is_ok() || crate::manifest::OWN.contains(&app.as_str()))
		{
			continue;
		}
		if entry.size() > LARGEST_DECLARATION {
			return Err(shape(format!("`{app}.toml` weighs {} bytes", entry.size())));
		}
		let mut text = String::new();
		std::io::Read::read_to_string(&mut std::io::Read::take(entry, LARGEST_DECLARATION), &mut text)
			.map_err(|e| shape(e.to_string()))?;
		declared.insert(app, text);
	}
	Ok(declared)
}

fn hex(bytes: &[u8]) -> String {
	bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `image.tar` out to `image`, and `service.toml` as text: the two files the workflow uploads.
fn unpack(name: &str, zip: &Path, image: &Path) -> Result<String, Error> {
	let shape = |why: String| Error::Shape { name: name.to_owned(), why };
	let file = std::fs::File::open(zip).map_err(io(zip.display().to_string()))?;
	let mut archive = zip::ZipArchive::new(file).map_err(|e| shape(e.to_string()))?;
	{
		let mut entry = archive.by_name("image.tar").map_err(|e| shape(e.to_string()))?;
		let mut out = Writer::create(image).map_err(io(image.display().to_string()))?;
		std::io::copy(&mut entry, &mut out).map_err(io(image.display().to_string()))?;
		out.finish().map_err(io(image.display().to_string()))?;
	}
	let mut declaration = String::new();
	let mut entry = archive.by_name("service.toml").map_err(|e| shape(e.to_string()))?;
	std::io::Read::read_to_string(&mut entry, &mut declaration).map_err(|e| shape(e.to_string()))?;
	Ok(declaration)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn run() -> Run {
		Run {
			repository: Repository { full_name: "owner/infra".into() },
			path: WORKFLOW.into(),
			head_branch: Some("main".into()),
			event: "push".into(),
			status: "completed".into(),
			conclusion: Some("success".into()),
			head_sha: None,
		}
	}

	#[test]
	fn a_successful_deploy_run_on_main_is_one_to_deploy() {
		assert!(check(1, &run(), "owner/infra").is_ok());
		assert!(check(1, &Run { event: "schedule".into(), ..run() }, "owner/infra").is_ok());
	}

	#[test]
	fn anything_else_is_refused() {
		let refused = |record: Run| check(1, &record, "owner/infra").is_err();
		assert!(refused(Run { repository: Repository { full_name: "else/infra".into() }, ..run() }));
		assert!(refused(Run { path: ".github/workflows/other.yml".into(), ..run() }));
		assert!(refused(Run { head_branch: Some("feature".into()), ..run() }));
		assert!(refused(Run { head_branch: None, ..run() }));
		assert!(refused(Run { event: "pull_request".into(), ..run() }));
		assert!(refused(Run { status: "in_progress".into(), conclusion: None, ..run() }));
		assert!(refused(Run { conclusion: Some("failure".into()), ..run() }));
	}

	#[test]
	fn the_sources_are_the_owner_and_name_pairs_the_node_lists() {
		assert_eq!(sources(" canmi21/web\tmonoflake/infra \n"), ["canmi21/web", "monoflake/infra"]);
		assert!(sources("infra a/b/c").is_empty());
		let one = GitHub::new(String::new(), sources("monoflake/infra"), Egress::new(Vec::new()));
		assert_eq!(one.only_source(), Some("monoflake/infra"));
		let two = GitHub::new(String::new(), sources("a/b c/d"), Egress::new(Vec::new()));
		assert_eq!(two.only_source(), None);
	}

	#[test]
	fn an_artifact_names_an_app_only_when_the_name_could_be_one() {
		let of = app_of;
		let found = |app: &str, arch: &'static str| Some((app.to_owned(), arch));
		assert_eq!(of("deploy-geo-arm64"), found("geo", "arm64"));
		assert_eq!(of("deploy-geo-amd64"), found("geo", "amd64"));
		assert_eq!(of("deploy-host-amd64"), found("host", "amd64"));
		assert_eq!(of("deploy-keeper-arm64"), found("keeper", "arm64"));
		assert_eq!(of("deploy-geo"), found("geo", "arm64"));
		assert_eq!(of("deploy-host"), found("host", "arm64"));
		assert_eq!(of("deploy-my-app-amd64"), found("my-app", "amd64"));
		assert_eq!(of("deploy-my-app"), found("my-app", "arm64"));
		// A name from GitHub's answer never reaches a path, but it is refused before it could.
		assert_eq!(of("deploy-../../etc"), None);
		assert_eq!(of("deploy-../../etc-arm64"), None);
		assert_eq!(of("deploy--arm64"), None);
		assert_eq!(of("deploy-api"), None);
		assert_eq!(of("other-geo-arm64"), None);
		assert_eq!(of("other-geo"), None);
	}

	#[test]
	fn an_app_asking_for_an_architecture_is_found_in_it_whichever_this_node_runs() {
		let artifact = |app: &str, arch: &'static str| Artifact {
			app: app.into(),
			arch,
			repository: "monoflake/platform".into(),
			id: 1,
			digest: String::new(),
		};
		let built = Built {
			commit: None,
			artifacts: vec![artifact("database", "amd64"), artifact("geo", "amd64")],
			others: vec![artifact("database", "arm64")],
			declarations: None,
		};
		let found =
			|app, arch| built.built_for(app, arch).map(|artifact| (artifact.app.as_str(), artifact.arch));
		assert_eq!(found("database", "arm64"), Some(("database", "arm64")));
		assert_eq!(found("database", "amd64"), Some(("database", "amd64")));
		assert_eq!(found("geo", "arm64"), None);
		assert_eq!(found("cron", "arm64"), None);
	}

	/// A zip of `entries`, as upload-artifact makes one, in `directory`.
	fn zipped(directory: &Path, entries: &[(&str, &[u8])]) -> PathBuf {
		use std::io::Write;
		let path = directory.join("declarations.zip");
		let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
		for (name, bytes) in entries {
			zip.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
			zip.write_all(bytes).unwrap();
		}
		zip.finish().unwrap();
		path
	}

	#[test]
	fn declarations_are_read_by_app_and_nothing_else_is() {
		let directory = tempfile::tempdir().unwrap();
		let zip = zipped(
			directory.path(),
			&[
				("geo.toml", b"name = \"geo\""),
				("caddy.toml", b"name = \"caddy\""),
				("api.toml", b"reserved"),
				("../escape.toml", b"outside"),
				("readme.txt", b"not a declaration"),
			],
		);
		let declared = read_declarations(&zip).unwrap();
		let mut apps: Vec<&str> = declared.keys().map(String::as_str).collect();
		apps.sort_unstable();
		assert_eq!(apps, ["caddy", "geo"]);
		assert_eq!(declared["geo"], "name = \"geo\"");
	}

	#[test]
	fn declarations_past_what_ci_uploads_are_refused_whole() {
		let directory = tempfile::tempdir().unwrap();
		let heavy = vec![b'#'; usize::try_from(LARGEST_DECLARATION).unwrap() + 1];
		let zip = zipped(directory.path(), &[("geo.toml", &heavy)]);
		assert!(matches!(read_declarations(&zip), Err(Error::Shape { .. })));
		let not_zip = directory.path().join("plain");
		std::fs::write(&not_zip, b"not a zip").unwrap();
		assert!(matches!(read_declarations(&not_zip), Err(Error::Shape { .. })));
	}

	#[test]
	fn a_node_takes_the_architecture_its_cpu_names() {
		assert_eq!(arch_of("aarch64"), Some("arm64"));
		assert_eq!(arch_of("x86_64"), Some("amd64"));
		assert_eq!(arch_of("riscv64"), None);
	}

	#[test]
	fn a_digest_is_lowercase_hex() {
		assert_eq!(hex(&[0x00, 0xab, 0x10]), "00ab10");
	}
}
