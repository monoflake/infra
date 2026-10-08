# Nodes

A node is a machine that runs host. This file lists them and says what a person decides about each;
what the machine can measure about itself is not written here.

## Two families of system, Debian and Alpine

**A node runs Debian or Alpine, and nothing else.** Every step that touches the machine rather than a
container -- the firewall, the mount Docker waits for, the services that start at boot -- is written
once for each, systemd and OpenRC, and a third family would be a third copy of all of it. Armbian is
Debian.

## A node is named for where it is

**A node's name is the three-letter IATA code of a place near it**, an airport or a city: `rdu`,
`tyo`. It names neither the provider nor the architecture, which change when a machine is replaced
while the place usually does not. Two nodes in one city take two codes that city has -- `tyo`,
`nrt` and `hnd` are all Tokyo. The name is the node's hostname, its name on the tailnet and the value
of `NODE` in host's `.env`.

## Three things are declared, and the rest is measured

| Axis               | Answers                                      | Values                               |
| ------------------ | -------------------------------------------- | ------------------------------------ |
| **tier**           | whether it stays up                          | `datacenter`, `home` or `transient`  |
| **failure domain** | which nodes fail together                    | the account's provider, by its code  |
| **expiry**         | until when it is expected to be held, a year | an estimate, recorded without reason |

- **`datacenter`** is a provider's machine, held for years. **`home`** is a machine in the house: it
  goes offline with the house's power and line, and what runs on it is allowed to go with it.
  **`transient`** is a machine held for now and replaceable at any time, nothing on it but
  computation.
- **A failure domain is an account.** What ends an account -- a card, a plan, a provider's decision --
  takes every machine under it at once, so two machines in one account are one domain however far
  apart they are. Two accounts at one provider are two domains. **A domain is written as the
  provider's code** from the registry in platform's `spec/architecture/gateway.md`, "Providers are
  short codes, registered here" -- `oci`, `az`, `rkn`, and `int` for the hardware in the house -- and
  shown to a person by the provider's name; a second account at a provider is the code and a
  number, `oci-2`. **`int` is the one code that is not an account**: a self-hosted machine fails
  with its own house, and the author never keeps two in one place, so every `int` node is a failure
  domain of its own, `rdu` and `sha` two, and whatever counts domains counts each `int` node apart.
  Decided on 2026-10-08.
- **Capacity is measured, never declared**: the architecture, memory and disk are what `meter`
  reports ([meter.md](meter.md)). A machine given more memory changes no record, and a declared size
  would be wrong from that day.

**One tier says nothing about size**, and that is on purpose. Whether a machine stays up and whether
something fits on it change for different reasons -- an upgrade changes the one, a lapsed plan the
other -- so a tier mixing both would be rewritten for either.

## Three nodes are the core, named by the author

**`rdu`, `buf` and `tyo` are the core**, `core = true` in `nodes.toml`, and every other node is a
worker. The core holds the control plane -- the relay's log, platform's
`spec/architecture/relay.md`, the deployer, the platform's database -- and a worker runs apps and
keeps nothing the rest depends on. The workspace's `spec/architecture/ship-cloud.md` is why a
node somebody else brought is never core.

- **Three accounts, so losing any one leaves two that agree.** Two of them are in the eastern US,
  so the two that make a majority are milliseconds apart, and `tyo` is the copy across the ocean.
- **Named, not derived from the axes above**, since the one set that is three accounts with a
  majority in the east has a `home` node in it and an expiry a year away. What that costs is taken
  knowingly: `rdu` goes down with the house, leaving two cores that agree across the Pacific, slower
  but up; `buf` is held until 2027, and what replaces it takes its place in the core.
- **`gvx` and `bru` are workers**: a gigabyte of memory each and no IPv4.

Decided on 2026-10-07, in place of a rule that placed the platform on whichever node was
`datacenter`, large and long-held, which named `tyo` alone.

## An x86 node may run arm64 images, emulated, and never the other way

