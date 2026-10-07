//! All of Caddy, rendered from host's state: written to the file Caddy starts from, then loaded
//! through its admin socket. Never a patch, never Caddy's own autosave. See
//! spec/architecture/host.md, "host renders all of Caddy, and Caddy remembers nothing".
//!
//! The shape follows what Caddy's own adapter made of the Caddyfile this replaces: one wildcard
//! route per suffix, a guard on the source address first, and a subroute of names inside it.

use crate::config::CaddyConfig;
use crate::store::{Deployed, Route};
use deploy::manifest::{Limit, Rollout};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum Error {
	#[error("writing {path}: {source}")]
	Write { path: String, source: std::io::Error },
	#[error("Caddy's admin socket: {0}")]
	Admin(#[from] deploy::http::Error),
	#[error("Caddy refused the configuration ({status}): {body}")]
	Refused { status: u16, body: String },
}

/// Upstream, as Caddy dials it.
struct Target {
	name: String,
	dial: String,
	home: Option<String>,
	/// An app rolled out beside its predecessor; see `lingering`.
	beside: bool,
}

/// Apps Caddy dials at an address rather than by name: each whose new version runs beside the old
/// under a name of its own, until it takes the app's. An address outlasts the rename, so no
/// request finds the name between the two. See spec/architecture/host.md, "An app chooses how it is
/// rolled out, and keeping nothing earns a gapless one".
pub type Switched = HashMap<String, IpAddr>;

/// Where Caddy dials `app` at `port`: its address while it is switched, its name otherwise.
fn dial(app: &Deployed, port: u16, switched: &Switched) -> String {
	match switched.get(&app.manifest.name) {
		Some(address) => SocketAddr::new(*address, port).to_string(),
		None => format!("{}:{port}", app.manifest.name),
	}
}

/// A proxy to an app rolled out beside its predecessor keeps a socket open across the reload that
/// switches it, for as long as the version before is given to finish.
fn lingering(mut proxy: Value, beside: bool) -> Value {
	if beside {
		proxy["stream_close_delay"] = json!(format!("{}s", deploy::beside::GRACE.as_secs()));
	}
	proxy
}

fn beside(app: &Deployed) -> bool {
	app.manifest.rollout == Rollout::Beside
}

/// `host:port` is dialed as plain HTTP. `https://host[:port]` is a LAN device that speaks only TLS
/// under its own certificate, like the UniFi router: reached over TLS without verifying it, and
/// with an `Origin` that is exactly this name's own translated into the device's, since it accepts
/// a WebSocket from nowhere else. Any other origin reaches it untouched, for it to refuse. Why
/// neither is checked more strictly is spec/architecture/host.md.
fn proxy(upstream: &str, name: &str) -> Value {
	let Some(address) = upstream.strip_prefix("https://") else {
		return json!({ "handler": "reverse_proxy", "upstreams": [{ "dial": upstream }] });
	};
	let address = address.trim_end_matches('/');
	let dial = if address.contains(':') { address.to_owned() } else { format!("{address}:443") };
	let own = format!("^https://{}$", name.replace('.', "\\."));
	json!({
		"handler": "reverse_proxy",
		"upstreams": [{ "dial": dial }],
		"transport": { "protocol": "http", "tls": { "insecure_skip_verify": true } },
		"headers": { "request": { "replace": { "Origin": [
			{ "search_regexp": own, "replace": format!("https://{address}") }
		]}}}
	})
}

/// Compression for every answer Caddy passes on, so no app has to compress for itself; one that
/// arrives encoded already is passed through. See spec/architecture/host.md, "Every name is
/// compressed at Caddy, and no app compresses for itself".
fn encode() -> Value {
	json!({ "handler": "encode", "encodings": { "zstd": {}, "gzip": {} }, "prefer": ["zstd", "gzip"] })
}

/// One name, proxied to its upstream; a request for exactly `/` goes to the target's home first
/// when it has one.
fn named(host: String, target: &Target) -> Value {
	let mut routes = Vec::new();
	if let Some(home) = &target.home {
		routes.push(json!({
			"match": [{ "path": ["/"] }],
			"handle": [{ "handler": "static_response", "status_code": 307, "headers": { "Location": [home] } }]
		}));
	}
	let proxied = lingering(proxy(&target.dial, &host), target.beside);
	routes.push(json!({ "handle": [encode(), proxied] }));
	json!({ "match": [{ "host": [host] }], "handle": [{ "handler": "subroute", "routes": routes }] })
}

fn abort() -> Value {
	json!({ "handle": [{ "handler": "static_response", "abort": true }] })
}

fn refuse_unless(sources: &[String]) -> Value {
	json!({
		"match": [{ "not": [{ "remote_ip": { "ranges": sources } }] }],
		"handle": [{ "handler": "static_response", "abort": true }]
	})
}

/// The gateway's mark, on what it forwards from the public; the same pair as `MARK` in the gateway.
const MARK: (&str, &str) = ("X-Gateway", "public");

/// `path` as a regular expression matching itself and nothing else.
fn escaped(path: &str) -> String {
	path.chars().fold(String::new(), |mut out, character| {
		if "\\.+*?()|[]{}^$".contains(character) {
			out.push('\\');
		}
		out.push(character);
		out
	})
}

/// A scope's limits, counted by the visitor's address on what the gateway forwards, and on nothing
/// else: our own callers meet none. One zone a row, named as the gateway's counters are without the
/// address. See platform's spec/architecture/services.md, "A limit is declared once and kept in
/// three places".
fn limited(scope: &str, limits: &[Limit]) -> Option<Value> {
	if limits.is_empty() {
		return None;
	}
	// Caddy sees an address and no account, so it keeps a floor under the address's rows alone.
	let zones: serde_json::Map<String, Value> = limits
		.iter()
		.filter(|limit| limit.subject() == "address")
		.map(|limit| {
			let methods: Vec<String> = limit.methods.iter().map(|method| method.to_lowercase()).collect();
			let named = limit.path.replace("/*", "/any");
			let path = named.split('/').filter(|part| !part.is_empty()).collect::<Vec<_>>();
			let path = if path.is_empty() { "root".to_owned() } else { path.join("-") };
			// A limit names the path after the version, so it counts every version's call to it, and
			// the unversioned one until its callers move; a prefix ending in `/*` counts everything
			// under it. See platform's spec/architecture/gateway.md, "The declaration".
			let pattern = match limit.path.strip_suffix('*') {
				Some(stem) => format!("^(/v[1-9][0-9]*)?{}", escaped(stem)),
				None => format!("^(/v[1-9][0-9]*)?{}$", escaped(&limit.path)),
			};
			let zone = json!({
				"match": [{
					"method": limit.methods,
					"path_regexp": { "pattern": pattern },
					"header": { MARK.0: [MARK.1] }
				}],
				// The address Caddy took from Cloudflare's header; `http.request.client_ip` is no
				// placeholder, and a key that does not resolve counts everyone as one.
				"key": "{http.vars.client_ip}",
				// A sliding window cannot be a bucket; it allows the most the bucket ever admits in
				// one, so the floor never refuses what `quota` lets through. See
				// platform's spec/architecture/quota.md, "Who asks".
				"window": format!("{}s", limit.seconds),
				"max_events": limit.most_in_window()
			});
			(format!("{scope}_{}_{path}", methods.join("-")), zone)
		})
		.collect();
	Some(json!({ "handler": "rate_limit", "rate_limits": zones }))
}

/// What a refused call is answered with: the one envelope, never kept by the gateway's cache. The
/// limiter has already said when to try again.
fn refused() -> Value {
	let body = serde_json::to_string(&response::Envelope::error("rate_limited")).unwrap_or_default();
	json!({
		"match": [{ "expression": "{http.error.status_code} == 429" }],
		"handle": [{
			"handler": "static_response",
			"status_code": 429,
			"headers": {
				"Content-Type": ["application/json"],
				"Cache-Control": ["no-store"]
			},
			"body": body
		}]
	})
}

/// One of Caddy's sides: the private one, which the LAN and the node's own containers ask, and the
/// tunnel's, which only the gateway reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
	Private,
	Tunnel,
}

