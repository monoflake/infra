/**
 * Infra's own addresses: host's door, keeper and host, which bootstrap everything else and so are
 * named below it. See the workspace's spec/architecture/layers.md, "Addresses are split by who owns
 * the
 * name".
 */
export const INFRA = {
	// host's door, the name the panel answered on before it retired: CI's notice and the console's
	// reads, by Caddy's allowlist. Still keyed `panel`, as the platform's sdk reads it. See
	// apps/deploy/host/src/caddy.rs, `door`.
	panel: 'https://infra.internal.ixc.one',
	keeper: 'https://keeper.internal.ixc.one',
	// host's API on its own network, as Caddy and a peer reach it.
	host: 'http://host:11011',
} as const;

/** The port the panel's development server answered on, kept while the platform's sdk names it. */
export const PANEL_PORT = 26519;

/**
 * Where `mise run reach` answers: a plain-HTTP door on this machine to an interface that only the
 * LAN reaches. Not an app, so it is kept out of the pinned ports. See spec/repository.md, "Reaching
 * the LAN from a browser that cannot".
 */
export const REACH_PORT = 26520;