**A node declares `emulate = ["arm64"]` in `nodes.toml` to run arm64 images it cannot run natively.**
`mise run node` installs QEMU's user-mode emulation and registers it with the kernel's
`binfmt_misc`, fixed at registration so a container needs nothing of its own -- Debian's
`qemu-user-binfmt` registers it so; an Alpine node may not declare it, its registration unproven
and nothing placed on `nrt` or `hnd`, the two x86 Alpine nodes, needing it -- and host is told `EMULATE=arm64` in its `.env` and runs an app that asks
for arm64 there by emulation -- [host.md](host.md), "An app may ask for one architecture".
It is for an app whose files must be the same bytes on every node, which an arm64 program
emulated writes exactly as a native one does, its C library included: the platform's Postgres,
streaming physically between `tyo`, `rdu` and `buf` -- platform's `spec/architecture/databases.md`.

**The emulated core is QEMU's default, every extension it knows, not one pinned to ARMv8.0.**
`QEMU_CPU=cortex-a72` would make an emulated node a second check of the floor every arm64 image is
held to -- [host.md](host.md), "An image is built for speed, and for any node of its architecture"
-- but `rdu` is that floor natively, and the one app emulated, the database, runs there too; and
without LSE atomics the emulated Postgres would be slower still. The floor is checked where it
covers every image before any node runs it, when an image first needs checking: in CI, by running
each arm64 binary under `qemu-aarch64 -cpu cortex-a72`. Decided on 2026-10-07.

**Only x86 emulates arm64.** x86 orders memory more strictly than arm64, so an arm64 program on it
gets every guarantee it was built to expect and more. An x86 program emulated on arm64 would rely on
an ordering the host does not give and the emulator must add around every access -- slow at best,
and at worst wrong in shared memory, which is where Postgres's processes meet. Rejected for that.

**It is slow, and measured so.** On `buf` on 2026-10-07, an emulated standby replayed a 150 MB load
and 200 writes a second as closely as `rdu` natively did, while a query ran twenty to thirty times
slower than on `tyo`: 1,455 read-only transactions a second against `tyo`'s 30,519, a count of a
million rows in 3.9 s against 0.12. So an emulated node is a standby that replays well and a primary
of last resort.

## Nothing comes in but over the tailnet

**A node opens no inbound port to the public.** Public traffic reaches it through its own tunnel,
which it dials out -- platform's `spec/architecture/services.md`, "Every node is the same node" --
so whether a machine has a public IPv4, an IPv6, or neither changes nothing about how it is reached.
The provider's firewall and the node's own both refuse whatever arrives unasked; ssh is reached over
the tailnet, and a provider's console is the way back in when the tailnet is not. The one exception both
let in is Tailscale's UDP port, 41641: without it every path to the node is relayed through
Tailscale's servers rather than direct.

**The node's own firewall is a table of its own, `inet node`, beside Docker's and Tailscale's and
never touching them** -- [`nodes/firewall.nft`](../../nodes/firewall.nft):

- **In**: what answers a connection the node made, the loopback, the tailnet and Docker's bridges,
  ICMP of both versions -- IPv6 dies without neighbor discovery -- Tailscale's own UDP port for direct
  paths, and the DHCP replies an address is leased by. Everything else is dropped.
- **Through**: a new connection that Docker translated to a container is dropped unless it came in
  over a trusted interface. Docker publishes a port by rewriting it before the input chain ever sees
  it, so a firewall on input alone would leave every published port open; this rule closes them to
  the public whatever a container publishes.
- **Trusted** is the loopback, the tailnet, Docker's bridges and, for the node in the house, its LAN
  -- `lan` in `nodes.toml`, since the house's devices reach it there.

Rejected: **Docker's `iptables: false`**, which leaves containers without the address translation
they reach the network through, and **Debian's `nftables.service`**, whose stop flushes the whole
ruleset, Docker's and Tailscale's with it; the node loads its table with a unit of its own.

## A node is set up by one task, run again at will

**`mise run node <name>` brings a machine to what [`nodes/`](../../nodes/) holds for it**, and run
again changes nothing: its name as hostname and on the tailnet, UTC, the files that make Docker wait
for `/data` -- [host.md](host.md), "The control plane going down is not an outage" -- the
firewall, and sshd allowing local port forwarding, which is how `mise run node <verb>` reaches host
and which Alpine refuses as shipped -- each written once for Debian and once for Alpine. cloud-init is told to keep the hostname,
or it would put the provider's back at every boot. The first run names the machine by `--address`,
since it is not yet called by its name. A name set by hand in Tailscale's console outranks the one the machine
asks for, so a node renamed there keeps its old tailnet name until the console says otherwise.