impl Side {
	/// As a declaration's `sides` names it.
	fn name(self) -> &'static str {
		match self {
			Side::Private => "private",
			Side::Tunnel => "tunnel",
		}
	}
}

/// The header the private side sends `INTERNAL_TOKEN` to the public gateway in; the same name as
/// `INTERNAL_HEADER` in the gateway.
const INTERNAL_HEADER: &str = "X-Internal";

/// Each API scope a side carries, its prefix stripped before the service sees the request; what the
/// public may reach of it is the gateway's table, not this render. Limits are only ever counted on
/// the tunnel's side, over what the gateway forwards. See platform's spec/architecture/services.md,
/// "A path with no scope is a 400, on both gateways".
fn scopes(apps: &[Deployed], side: Side, switched: &Switched) -> Vec<Value> {
	scopes_then(
		apps,
		side,
		switched,
		json!({ "handle": [{ "handler": "static_response", "status_code": 404 }] }),
	)
}

/// `scopes`, with `rest` in place of the 404 for a scope not deployed here.
fn scopes_then(apps: &[Deployed], side: Side, switched: &Switched, rest: Value) -> Vec<Value> {
	let mut routes = vec![json!({
		"match": [{ "path": ["/"] }],
		"handle": [{ "handler": "static_response", "status_code": 400 }]
	})];
	routes.extend(apps.iter().filter_map(|app| {
		let name = &app.manifest.name;
		let api = app.manifest.api.as_ref().filter(|api| api.carried_on(side.name()))?;
		let port = app.manifest.container.as_ref()?.port?;
		let mut handle = vec![json!({ "handler": "rewrite", "strip_path_prefix": format!("/{name}") })];
		handle.extend((side == Side::Tunnel).then(|| limited(name, &api.limits)).flatten());
		let proxied = lingering(proxy(&dial(app, port, switched), name), beside(app));
		handle.extend([encode(), proxied]);
		Some(json!({
			"match": [{ "path": [format!("/{name}"), format!("/{name}/*")] }],
			"handle": handle
		}))
	}));
	routes.push(rest);
	routes
}

fn api_host(host: String, apps: &[Deployed], side: Side, switched: &Switched) -> Value {
	json!({
		"match": [{ "host": [host] }],
		"handle": [{ "handler": "subroute", "routes": scopes(apps, side, switched) }]
	})
}

/// A private path's scope, and the version after it when it names one.
const VERSIONED: &str = "^/([a-z][a-z0-9-]*)/(v[1-9][0-9]*)(/.*)?$";
const UNVERSIONED: &str = "^/([a-z][a-z0-9-]*)(/.*)?$";

/// A scope not deployed on this node, handed to the public gateway: `/{scope}/v{n}/...` as
/// `/v{n}/{scope}/...`, a path naming no version as `v1`'s. Only a private scope, one of
/// `private_scopes`, carries `INTERNAL_TOKEN` in `X-Internal`; a public one goes as any caller's.
fn to_public_gateway(config: &CaddyConfig) -> Value {
	let rewrite = |pattern: &str, replace: &str| {
		json!({
			"group": "version",
			"match": [{ "path_regexp": { "pattern": pattern } }],
			"handle": [{ "handler": "rewrite", "path_regexp": [{ "find": pattern, "replace": replace }] }]
		})
	};
	let host = &config.public_api;
	let mut routes = Vec::new();
	if !config.private_scopes.is_empty() {
		let paths: Vec<String> = config
			.private_scopes
			.iter()
			.flat_map(|scope| [format!("/{scope}"), format!("/{scope}/*")])
			.collect();
		routes.push(json!({
			"match": [{ "path": paths }],
			"handle": [{ "handler": "headers", "request": {
				"set": { INTERNAL_HEADER: ["{env.INTERNAL_TOKEN}"] }
			}}]
		}));
	}
	routes.extend([rewrite(VERSIONED, "/$2/$1$3"), rewrite(UNVERSIONED, "/v1/$1$2")]);
	routes.push(json!({ "handle": [
			encode(),
			{
				"handler": "reverse_proxy",
				"upstreams": [{ "dial": format!("{host}:443") }],
				"transport": { "protocol": "http", "tls": { "server_name": host } },
				"headers": { "request": { "set": { "Host": [host] } } }
			}
	]}));
	json!({ "handle": [{ "handler": "subroute", "routes": routes }] })
}

