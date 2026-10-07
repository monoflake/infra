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
longer rare. The direction is the relay's ordered log, which a node that was away catches up on by
comparing its place with any neighbor's -- platform's `spec/architecture/relay.md`. Whether host
also asks GitHub on start, until the relay exists, is undecided.

## An app's name is not tied to the repository that first deployed it

host refuses only its reserved names, so once a second repository is a source -- web's, for the
console's node build -- a run of it could ship `deploy-gateway-arm64` and replace the platform's
gateway on a node. The platform's deployer ties each Worker's name to one `owner/repo`
(platform's `spec/architecture/deployer.md`, "What it refuses"); host doing the same -- an app
keeps the repository it was first deployed from, or an `APP_SOURCES` list in the node's `.env` --
is decided in direction and not in shape: which of the two, and where the first deploy is recorded.
It must land before web is admitted as a source. Also: host fetches with the monoflake-owned
`GITHUB_ACTIONS_TOKEN`, and a canmi21 repository needs `GITHUB_ACTIONS_TOKEN_CANMI21`, picked by the
source's owner.

## An old run's notice can roll an app back

host takes each `(repository, run)` once per process, and again if taking it failed, but never asks
whether a run is newer than the one that deployed the app. A notice naming an old successful run --
after host restarts, or for a run it never took -- deploys that run's image over a newer one. The
platform's deployer refuses a run that is not newer than the last that deployed its Worker
(platform's `spec/architecture/deployer.md`); host keeping the same mark per app, and refusing a
run not newer than it, is the fix in direction. Where the mark is kept, and how an operator's
explicit rollback stays allowed past it, are undecided.

## The nodes' tunnels are configured differently

`rdu`'s tunnel routes `*.canmi.app` to Caddy with WARP routing on; `tyo`'s, written later by
`mise run tunnel`, is `ingress: [http_status:404]` with WARP routing off -- seen 2026-10-07. Workers
VPC reaches both anyway, so nothing is broken, but two nodes made by one task differ. Whether the
task should write rdu's shape everywhere, or rdu's is a leftover from before VPC to be trimmed, is
undecided.