**A node may name grants of its own**, `grants = ["app:role", …]` in `nodes.toml`, which `mise run
node` adds to host's `GRANTS` after its family's and, on a core node, the core's, each pair once;
each is checked as `app:role` with a role host knows before any machine is asked. It is how a node
outside the core runs what the core is granted, as `sha` runs a database standby.

**The firewall goes up under a guard.** Unless a second ssh session, opened after it, proves the
machine is still reachable, the machine takes the table down by itself a minute later, so a rule that
locks the session out undoes itself.

## A cloud node is brought up by two more tasks

**`mise run tunnel <name>` wires the node into Cloudflare, and `mise run node host <name>` starts its
first host.** The first gives the node a tunnel and a Workers VPC service of its own name, the VPC
service reaching Caddy through that tunnel as `rdu`'s does, and puts the tunnel's token on the node.
The second loads host's image from the newest CI run, starts it from its compose file with the
node's `.env`, and hands it that run, so host deploys the rest -- posted with the machine's own curl,
or wget, to host's address on its network. **Setting a node up pulls no image**: Docker Hub is not
reachable from every node, `sha` in Shanghai first, and every image a node runs arrives as a run's
artifact. It hands the run as one keeper has
already passed on: a run that rebuilt host makes host wait for keeper, and a fresh node has none
until host deploys it.

**A new node's VPC service is bound wherever the nodes are listed.** The platform's `hook` and
`gateway` and web's console each bind every node's, and the platform's deployer admits a binding
from outside its organization only once `WORKER_RESOURCES` on the deployer's node gives it -- so
`sha`'s console binding was refused on 2026-10-08, and the console counted eight nodes heard of
seven, until its id was added there for `canmi21/web`.

**`edge` is dual-stack**, `172.30.0.0/24` and `fd34:1053:16bd::/64`, the same unrouted ULA on every
node, so a tunnel may reach Cloudflare over IPv6 as well; Docker translates its outbound traffic as
it does IPv4's. Nothing published becomes reachable from the public IPv6 side: the node's forward
chain drops a translated connection that did not come in on a trusted interface, for both families,
proven before it was adopted. `mise run node` makes it so, and moves a node's IPv4-only `edge` over
by saving its members, recreating it and reconnecting them, the tunnel at its fixed address,
then restarting the tunnel -- a few seconds of the public door -- and picks up a run cut short. The
tunnel still reaches Caddy over IPv4, so Caddy's trust in it is unchanged. Creating the first IPv6
network turns IPv6 forwarding on; no node lost its IPv6 route to that, since none relies on the
kernel accepting router advertisements with `accept_ra = 1`.

**A node may set which family its tunnel uses**, `tunnel_ip_version`, `auto`, `4` or `6`, written
as `TUNNEL_EDGE_IP_VERSION` beside the protocol. `auto`, the default, resolves to IPv4 on `edge`,
since an address from a ULA sorts after IPv4, and falls back from IPv6 to IPv4 only; `6` is IPv6
alone.

**A node may set its tunnel's protocol**, `tunnel_protocol` in `nodes.toml`, one of `quic`,
`http2` or `auto`: `mise run node` writes it as `TUNNEL_TRANSPORT_PROTOCOL` into the tunnel's
`config.env`, which overrides the image's QUIC, removes it when unset, and redeploys the tunnel when
it changed. It is for a node whose line drops QUIC, `sha` first.

**The tunnel comes first.** host deploys the tunnel in the run it is handed, and a first deploy
that fails leaves nothing behind -- the app's directory goes with it, a token placed there
included -- so the token has to be on the node before host starts.