/// The private API host, which Caddy answers by inside every app's network too.
pub fn private_api_host(config: &CaddyConfig) -> String {
	format!("api.{}", config.private_suffix)
}

/// The private API host as a container on this node asks it: plain HTTP on Caddy's own port 80,
/// which the name resolves to inside every app's network. A scope deployed here is answered by its
/// service; any other goes on to the public gateway. Only the app networks reach it, never the
/// LAN, the tailnet or the tunnel, and a caller's own `X-Internal` is dropped. See
/// spec/architecture/host.md, "Every node answers the private API, and sends on what is not its
/// own".
fn private_api(config: &CaddyConfig, apps: &[Deployed], switched: &Switched) -> Value {
	let host = private_api_host(config);
	let tunnel = json!({
		"match": [{ "remote_ip": { "ranges": [config.tunnel_source] } }],
		"handle": [{ "handler": "static_response", "abort": true }]
	});
	let unclaimed = json!({ "handle": [{ "handler": "headers", "request": {
		"delete": [INTERNAL_HEADER, "Cf-Connecting-Ip"]
	}}]});
	let mut routes = vec![tunnel, refuse_unless(&config.app_sources), unclaimed];
	routes.extend(scopes_then(apps, Side::Private, switched, to_public_gateway(config)));
	json!({
		"match": [{ "host": [host] }],
		"handle": [{ "handler": "subroute", "routes": routes }],
		"terminal": true
	})
}

/// The label host's door answers on, the one the panel answered on before it retired. Reserved, so
/// no app or route takes it.
pub const DOOR: &str = "infra";

/// What the door passes on to host: CI's notice, and the reads the console asks a node for. Every
/// write is asked over the tailnet instead. See spec/architecture/host.md, "host has no interface
/// on the node, and a door Caddy keeps".
const DOOR_READS: &str = concat!(
	"^/api/(node/(now|series)|apps|apps/[^/]+|apps/[^/]+/(history|health|metrics/series)|events",
	"|inspect/disk)$"
);

/// host at `<DOOR>.<suffix>`, by the allowlist above and nothing else: a 404 for the rest. Caddy
/// adds no authentication; host's own tokens are the only check, carried through untouched. host
/// is dialed by its name on its own network; see `deploy::engine::on_own_network`.
fn door(name: String, host: &str) -> Value {
	json!({
		"match": [{ "host": [name.clone()] }],
		"handle": [{ "handler": "subroute", "routes": [
			{
				"match": [{ "method": ["POST"], "path": ["/notice"] }],
				"handle": [encode(), proxy(host, &name)]
			},
			{
				"match": [{ "method": ["GET"], "path_regexp": { "pattern": DOOR_READS } }],
				"handle": [encode(), proxy(host, &name)]
			},
			{ "handle": [{ "handler": "static_response", "status_code": 404 }] }
		]}]
	})
}

/// Everything reached by a subdomain of its own on one side, by its label: each app with an
/// interface, and each route. host has none of its own, and is reached by `door` alone. A stale
/// app still holding the door's label is passed over, and said so.
/// Every label is on `.app`; the private suffix carries it only when the interface's `lan`, or the
/// route's `private`, says so. See spec/architecture/host.md, "The private suffix is a mirror of
/// part of `.app`, and nothing else".
fn interfaces(
	apps: &[Deployed],
	routes: &[Route],
	public: bool,
	switched: &Switched,
) -> Vec<Target> {
	let mut targets = Vec::new();
	targets.extend(
		apps
			.iter()
			.filter(|app| {
				app.manifest.interface.as_ref().is_some_and(|interface| public || interface.lan)
			})
			.filter_map(|app| {
				let interface = app.manifest.interface.as_ref()?;
				Some(Target {
					name: interface.label(&app.manifest.name).to_owned(),
					dial: dial(app, app.manifest.container.as_ref()?.port?, switched),
					home: interface.home.clone(),
					beside: beside(app),
				})
			}),
	);
	targets.extend(routes.iter().filter(|route| public || route.private).map(|route| Target {
		name: route.name.clone(),
		dial: route.upstream.clone(),
		home: route.home.clone(),
		beside: false,
	}));
	targets.retain(|target| {
		let stale = target.name == DOOR;
		if stale {
			eprintln!("host: `{}` answers on `{DOOR}`, which is host's door; not routed", target.dial);
		}
		!stale
	});
	targets
}

/// The LAN's side, and the certificate policy that serves it.
fn private_side(
	config: &CaddyConfig,
	apps: &[Deployed],
	routes: &[Route],
	trusted: &Value,
	switched: &Switched,
) -> (Value, Value) {
	let private = &config.private_suffix;

	let mut inside = vec![refuse_unless(&config.private_sources)];
	inside.push(api_host(format!("api.{private}"), apps, Side::Private, switched));
	inside.push(door(format!("{DOOR}.{private}"), &config.host));
	for target in interfaces(apps, routes, false, switched) {
		inside.push(named(format!("{}.{private}", target.name), &target));
	}
	inside.push(abort());

	let lan = vec![json!({
		"match": [{ "host": [format!("*.{private}")] }],
		"handle": [{ "handler": "subroute", "routes": inside }],
		"terminal": true
	})];
	let subjects = vec![format!("*.{private}")];

	let server = json!({
		"listen": [":443"],
		"routes": lan,
		"trusted_proxies": trusted,
		"client_ip_headers": ["Cf-Connecting-Ip"]
	});
	let tls = json!({ "automation": { "policies": [{
		"subjects": subjects,
		"issuers": [{
			"module": "acme",
			"email": config.acme_email,
			"challenges": { "dns": {
				"provider": { "name": "cloudflare", "api_token": "{env.CLOUDFLARE_API_TOKEN}" },
				"resolvers": [config.dns_resolver]
			}}
		}]
	}]}});
	(server, tls)
}

