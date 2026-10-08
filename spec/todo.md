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
- **host catches up on runs it missed**, decided on 2026-10-08 after rdu lost platform's run
  37710256103 while keeper was recreating its host. At start and every 15 minutes, host lists each
  source's successful `deploy.yml` runs on `main` for the 7 days their artifacts are kept; a run
  with no event in its history, and newer than the oldest run it holds for that repository, was
  missed. Missed runs are taken oldest first through the same path a notice takes, each deploying
  only the apps no newer run built, so an older build never replaces a newer one; a missed run that
  built host is handed to keeper on host's network. A node away longer than 7 days still needs a
  run by hand.
- **A health-check stage of its own**, which needs `libs/deploy`'s `replace_beside` to report its
  progress: today `starting` covers the start and the check together --
  [architecture/host.md](architecture/host.md), "Every event is kept, and none is pruned".
- **x86 nodes run arm64 images by emulation**: `emulate = ["arm64"]` in `nodes.toml`, which
  `mise run node` installs QEMU's user-mode emulation for, and `arch = "arm64"` in an app's
  `service.toml`, which host fetches that architecture's artifact for and refuses to place where it
  cannot run -- [architecture/nodes.md](architecture/nodes.md), "An x86 node may run arm64 images,
  emulated, and never the other way", and [architecture/host.md](architecture/host.md), "An app may
  ask for one architecture". `buf` first, for the platform's Postgres.
