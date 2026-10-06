//! How the GitHub client leaves the node: straight out, or through the egress proxies a node
//! without IPv4 is given, tried in order. See spec/architecture/nodes.md, "The nodes".

use hyper::Uri;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::connect::proxy::Tunnel;
use hyper_util::rt::TokioIo;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::net::TcpStream;
use tower_service::Service;

/// How long one proxy has to open its tunnel before the next is asked.
const ATTEMPT: Duration = Duration::from_secs(10);

/// hyper-util names this type but does not export it.
type TunnelError = <Tunnel<HttpConnector> as Service<Uri>>::Error;

#[derive(Debug, thiserror::Error)]
#[error("EGRESS_PROXIES entry `{0}` is not http://<host>:<port>")]
pub struct Malformed(String);

/// The proxies `EGRESS_PROXIES` names, in the order they are tried: `http://<host>:<port>` URLs
/// separated by whitespace. None means connecting directly.
pub fn proxies(value: &str) -> Result<Vec<Uri>, Malformed> {
	value
		.split_whitespace()
		.map(|entry| proxy(entry).ok_or_else(|| Malformed(entry.to_owned())))
		.collect()
}

fn proxy(entry: &str) -> Option<Uri> {
	let uri: Uri = entry.parse().ok()?;
	let authority = uri.authority()?;
	let plain = uri.scheme_str() == Some("http") && !authority.as_str().contains('@');
	let bare = matches!(uri.path(), "" | "/") && uri.query().is_none();
	(plain && bare && authority.port().is_some()).then_some(uri)
}

/// Why one proxy opened no tunnel.
#[derive(Debug, thiserror::Error)]
enum Refusal {
	#[error("{}", chain(.0))]
	Tunnel(TunnelError),
	#[error("no tunnel within {} s", ATTEMPT.as_secs())]
	Silent,
}

/// Every proxy was asked and none opened a tunnel.
#[derive(Debug, thiserror::Error)]
#[error("every egress proxy failed: {}", list(.0))]
pub struct Unreachable(Vec<(Uri, Refusal)>);

fn list(failures: &[(Uri, Refusal)]) -> String {
	let each = failures.iter().map(|(proxy, why)| format!("{proxy} ({why})"));
	each.collect::<Vec<_>>().join(", ")
}

/// `error` and every cause beneath it, as one line.
pub(crate) fn chain(error: &dyn std::error::Error) -> String {
	let mut line = error.to_string();
	let mut cause = error.source();
	while let Some(error) = cause {
		line.push_str(&format!(": {error}"));
		cause = error.source();
	}
	line
}

/// The connector under TLS: TLS to the target runs inside whatever this opens, so a proxy only
/// ever carries ciphertext.
#[derive(Clone)]
pub enum Egress {
	Direct(HttpConnector),
	Through(Arc<[(Uri, Tunnel<HttpConnector>)]>),
}

impl Egress {
	pub fn new(proxies: Vec<Uri>) -> Self {
		if proxies.is_empty() {
			return Self::Direct(http());
		}
		let tunnels = proxies.into_iter().map(|proxy| (proxy.clone(), Tunnel::new(proxy, http())));
		Self::Through(tunnels.collect())
	}
}

fn http() -> HttpConnector {
	let mut http = HttpConnector::new();
	http.enforce_http(false);
	http
}