/// `lan` is whether the node has one; without it there is no LAN side and no certificate, and the
/// node's containers still ask the private API host on port 80. See spec/architecture/host.md,
/// "Caddy is deployed like any app, and is the one door".
#[cfg(test)]
pub fn render(config: &CaddyConfig, apps: &[Deployed], routes: &[Route], lan: bool) -> Value {
	render_switched(config, apps, routes, lan, &Switched::new())
}

/// `render`, with the apps `switched` names dialed at their address.
pub fn render_switched(
	config: &CaddyConfig,
	apps: &[Deployed],
	routes: &[Route],
	lan: bool,
	switched: &Switched,
) -> Value {
	let public = &config.public_suffix;

	let mut outside = vec![refuse_unless(std::slice::from_ref(&config.tunnel_source))];
	outside.push(api_host(format!("api.{public}"), apps, Side::Tunnel, switched));
	outside.push(door(format!("{DOOR}.{public}"), &config.host));
	for target in interfaces(apps, routes, true, switched) {
		outside.push(named(format!("{}.{public}", target.name), &target));
	}
	outside.push(json!({ "handle": [{ "handler": "static_response", "status_code": 404 }] }));

	// The visitor's address comes from Cloudflare's header, and only when cloudflared sent it.
	let trusted = json!({ "source": "static", "ranges": [config.tunnel_source] });
	let private = lan.then(|| private_side(config, apps, routes, &trusted, switched));

	let mut servers = serde_json::Map::new();
	if let Some((server, _)) = &private {
		servers.insert("private".into(), server.clone());
	}
	servers.insert(
		"tunnel".into(),
		json!({
			"listen": [":80"],
			"routes": [private_api(config, apps, switched), {
				"match": [{ "host": [format!("*.{public}")] }],
				"handle": [{ "handler": "subroute", "routes": outside }],
				"terminal": true
			}],
			"errors": { "routes": [refused()] },
			"trusted_proxies": trusted,
			"client_ip_headers": ["Cf-Connecting-Ip"]
		}),
	);

	let mut caddy = serde_json::Map::new();
	caddy.insert("http".into(), json!({ "servers": servers }));
	if let Some((_, tls)) = private {
		caddy.insert("tls".into(), tls);
	}
	json!({
		"admin": { "listen": config.admin_listen, "config": { "persist": false } },
		"apps": caddy
	})
}

/// Write the file first, then load it: a Caddy that restarts before host is back starts from what
/// was last applied rather than from nothing.
pub async fn apply(config: &CaddyConfig, rendered: &Value) -> Result<(), Error> {
	let bytes = serde_json::to_vec_pretty(rendered).unwrap_or_default();
	write(&config.config_file, &bytes).await?;
	let (status, body) = deploy::http::post_unix(&config.admin_socket, "/load", bytes).await?;
	if status >= 300 {
		return Err(Error::Refused { status, body });
	}
	Ok(())
}

/// Through a temporary file and a rename, so neither Caddy nor the resolver ever starts from half
/// of one.
pub async fn write(path: &Path, bytes: &[u8]) -> Result<(), Error> {
	let failed = |source| Error::Write { path: path.display().to_string(), source };
	if let Some(parent) = path.parent() {
		tokio::fs::create_dir_all(parent).await.map_err(failed)?;
	}
	let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
	let temporary = path.with_file_name(format!("{name}.next"));
	tokio::fs::write(&temporary, bytes).await.map_err(failed)?;
	tokio::fs::rename(&temporary, path).await.map_err(failed)
}

#[cfg(test)]
mod tests {
	use super::*;
	use deploy::Manifest;

	fn config() -> CaddyConfig {
		CaddyConfig {
			container: "caddy".into(),
			host: deploy::engine::on_own_network("host", crate::config::PORT),
			admin_socket: "/nowhere/admin.sock".into(),
			config_file: "/nowhere/caddy.json".into(),
			admin_listen: "unix//data/admin.sock".into(),
			private_suffix: "inside.test".into(),
			public_suffix: "outside.test".into(),
			private_sources: vec!["10.0.0.0/24".into()],
			tunnel_source: "172.30.0.20".into(),
			acme_email: "someone@example.com".into(),
			dns_resolver: "1.1.1.1".into(),
			public_api: "api.public.test".into(),
			app_sources: vec!["172.16.0.0/12".into()],
			private_scopes: vec!["ledger".into()],
		}
	}

	fn geo() -> Deployed {
		Deployed {
			manifest: Manifest::parse(include_str!("../../../../libs/deploy/fixtures/geo.toml")).unwrap(),
			image: "sha256:a".into(),
			previous: None,
			deployed_at: String::new(),
			held: false,
		}
	}

	fn text(value: &Value) -> String {
		serde_json::to_string(value).unwrap()
	}