**What every node runs is host, keeper, Caddy, the tunnel, the meter and the relay.** CI's notice
reaches host through Caddy's door on every node -- [host.md](host.md), "host has no interface on the
node, and a door Caddy keeps"; the resolver
answers the house's LAN and stays on the node that has one. `mise run node` gives the relay what it
reads -- `RELAY_SECRET`, `HOST_READ_TOKEN` and `RELAY_PEERS` in its `secret.env`, the read token and
`relay:peer` in host's `.env` -- both shared secrets made once, in the repository's secrets.

**Each node's host has a token of its own**, `HOST_TOKEN_<NAME>` in the repository's secrets, made
the first time the node is brought up: the token is root on its machine, so one leaked stays one
machine. The token GitHub's Actions are read with is shared, since it can only read what CI built.

## The nodes

What each node is declared to be is [`nodes/nodes.toml`](../../nodes/nodes.toml), and nowhere else.

**What the machines were seen to be on 2026-10-07**, `sha` on 2026-10-08, a snapshot to choose placements by and nothing
more -- what the meter reports is the record, and a machine that changes leaves this stale:

| Node  | Account     | CPU                                    | vCPUs | Instruction set                                      | Memory  |
| ----- | ----------- | -------------------------------------- | ----- | ---------------------------------------------------- | ------- |
| `tyo` | Oracle      | Arm Neoverse N1                        | 4     | ARMv8.2: LSE atomics, dot product, CRC32, AES, SHA-2 | 23 GiB  |
| `nrt` | Oracle      | AMD EPYC 7551, Zen                     | 2     | x86-64-v3, AES-NI, SHA-NI                            | 966 MiB |
| `hnd` | Oracle      | AMD EPYC 7551, Zen                     | 2     | x86-64-v3, AES-NI, SHA-NI                            | 966 MiB |
| `gvx` | Azure       | Arm Neoverse N1                        | 2     | ARMv8.2, as `tyo`                                    | 970 MiB |
| `bru` | Azure       | AMD EPYC 7763, Zen 3                   | 2     | x86-64-v3, AES-NI, SHA-NI                            | 898 MiB |
| `buf` | RackNerd    | Intel Xeon E5-2690 v4, Broadwell       | 2     | x86-64-v3, AES-NI, no SHA-NI; emulates arm64         | 3.3 GiB |
| `rdu` | Self-hosted | Arm Cortex-A72 and A53, big and little | 8     | ARMv8.0: CRC32, AES, SHA-2, no LSE atomics           | 7.7 GiB |
| `sha` | Self-hosted | AMD Ryzen 5 5600G, Zen 3               | 4     | x86-64-v3, AES-NI, SHA-NI                            | 15 GiB  |

`nrt` and `hnd` show two vCPUs that are one core's two threads, as `bru`'s are; `tyo`, `gvx` and
`buf` give a core each. On one core, measured the same day, `tyo` ran a loop in 1.87 s, `buf` in
2.07 s and `rdu` in 6.39 s, scheduled onto an A53.

**`gvx` and `bru` have no public IPv4**, inbound or, from late 2026, outbound -- `ipv4 = false` in
`nodes.toml`. On such a node only one thing needs IPv4: host and keeper asking GitHub for a run and
downloading what it built. Images arrive as archives, never pulled from a registry, and the tunnel,
the tailnet and the package mirrors all speak IPv6.

**Every node with IPv4 runs an egress proxy, and a node without it asks them in turn.** tinyproxy, set
up by `mise run node`, admitting the tailnet alone and passing on HTTPS to GitHub's names and
nothing else. It binds every address, since the tailnet's is not there yet when it starts at boot,
and the firewall drops whatever does not come in over the tailnet. A node with `ipv4 = false` gives host and keeper `EGRESS_PROXIES`,
every such proxy by its tailnet address, and `libs/deploy`'s GitHub client tries them in order. The
tailnet is what makes this work: it carries IPv4 between nodes over an IPv6 path, so a container
with IPv4 alone reaches a proxy at a `100.x` address on a machine with no IPv4 of its own.

Rejected: **NAT64 with DNS64**, which needs IPv6 inside every container, and Docker's bridges have
none; and **a tailnet exit node**, which sends all of a node's traffic through one other machine.
A proxy is a stopgap rather than the end: once nodes fetch what CI built from each other --
platform's `spec/architecture/relay.md` -- a node without IPv4 needs no GitHub at all.
