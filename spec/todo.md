# Todo

Work infra has agreed to and not finished. Open questions are [issues.md](issues.md) and the direction
is [roadmap.md](roadmap.md); how the three divide the work is the workspace's `spec/planning.md`.

- **The bot that turns a console change into a pull request.** Until it exists a label is changed in
  `service.toml` -- [architecture/host.md](architecture/host.md), "A label is declared in the
  repository, and the console will write it there".
- **A filter fed to the resolver's chain.** The step is rendered and tested, and host passes it an
  empty list -- [architecture/host.md](architecture/host.md).
- **The console draws what host's inspect routes answer**: a file manager and a view of containers --
  [architecture/inspect.md](architecture/inspect.md).
- **The egress proxies, and `EGRESS_PROXIES` in `libs/deploy`'s GitHub client**, before `gvx` and
  `bru` lose IPv4 -- [architecture/nodes.md](architecture/nodes.md), "The nodes".
- **`app_of` stops reading a bare `deploy-<app>` as arm64** once no run that still names its
  artifacts that way is fresh enough to deploy -- [architecture/host.md](architecture/host.md),
  "The machine pulls; nothing pushes into it".
- **A health-check stage of its own**, which needs `libs/deploy`'s `replace_beside` to report its
  progress: today `starting` covers the start and the check together --
  [architecture/host.md](architecture/host.md), "Every event is kept, and none is pruned".
- **x86 nodes run arm64 images by emulation**: `emulate = ["arm64"]` in `nodes.toml`, which
  `mise run node` installs QEMU's user-mode emulation for, and `arch = "arm64"` in an app's
  `service.toml`, which host fetches that architecture's artifact for and refuses to place where it
  cannot run -- [architecture/nodes.md](architecture/nodes.md), "An x86 node may run arm64 images,
  emulated, and never the other way", and [architecture/host.md](architecture/host.md), "An app may
  ask for one architecture". `buf` first, for the platform's Postgres.
- **A new host, keeper or Caddy reaches one node first.** It goes to the canary, nrt, marked
  `canary = true` in `nodes.toml` and the only node so marked, before the rest take it -- the door
  that broke on all seven nodes at once on 2026-10-07 would have stopped at one. The rest learn by
  asking: a run that built one of the three is held on every other node, whose host and keeper
  poll the canary's verdict over the tailnet, through the canary's Caddy with the read token, and
  deploy once it passes or skip, saying why, when it fails or a deadline passes. The verdict passes
  when the canary has deployed the run and reports healthy; the others' reaching it through the
  canary's Caddy proves that Caddy routes to host. `mise run node deploy` by hand overrides the
  hold. Networks between a node's own apps are already put right on every start: keeper joins
  host's network as it starts, and host attaches itself and Caddy to every network as it starts
  and after each new Caddy.