	#[test]
	fn an_api_scope_answers_on_both_api_hosts_regardless_of_its_public_flag() {
		let mut private = geo();
		private.manifest.api.as_mut().unwrap().public = false;
		let rendered = text(&render(&config(), &[private], &[], true));
		assert!(rendered.contains(r#""host":["api.inside.test"]"#));
		assert!(rendered.contains(r#""host":["api.outside.test"]"#));
		assert!(rendered.contains(r#""path":["/geo","/geo/*"]"#));
		assert!(rendered.contains(r#""strip_path_prefix":"/geo""#));
		// geo declares no interface, so a private scope still reaches nothing of its own on either
		// suffix -- the API host alone carries it, on both sides. `api.public` is the gateway's scope
		// table's alone; see platform's spec/architecture/services.md, "One API host, scoped by path".
		assert!(!rendered.contains("geo.outside.test"));
		assert!(!rendered.contains("geo.inside.test"));
		// The LAN's side, the tunnel's, and the private API host the node's containers ask.
		assert_eq!(rendered.matches(r#""dial":"geo:23440""#).count(), 3);
	}

	#[test]
	fn a_switched_app_is_dialed_at_its_address_and_a_beside_one_keeps_its_streams() {
		let mut beside = geo();
		beside.manifest.rollout = deploy::manifest::Rollout::Beside;
		beside.manifest.interface =
			Some(deploy::manifest::Interface { domain: None, lan: true, home: None });
		let switched = super::Switched::from([("geo".to_owned(), [172, 18, 0, 7].into())]);
		let apps = [beside];
		let rendered = text(&super::render_switched(&config(), &apps, &[], true, &switched));
		// Both API hosts, the private one the node's containers ask, and the label on both sides.
		assert_eq!(rendered.matches(r#""dial":"172.18.0.7:23440""#).count(), 5);
		assert!(!rendered.contains(r#""dial":"geo:23440""#));
		assert_eq!(rendered.matches(r#""stream_close_delay":"30s""#).count(), 5);
		// Unswitched, it is dialed by its name again, and still keeps its streams.
		let named = text(&render(&config(), &apps, &[], true));
		assert_eq!(named.matches(r#""dial":"geo:23440""#).count(), 5);
		assert_eq!(named.matches(r#""stream_close_delay":"30s""#).count(), 5);
		// An app replaced in place renders as it always did.
		assert!(!text(&render(&config(), &[geo()], &[], true)).contains("stream_close_delay"));
	}

	#[test]
	fn a_public_scope_is_on_the_tunnels_api_host_too() {
		let rendered = render(&config(), &[geo()], &[], true);
		let tunnel = &rendered["apps"]["http"]["servers"]["tunnel"]["routes"][1]["handle"][0]["routes"];
		assert_eq!(tunnel[1]["match"][0]["host"][0], "api.outside.test");
		assert_eq!(text(&rendered).matches(r#""dial":"geo:23440""#).count(), 3);
	}

	#[test]
	fn a_limit_counts_what_the_gateway_forwards_on_the_tunnels_side_alone() {
		let rendered = render(&config(), &[geo()], &[], true);
		let servers = &rendered["apps"]["http"]["servers"];
		let tunnel = &servers["tunnel"]["routes"][1]["handle"][0]["routes"][1]["handle"][0]["routes"];
		let handle = &tunnel[1]["handle"];
		assert_eq!(handle[0]["handler"], "rewrite");
		assert_eq!(handle[1]["handler"], "rate_limit");
		let zone = &handle[1]["rate_limits"]["geo_get-head_address"];
		assert_eq!(text(&zone["match"][0]["method"]), r#"["GET","HEAD"]"#);
		let pattern = regex::Regex::new(zone["match"][0]["path_regexp"]["pattern"].as_str().unwrap());
		let pattern = pattern.unwrap();
		assert!(pattern.is_match("/address") && pattern.is_match("/v1/address"));
		assert!(!pattern.is_match("/address/x") && !pattern.is_match("/v1x/address"));
		assert_eq!(zone["match"][0]["header"]["X-Gateway"][0], "public");
		assert_eq!(zone["key"], "{http.vars.client_ip}");
		// geo's row is 60 a minute with no burst named: a full bucket of 60, and 60 more meanwhile.
		assert_eq!((zone["window"].as_str(), zone["max_events"].as_u64()), (Some("60s"), Some(120)));
		// The LAN and the tailnet meet no limit.
		assert!(!text(&servers["private"]).contains(r#""handler":"rate_limit""#));
		// A refusal is the envelope, and the gateway keeps none of it.
		let refused = &servers["tunnel"]["errors"]["routes"][0]["handle"][0];
		assert_eq!(refused["status_code"], 429);
		assert_eq!(refused["headers"]["Cache-Control"][0], "no-store");
		assert!(refused["body"].as_str().unwrap().contains(r#""code":"rate_limited""#));
	}

	#[test]
	fn a_scope_with_no_limits_has_no_limiter() {
		let mut free = geo();
		free.manifest.api.as_mut().unwrap().limits.clear();
		assert!(!text(&render(&config(), &[free], &[], true)).contains(r#""handler":"rate_limit""#));
	}

	#[test]
	fn a_container_asks_its_own_node_and_the_public_gateway_for_the_rest_with_or_without_a_lan() {
		for lan in [true, false] {
			let rendered = render(&config(), &[geo()], &[], lan);
			let first = &rendered["apps"]["http"]["servers"]["tunnel"]["routes"][0];
			assert_eq!(first["match"][0]["host"][0], "api.inside.test");
			let routes = &first["handle"][0]["routes"];
			// Never the tunnel, and only the app networks: the LAN's and the tailnet's sources,
			// `private_sources`, are refused here and keep the LAN side.
			assert_eq!(routes[0]["match"][0]["remote_ip"]["ranges"][0], "172.30.0.20");
			assert_eq!(routes[0]["handle"][0]["abort"], true);
			let admitted = &routes[1]["match"][0]["not"][0]["remote_ip"]["ranges"];
			assert_eq!(text(admitted), r#"["172.16.0.0/12"]"#);
			assert!(!text(admitted).contains("10.0.0.0/24"));
			assert_eq!(routes[1]["handle"][0]["abort"], true);
			// A caller's own token, and Cloudflare's header, are dropped before anything else.
			let dropped = &routes[2]["handle"][0]["request"]["delete"];
			assert_eq!(text(dropped), r#"["X-Internal","Cf-Connecting-Ip"]"#);
			assert!(text(routes).contains(r#""dial":"geo:23440""#));
			let rest = &routes.as_array().unwrap().last().unwrap()["handle"][0]["routes"];
			let proxy = &rest.as_array().unwrap().last().unwrap()["handle"][1];
			assert_eq!(proxy["upstreams"][0]["dial"], "api.public.test:443");
			assert_eq!(proxy["transport"]["tls"]["server_name"], "api.public.test");
			assert_eq!(proxy["headers"]["request"]["set"]["Host"][0], "api.public.test");
		}
	}

	#[test]
	fn a_scope_on_the_tunnel_alone_is_on_no_private_host() {
		let mut tunnel = geo();
		tunnel.manifest.api.as_mut().unwrap().sides = Some(vec!["tunnel".into()]);
		let rendered = render(&config(), &[tunnel], &[], true);
		let servers = &rendered["apps"]["http"]["servers"];
		assert!(!text(&servers["private"]).contains("geo:23440"));
		assert!(!text(&servers["tunnel"]["routes"][0]).contains("geo:23440"));
		assert!(text(&servers["tunnel"]["routes"][1]).contains("geo:23440"));
	}

	#[test]
	fn only_a_private_scope_is_forwarded_with_the_token() {
		let rest = to_public_gateway(&config());
		let routes = rest["handle"][0]["routes"].as_array().unwrap();
		let setting: Vec<&Value> =
			routes.iter().filter(|route| text(route).contains("{env.INTERNAL_TOKEN}")).collect();
		assert_eq!(setting.len(), 1);
		let paths = text(&setting[0]["match"][0]["path"]);
		assert_eq!(paths, r#"["/ledger","/ledger/*"]"#);
		assert_eq!(setting[0]["handle"][0]["request"]["set"]["X-Internal"][0], "{env.INTERNAL_TOKEN}");
		// A node naming no private scope sends the token nowhere.
		let none = CaddyConfig { private_scopes: vec![], ..config() };
		assert!(!text(&to_public_gateway(&none)).contains("INTERNAL_TOKEN"));
	}

	#[test]
	fn a_private_path_becomes_the_public_gateways_with_its_version_or_v1() {
		let rest = to_public_gateway(&config());
		let rewrites: Vec<Value> = rest["handle"][0]["routes"]
			.as_array()
			.unwrap()
			.iter()
			.filter(|route| route.get("group").is_some())
			.cloned()
			.collect();
		assert_eq!(rewrites.len(), 2);
		// One group: the first rewrite that matches is the only one applied.
		let rewrite = |path: &str| -> String {
			for route in &rewrites {
				assert_eq!(route["group"], "version");
				let rule = &route["handle"][0]["path_regexp"][0];
				let find = regex::Regex::new(rule["find"].as_str().unwrap()).unwrap();
				if find.is_match(path) {
					return find.replace(path, rule["replace"].as_str().unwrap()).into_owned();
				}
			}
			path.to_owned()
		};
		assert_eq!(rewrite("/shot/v1/tasks/abc"), "/v1/shot/tasks/abc");
		assert_eq!(rewrite("/geo/v2"), "/v2/geo");
		assert_eq!(rewrite("/ledger/events"), "/v1/ledger/events");
		assert_eq!(rewrite("/ledger"), "/v1/ledger");
		assert_eq!(rewrite("/cron/v1x/run"), "/v1/cron/v1x/run");
	}

	#[test]
	fn a_path_with_no_scope_is_malformed() {
		let first = &scopes(&[geo()], Side::Private, &super::Switched::new())[0];
		assert_eq!(first["match"][0]["path"][0], "/");
		assert_eq!(first["handle"][0]["status_code"], 400);
	}

	#[test]
	fn every_proxied_answer_is_compressed_first() {
		let nas = Route {
			name: "nas".into(),
			upstream: "10.0.0.21:80".into(),
			private: true,
			public: true,
			home: None,
		};
		// Every list of handlers that proxies has the encoder immediately before the proxy.
		fn check(value: &Value, proxies: &mut usize) {
			match value {
				Value::Array(items) => {
					for (index, item) in items.iter().enumerate() {
						if item["handler"] == "reverse_proxy" {
							*proxies += 1;
							assert!(index > 0 && items[index - 1]["handler"] == "encode", "{items:?}");
						}
						check(item, proxies);
					}
				}
				Value::Object(fields) => fields.values().for_each(|field| check(field, proxies)),
				_ => {}
			}
		}
		let mut proxies = 0;
		check(&render(&config(), &[geo()], &[nas], true), &mut proxies);
		assert!(proxies >= 3);
	}

	#[test]
	fn the_source_guard_comes_first_on_both_sides() {
		let rendered = render(&config(), &[], &[], true);
		let servers = &rendered["apps"]["http"]["servers"];
		for (server, route, source) in [("private", 0, "10.0.0.0/24"), ("tunnel", 1, "172.30.0.20")] {
			let first = &servers[server]["routes"][route]["handle"][0]["routes"][0];
			assert_eq!(first["match"][0]["not"][0]["remote_ip"]["ranges"][0], source);
		}
	}

	#[test]
	fn a_route_appears_on_the_sides_it_asks_for() {
		let nas = Route {
			name: "nas".into(),
			upstream: "10.0.0.21:80".into(),
			private: false,
			public: true,
			home: None,
		};
		let rendered = text(&render(&config(), &[], &[nas], true));
		assert!(rendered.contains("nas.outside.test"));
		assert!(!rendered.contains("nas.inside.test"));
		// host is on neither: the panel is its only way in.
		assert!(!rendered.contains("host.inside.test") && !rendered.contains("host.outside.test"));
	}

	#[test]
	fn an_apps_declared_home_redirects_its_root() {
		let text = include_str!("../../../../libs/deploy/fixtures/gemini.toml");
		let gemini = Deployed {
			manifest: Manifest::parse(text).unwrap(),
			image: "sha256:g".into(),
			previous: None,
			deployed_at: String::new(),
			held: false,
		};
		let rendered = super::tests::text(&render(&config(), &[gemini], &[], true));
		assert!(rendered.contains(r#""Location":["/admin"]"#));
		assert_eq!(rendered.matches(r#""dial":"gemini:20830""#).count(), 2);
	}

	#[test]
	fn a_home_redirects_the_root_and_nothing_else() {
		let gemini = Route {
			name: "gemini".into(),
			upstream: "gemini.test:8083".into(),
			private: true,
			public: true,
			home: Some("/admin".into()),
		};
		let rendered = text(&render(&config(), &[], &[gemini], true));
		assert!(rendered.contains(r#""match":[{"path":["/"]}]"#));
		assert!(rendered.contains(r#""Location":["/admin"]"#));
		assert!(rendered.contains(r#""status_code":307"#));
		// Everything past the root still reaches the application, on both sides.
		assert_eq!(rendered.matches(r#""dial":"gemini.test:8083""#).count(), 2);
	}

	#[test]
	fn an_https_upstream_is_reached_over_tls_on_443_unless_it_names_a_port() {
		let unifi = Route {
			name: "unifi".into(),
			upstream: "https://device.test".into(),
			private: false,
			public: true,
			home: None,
		};
		let rendered = text(&render(&config(), &[], &[unifi], true));
		assert!(rendered.contains(r#""dial":"device.test:443""#));
		assert!(rendered.contains(r#""tls":{"insecure_skip_verify":true}"#));
		assert_eq!(
			text(&proxy("https://device.test:8443/", "unifi.outside.test")["upstreams"]),
			r#"[{"dial":"device.test:8443"}]"#
		);
		// A plain upstream carries no transport and no rewriting at all.
		let plain = proxy("10.0.0.21:80", "nas.outside.test");
		assert!(plain.get("transport").is_none() && plain.get("headers").is_none());
	}

	#[test]
	fn only_the_names_own_origin_is_translated_for_a_device() {
		let rendered = proxy("https://device.test", "unifi.outside.test");
		let rule = &rendered["headers"]["request"]["replace"]["Origin"][0];
		let own = regex::Regex::new(rule["search_regexp"].as_str().unwrap()).unwrap();
		assert!(own.is_match("https://unifi.outside.test"));
		// Anchored at both ends and with its dots escaped, so neither a longer name nor a lookalike
		// with any character in place of a dot is taken for this one.
		assert!(!own.is_match("https://unifi.outside.test.evil.test"));
		assert!(!own.is_match("https://unifiXoutside.test"));
		assert!(!own.is_match("http://unifi.outside.test"));
		assert_eq!(rule["replace"], "https://device.test");
	}

	#[test]
	fn keepers_whole_interface_is_on_both_suffixes() {
		let keeper = Deployed {
			manifest: Manifest::parse(include_str!("../../keeper/service.toml")).unwrap(),
			image: "sha256:k".into(),
			previous: None,
			deployed_at: String::new(),
			held: false,
		};
		// keeper's whole interface is on `.app` now, behind Access like any other -- not the one
		// `/notice` path the Worker used to reach it by. See spec/architecture/host.md, "The private
		// suffix is a mirror of part of `.app`, and nothing else".
		let rendered = text(&render(&config(), &[keeper], &[], true));
		assert!(rendered.contains(r#"{"host":["keeper.outside.test"]}"#));
		assert!(rendered.contains(r#"{"host":["keeper.inside.test"]}"#));
		assert_eq!(rendered.matches(r#""dial":"keeper:11010""#).count(), 2);
	}

	/// The door's routes on the side whose servers are named `server`, under `name`.
	fn door_on(rendered: &Value, server: &str, name: &str) -> Value {
		let side = &rendered["apps"]["http"]["servers"][server]["routes"];
		let names = side
			.as_array()
			.unwrap()
			.iter()
			.flat_map(|route| route["handle"][0]["routes"].as_array().cloned().unwrap_or_default());
		let found = names.into_iter().find(|route| route["match"][0]["host"][0] == name);
		found.unwrap_or_else(|| panic!("no {name} on {server}"))["handle"][0]["routes"].clone()
	}

	#[test]
	fn the_door_passes_the_notice_and_the_reads_to_host_and_nothing_else() {
		let rendered = full(true);
		for (server, name) in [("tunnel", "infra.outside.test"), ("private", "infra.inside.test")] {
			let routes = door_on(&rendered, server, name);
			assert_eq!(routes[0]["match"][0], json!({ "method": ["POST"], "path": ["/notice"] }));
			assert_eq!(routes[1]["match"][0]["method"], json!(["GET"]));
			assert_eq!(routes[2]["handle"][0]["status_code"], 404);
			assert_eq!(text(&routes).matches(r#""dial":"host.app-host:11011""#).count(), 2);
			// No token of Caddy's own: what the caller sent reaches host as it was.
			assert!(!text(&routes).contains("Authorization"));
		}
		let reads = regex::Regex::new(DOOR_READS).unwrap();
		for path in [
			"/api/node/now",
			"/api/node/series",
			"/api/apps",
			"/api/apps/geo",
			"/api/apps/geo/history",
			"/api/apps/geo/health",
			"/api/apps/geo/metrics/series",
			"/api/events",
			"/api/inspect/disk",
		] {
			assert!(reads.is_match(path), "{path}");
		}
		for path in [
			"/api/session",
			"/api/apps/geo/restart",
			"/api/apps/geo/environment",
			"/api/apps/geo/logs",
			"/api/inspect/files/geo",
			"/api/caddy",
			"/api/routes",
			"/api/apps/geo/history/x",
			"/notice",
		] {
			assert!(!reads.is_match(path), "{path}");
		}
	}

	#[test]
	fn the_door_wins_over_an_app_that_still_holds_its_label() {
		let panel = Deployed {
			manifest: Manifest::parse(concat!(
				"version = 1\nname = \"panel\"\nplacements = [\"rdu\"]\n",
				"[container]\nport = 26519\nhealth = \"/health\"\n",
				"[interface]\ndomain = \"geo-ui\"\n"
			))
			.unwrap(),
			image: "sha256:p".into(),
			previous: None,
			deployed_at: String::new(),
			held: false,
		};
		// As a store from before the label was reserved holds it, which no check would now take.
		let mut stale = panel.clone();
		stale.manifest.interface.as_mut().unwrap().domain = Some(DOOR.into());
		let rendered = text(&render(&config(), &[stale], &[], true));
		assert!(!rendered.contains("panel:26519"));
		assert_eq!(rendered.matches(r#"{"host":["infra.outside.test"]}"#).count(), 1);
		let rendered = text(&render(&config(), &[panel], &[], true));
		assert!(rendered.contains(r#"{"host":["geo-ui.outside.test"]}"#));
	}

	#[test]
	fn the_render_is_the_same_for_the_same_state() {
		// Stable output is what makes a diff of two renders mean something changed.
		assert_eq!(
			text(&render(&config(), &[geo()], &[], true)),
			text(&render(&config(), &[geo()], &[], true))
		);
	}

	/// Every side there is: scopes on each side, and a route on both suffixes.
	fn full(lan: bool) -> Value {
		let nas = Route {
			name: "nas".into(),
			upstream: "10.0.0.21:80".into(),
			private: true,
			public: true,
			home: Some("/admin".into()),
		};
		render(&config(), &[geo()], &[nas], lan)
	}

	#[test]
	fn a_node_with_a_lan_renders_its_private_side_and_certificate_byte_for_byte() {
		let rendered = full(true);
		let private = concat!(
			r#"{"client_ip_headers":["Cf-Connecting-Ip"],"listen":[":443"],"#,
			r#""routes":[{"handle":[{"handler":"subroute","routes":[{"handle":[{"abort":true,"#,
			r#""handler":"static_response"}],"#,
			r#""match":[{"not":[{"remote_ip":{"ranges":["10.0.0.0/24"]}}]}]},"#,
			r#"{"handle":[{"handler":"subroute","routes":[{"handle":[{"handler":"static_response","#,
			r#""status_code":400}],"match":[{"path":["/"]}]},{"handle":[{"handler":"rewrite","#,
			r#""strip_path_prefix":"/geo"},{"encodings":{"gzip":{},"zstd":{}},"handler":"encode","#,
			r#""prefer":["zstd","gzip"]},{"handler":"reverse_proxy","#,
			r#""upstreams":[{"dial":"geo:23440"}]}],"match":[{"path":["/geo","/geo/*"]}]},"#,
			r#"{"handle":[{"handler":"static_response","status_code":404}]}]}],"#,
			r#""match":[{"host":["api.inside.test"]}]},"#,
			// host's door, ahead of every label an app or a route answers on.
			r#"{"handle":[{"handler":"subroute","routes":[{"handle":[{"encodings":{"gzip":{},"#,
			r#""zstd":{}},"handler":"encode","prefer":["zstd","gzip"]},{"handler":"reverse_proxy","#,
			r#""upstreams":[{"dial":"host.app-host:11011"}]}],"#,
			r#""match":[{"method":["POST"],"path":["/notice"]}]},"#,
			r#"{"handle":[{"encodings":{"gzip":{},"zstd":{}},"handler":"encode","#,
			r#""prefer":["zstd","gzip"]},{"handler":"reverse_proxy","#,
			r#""upstreams":[{"dial":"host.app-host:11011"}]}],"match":[{"method":["GET"],"#,
			r#""path_regexp":{"pattern":"^/api/(node/(now|series)|apps|apps/[^/]+|"#,
			r#"apps/[^/]+/(history|health|metrics/series)|events|inspect/disk)$"}}]},"#,
			r#"{"handle":[{"handler":"static_response","status_code":404}]}]}],"#,
			r#""match":[{"host":["infra.inside.test"]}]},"#,
			r#"{"handle":[{"handler":"subroute","#,
			r#""routes":[{"handle":[{"handler":"static_response","headers":{"Location":["/admin"]},"#,
			r#""status_code":307}],"match":[{"path":["/"]}]},{"handle":[{"encodings":{"gzip":{},"#,
			r#""zstd":{}},"handler":"encode","prefer":["zstd","gzip"]},{"handler":"reverse_proxy","#,
			r#""upstreams":[{"dial":"10.0.0.21:80"}]}]}]}],"match":[{"host":["nas.inside.test"]}]},"#,
			r#"{"handle":[{"abort":true,"handler":"static_response"}]}]}],"#,
			r#""match":[{"host":["*.inside.test"]}],"terminal":true}],"#,
			r#""trusted_proxies":{"ranges":["172.30.0.20"],"source":"static"}}"#,
		);
		assert_eq!(text(&rendered["apps"]["http"]["servers"]["private"]), private);
		let tls = concat!(
			r#"{"automation":{"policies":[{"issuers":[{"challenges":{"dns":"#,
			r#"{"provider":{"api_token":"{env.CLOUDFLARE_API_TOKEN}","#,
			r#""name":"cloudflare"},"resolvers":["1.1.1.1"]}},"email":"someone@example.com","#,
			r#""module":"acme"}],"subjects":["*.inside.test"]}]}}"#,
		);
		assert_eq!(text(&rendered["apps"]["tls"]), tls);
	}

	#[test]
	fn a_node_with_no_lan_asks_for_no_certificate() {
		let without = full(false);
		let servers = &without["apps"]["http"]["servers"];
		assert!(servers.get("private").is_none() && without["apps"].get("tls").is_none());
		let rendered = text(&without);
		assert!(!rendered.contains(r#"[":443"]"#) && !rendered.contains("CLOUDFLARE_API_TOKEN"));
		assert_eq!(servers["tunnel"]["listen"][0], ":80");
		assert!(servers.get("inside").is_none());
		// Everything else is what a node with a LAN renders.
		let mut with = full(true);
		with["apps"]["http"]["servers"].as_object_mut().unwrap().remove("private");
		with["apps"].as_object_mut().unwrap().remove("tls");
		assert_eq!(rendered, text(&with));
	}
}