async fn through(
	proxies: &[(Uri, Tunnel<HttpConnector>)],
	target: Uri,
) -> Result<TokioIo<TcpStream>, Unreachable> {
	let mut failures = Vec::new();
	for (proxy, tunnel) in proxies {
		let mut tunnel = tunnel.clone();
		let attempt = async {
			std::future::poll_fn(|cx| tunnel.poll_ready(cx)).await?;
			tunnel.call(target.clone()).await
		};
		let why = match tokio::time::timeout(ATTEMPT, attempt).await {
			Ok(Ok(stream)) => return Ok(stream),
			Ok(Err(error)) => Refusal::Tunnel(error),
			Err(_) => Refusal::Silent,
		};
		failures.push((proxy.clone(), why));
	}
	Err(Unreachable(failures))
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

impl Service<Uri> for Egress {
	type Response = TokioIo<TcpStream>;
	type Error = BoxError;
	type Future = Pin<Box<dyn Future<Output = Result<Self::Response, BoxError>> + Send>>;

	fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), BoxError>> {
		Poll::Ready(Ok(()))
	}

	fn call(&mut self, target: Uri) -> Self::Future {
		match self {
			Self::Direct(http) => {
				let connecting = http.call(target);
				Box::pin(async move { Ok(connecting.await?) })
			}
			Self::Through(proxies) => {
				let proxies = proxies.clone();
				Box::pin(async move { Ok(through(&proxies, target).await?) })
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use tokio::io::{AsyncReadExt, AsyncWriteExt};
	use tokio::net::TcpListener;
	use tokio::task::JoinHandle;

	#[test]
	fn no_proxies_means_connecting_directly() {
		assert!(proxies("").unwrap().is_empty());
		assert!(proxies(" \t\n").unwrap().is_empty());
		assert!(matches!(Egress::new(Vec::new()), Egress::Direct(_)));
	}

	#[test]
	fn the_proxies_are_kept_in_the_order_the_node_lists_them() {
		assert_eq!(proxies("http://a.test:8888").unwrap(), ["http://a.test:8888"]);
		let several = proxies(" http://b.test:8888\thttp://c.test:8888/ \n").unwrap();
		assert_eq!(several, ["http://b.test:8888/", "http://c.test:8888/"]);
	}

	#[test]
	fn a_malformed_entry_refuses_the_whole_list() {
		for entry in [
			"a.test:8888",
			"https://a.test:8888",
			"http://a.test",
			"http://user:secret@a.test:8888",
			"http://a.test:8888/path",
			"http://a.test:8888/?query",
			"socks5://a.test:1080",
		] {
			let list = format!("http://b.test:8888 {entry}");
			let error = proxies(&list).unwrap_err().to_string();
			assert_eq!(error, Malformed(entry.to_owned()).to_string());
		}
	}

	/// A proxy on a loopback port that answers one CONNECT with `answer` and hands back the
	/// request line it was sent.
	async fn proxy(answer: &'static [u8]) -> (Uri, JoinHandle<String>) {
		let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
		let uri = format!("http://{}", listener.local_addr().unwrap()).parse().unwrap();
		let answering = tokio::spawn(async move {
			let (mut stream, _) = listener.accept().await.unwrap();
			let mut head = Vec::new();
			while !head.ends_with(b"\r\n\r\n") {
				let mut byte = [0];
				stream.read_exact(&mut byte).await.unwrap();
				head.push(byte[0]);
			}
			stream.write_all(answer).await.unwrap();
			String::from_utf8(head).unwrap().lines().next().unwrap().to_owned()
		});
		(uri, answering)
	}

	/// A loopback port nothing listens on.
	async fn closed() -> Uri {
		let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
		format!("http://{}", listener.local_addr().unwrap()).parse().unwrap()
	}

	const TARGET: &str = canmi::EXTERNAL_GITHUB_API;

	#[tokio::test]
	async fn a_proxy_that_refuses_or_is_down_passes_to_the_next() {
		let down = closed().await;
		let (refusing, refused) = proxy(b"HTTP/1.1 403 Forbidden\r\n\r\n").await;
		let (accepting, accepted) = proxy(b"HTTP/1.1 200 Connection established\r\n\r\n").await;
		let mut egress = Egress::new(vec![down, refusing, accepting]);
		assert!(egress.call(TARGET.parse().unwrap()).await.is_ok());
		assert_eq!(refused.await.unwrap(), "CONNECT api.github.com:443 HTTP/1.1");
		assert_eq!(accepted.await.unwrap(), "CONNECT api.github.com:443 HTTP/1.1");
	}

	#[tokio::test]
	async fn when_every_proxy_fails_the_error_names_each() {
		let down = closed().await;
		let (refusing, _) = proxy(b"HTTP/1.1 403 Forbidden\r\n\r\n").await;
		let mut egress = Egress::new(vec![down.clone(), refusing.clone()]);
		let error = egress.call(TARGET.parse().unwrap()).await.unwrap_err().to_string();
		assert!(error.starts_with("every egress proxy failed: "), "{error}");
		assert!(error.contains(&down.to_string()) && error.contains(&refusing.to_string()), "{error}");
	}
}
