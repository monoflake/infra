//! The house's DNS, rendered from host's configuration: CoreDNS answers no name of its own and
//! passes every one down a chain that skips what fails. Written whole, as Caddy's configuration
//! is, and read again by CoreDNS on its own. See spec/architecture/host.md, "The resolver serves
//! the house, and answers nothing of its own".

use crate::caddy;
use crate::config::ResolverConfig;

/// Where CoreDNS answers inside its container; the node publishes it as 53.
const LISTEN: u16 = 1053;

/// Where its health is asked, as `[container]` in its declaration says.
const HEALTH: u16 = 15353;

/// The Corefile, or nothing on a node that runs no resolver. Every name goes to `filters` and then
/// the configured upstreams, in order, one that fails passed over.
pub fn render(config: &ResolverConfig, filters: &[String]) -> Option<String> {
	config.address.as_deref()?;
	let mut out =
		String::from("# Rendered by host, never edited by hand; see spec/architecture/host.md.\n");
	out
		.push_str(&format!(".:{LISTEN} {{\n\terrors\n\thealth :{HEALTH}\n\treload 10s\n\tcache 300\n"));
	let upstreams: Vec<&str> =
		filters.iter().chain(config.upstreams.iter()).map(String::as_str).collect();
	out.push_str(&format!(
		"\tforward . {} {{\n\t\tpolicy sequential\n\t\thealth_check 5s\n\t}}\n}}\n",
		upstreams.join(" ")
	));
	Some(out)
}

/// Render and write it, when the node runs a resolver; CoreDNS notices the file change itself.
pub async fn apply(config: &ResolverConfig, filters: &[String]) -> Result<(), caddy::Error> {
	match render(config, filters) {
		Some(text) => caddy::write(&config.file, text.as_bytes()).await,
		None => Ok(()),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn config() -> ResolverConfig {
		ResolverConfig {
			file: "/nowhere/Corefile".into(),
			address: Some("10.0.0.11".into()),
			upstreams: vec!["10.0.0.1".into(), "1.1.1.1".into()],
		}
	}

	#[test]
	fn answers_no_name_with_the_node_the_gateways_included() {
		let text = render(&config(), &[]).unwrap();
		assert!(text.contains(".:1053 {"));
		assert!(!text.contains("template"));
		assert!(!text.contains("10.0.0.11"));
		for name in ["monoflake", "ixc", "ill.li", "symlink"] {
			assert!(!text.contains(name), "{name}");
		}
	}

	#[test]
	fn passes_every_name_down_the_chain_a_filter_first() {
		let text = render(&config(), &["10.0.0.53".into()]).unwrap();
		assert!(text.contains("forward . 10.0.0.53 10.0.0.1 1.1.1.1 {\n\t\tpolicy sequential"));
	}

	#[test]
	fn renders_nothing_on_a_node_with_no_address_for_it() {
		let none = ResolverConfig { address: None, ..config() };
		assert_eq!(render(&none, &[]), None);
	}
}
