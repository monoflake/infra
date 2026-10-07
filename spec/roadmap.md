# Roadmap

Where infra is going, without the steps. What is decided and waiting is [todo.md](todo.md), and what
is open is [issues.md](issues.md); how the three divide the work is the workspace's
`spec/planning.md`.

- **A node is run from the console, and git stays the record of what it changes** --
  [architecture/host.md](architecture/host.md), "A label is declared in the repository, and the
  console will write it there"; a node itself keeps only host's API, reached without the edge over
  the tailnet -- "host has no interface on the node, and a door Caddy keeps".
- **Notifications** -- [architecture/host.md](architecture/host.md).
- **Every node in [architecture/nodes.md](architecture/nodes.md) runs host, and the one at home may
  go offline without taking the platform with it.**
