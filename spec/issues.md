# Issues

Open questions infra has. What is decided and waiting is [todo.md](todo.md), and the direction is
[roadmap.md](roadmap.md); how the three divide the work is the workspace's `spec/planning.md`, and the
questions every project shares are the workspace's `spec/issues.md`.

What is undecided about placing services on a node is the platform's -- platform's
`spec/issues/issues.md`.

## A node that was offline misses the runs that ended meanwhile

A finished run reaches a node only as a notice, which the platform's `hook` sends once, when GitHub
calls it -- [architecture/host.md](architecture/host.md), "The machine pulls; nothing pushes into
it". GitHub does not deliver a webhook again on its own, so a node that was down then never learns
of the run, and keeps the version before it until the next push to the same app. With a node that
is allowed to go offline -- [architecture/nodes.md](architecture/nodes.md), `home` -- that is no
longer rare. host could ask GitHub, when it starts, for each source's latest successful run and take
what it has not; how far back it looks, and whether keeper does the same for host, is undecided.
