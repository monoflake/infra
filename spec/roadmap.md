# Roadmap

Where infra is going, without the steps. What is decided and waiting is [todo.md](todo.md), and what
is open is [issues.md](issues.md); how the three divide the work is the workspace's
`spec/planning.md`.

- **The panel is where the node is run from, and git stays the record of what it changes** --
  [architecture/host.md](architecture/host.md), "A label is declared in the repository, and the
  panel will write it there".
- **The panel gives way to the console's node build**, a services app web builds that shows the
  node it runs on and reads only its host -- web's `spec/roadmap.md`, "One console runs the system,
  and it is this layer's". The panel stays until that build does what the panel does.
- **Notifications** -- [architecture/host.md](architecture/host.md).
- **Every node in [architecture/nodes.md](architecture/nodes.md) runs host, and the one at home may
  go offline without taking the platform with it.**
