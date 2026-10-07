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
- **A node reads where an app is placed before downloading it.** The declaration travels inside the
  artifact, so every node downloads every image a run built and only then drops what is placed
  elsewhere: a run of the platform's thirteen apps cost each small node minutes for the one it
  runs. CI uploads the declarations apart, small, and host fetches only what is placed on it --
  [architecture/host.md](architecture/host.md), "The machine pulls; nothing pushes into it".
- **An app chooses how it is rolled out**: `rollout = "replace" | "beside" | "manual"` in
  `service.toml`, `replace` the default -- [architecture/host.md](architecture/host.md), "An app
  chooses how it is rolled out, and keeping nothing earns a gapless one". `manual` first, which the
  platform's Postgres waits on; `beside` with it, for geo and every app that keeps nothing.
- **A new host, keeper or Caddy reaches one node first.** It goes to a canary node, which must
  answer a notice through Caddy's door and keeper's before the rest take it -- the door that broke
  on all seven nodes at once on 2026-10-07 would have stopped at one. And whatever must be true
  between the node's own apps -- which networks Caddy and keeper share with host -- is put right
  on every start, never left to the order things were deployed in.
