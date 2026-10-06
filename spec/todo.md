# Todo

Work infra has agreed to and not finished. Open questions are [issues.md](issues.md) and the direction
is [roadmap.md](roadmap.md); how the three divide the work is the workspace's `spec/planning.md`.

- **The bot that turns a panel change into a pull request.** Until it exists a label is changed in
  `service.toml` -- [architecture/host.md](architecture/host.md), "A label is declared in the
  repository, and the panel will write it there".
- **A filter fed to the resolver's chain.** The step is rendered and tested, and host passes it an
  empty list -- [architecture/host.md](architecture/host.md).
- **The panel draws what host's inspect routes answer**: a file manager and a view of containers --
  [architecture/inspect.md](architecture/inspect.md).
- **`home` becomes `rdu`**, in every placement and in host's `NODE`, since a node is named for
  where it is -- [architecture/nodes.md](architecture/nodes.md), "A node is named for where it is".
- **The egress proxy for `gvx` and `bru`**, before their IPv4 egress ends --
  [architecture/nodes.md](architecture/nodes.md), "The nodes".
