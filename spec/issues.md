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

## A changed `.env` reaches host only when host is replaced

`mise run node` rewrites host's `.env` -- a slot, a grant -- and says host needs recreating, but
only keeper replaces host, and only for a run that built it. On 2026-10-07 every node's host was
made to read its new `.env` by sending each the last infra run again by hand, which keeper took as a
replacement of host by the same image. Whether the node task asks keeper for that itself, keeper
gains a route that replaces host by the image it already runs, or host reads its grants from a file
it watches, is undecided.

## An app reaches an app on another node only by a tailnet address

Found on 2026-10-08, when cue's `qq` on `sha` had to reach its `hub` on `tyo` over a long-lived
WebSocket: nothing names an app across nodes. The only way that works is the `peer` role -- the
hub's port published on `tyo`'s tailnet address, admitted from the tailnet alone -- with that address
written into `qq`'s `config.env`, so the hub moving to another node is an edit by hand. `peer` was
meant for an app talking to itself on other nodes, and is borrowed here. The private suffix does not
help: `internal.ixc.one` is answered by the house's resolver with the house's node, for the LAN.
What deciding it involves: a name each app is reached by from any node -- a proxy on every node by
app name, as `primary` is for the database, or the private suffix resolved on every node to the
tailnet address of the node an app runs on -- fed by the same knowledge of which node runs what that
the public router needs (platform's `spec/todo/todo.md`, "Every node's interface is public through
one Worker").
