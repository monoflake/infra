# Issues

Open questions infra has. What is decided and waiting is [todo.md](todo.md), and the direction is
[roadmap.md](roadmap.md); how the three divide the work is the workspace's `spec/planning.md`, and the
questions every project shares are the workspace's `spec/issues.md`.

What is undecided about placing services on a node is the platform's -- platform's
`spec/issues/issues.md`.

## How a node without IPv4 reaches what only IPv4 serves

`gvx` and `bru` lose IPv4 egress -- [architecture/nodes.md](architecture/nodes.md), "The nodes" --
and host takes every build from GitHub's API, which has no IPv6. Either an app on a dual-stack
node proxies HTTPS for them over the tailnet, set as `HTTPS_PROXY` for host and dockerd alone, or a
tailnet exit node carries all of their traffic. The proxy is narrower and is an app like any
other; the exit node needs no code. Undecided.
