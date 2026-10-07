# Roadmap

Where infra is going, without the steps. What is decided and waiting is [todo.md](todo.md), and what
is open is [issues.md](issues.md); how the three divide the work is the workspace's
`spec/planning.md`.

- **The panel is where the node is run from, and git stays the record of what it changes** --
  [architecture/host.md](architecture/host.md), "A label is declared in the repository, and the
  panel will write it there".
- **The panel retires, and a node keeps only host's API.** The console at the edge is the one
  view -- web's `spec/roadmap.md`, "One console runs the system, and it is this layer's" -- and a
  node is reached without it over the tailnet, by SSH and a task that speaks host's API. CI's notice
  goes to host directly before the panel goes.
- **Notifications** -- [architecture/host.md](architecture/host.md).
- **Every node in [architecture/nodes.md](architecture/nodes.md) runs host, and the one at home may
  go offline without taking the platform with it.**
