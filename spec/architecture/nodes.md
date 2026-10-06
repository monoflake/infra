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
| **failure domain** | which nodes fail together                    | the account it is held under         |
| **expiry**         | until when it is expected to be held, a year | an estimate, recorded without reason |

- **`datacenter`** is a provider's machine, held for years. **`home`** is a machine in the house: it
  goes offline with the house's power and line, and what runs on it is allowed to go with it.
  **`transient`** is a machine held for now and replaceable at any time, nothing on it but
  computation.
- **A failure domain is an account.** What ends an account -- a card, a plan, a provider's decision --
  takes every machine under it at once, so two machines in one account are one domain however far
  apart they are. Two accounts at one provider are two domains.
- **Capacity is measured, never declared**: the architecture, memory and disk are what `meter`
  reports ([meter.md](meter.md)). A machine given more memory changes no record, and a declared size
  would be wrong from that day.

**One tier says nothing about size**, and that is on purpose. Whether a machine stays up and whether
something fits on it change for different reasons -- an upgrade changes the one, a lapsed plan the
other -- so a tier mixing both would be rewritten for either.

## Which node holds the platform is a placement's conclusion

No tier is called core. What the platform keeps -- its database's primary, the scheduler's state --
is placed by a rule over the axes above: `datacenter`, enough memory, an expiry years away; its
replicas by the same rule in another failure domain. Whichever node satisfies the rule holds the
platform, and today that is `tyo`. Small `datacenter` nodes run what keeps nothing; `home` holds what
only the author reads and what needs its disk.

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
for `/data` -- [host.md](host.md), "The control plane going down is not an outage" -- and the
firewall, each written once for Debian and once for Alpine. cloud-init is told to keep the hostname,
or it would put the provider's back at every boot. The first run names the machine by `--address`,
since it is not yet called by its name. A name set by hand in Tailscale's console outranks the one the machine
asks for, so a node renamed there keeps its old tailnet name until the console says otherwise.

**The firewall goes up under a guard.** Unless a second ssh session, opened after it, proves the
machine is still reachable, the machine takes the table down by itself a minute later, so a rule that
locks the session out undoes itself.

## A cloud node is brought up by two more tasks

**`mise run tunnel <name>` wires the node into Cloudflare, and `mise run node host <name>` starts its
first host.** The first gives the node a tunnel and a Workers VPC service of its own name, the VPC
service reaching Caddy through that tunnel as `rdu`'s does, and puts the tunnel's token on the node.
The second loads host's image from the newest CI run, starts it from its compose file with the
node's `.env`, and hands it that run, so host deploys the rest. It hands the run as one keeper has
already passed on: a run that rebuilt host makes host wait for keeper, and a fresh node has none
until host deploys it.

**The tunnel comes first.** host deploys the tunnel in the run it is handed, and a first deploy
that fails leaves nothing behind -- the app's directory goes with it, a token placed there
included -- so the token has to be on the node before host starts.

**What every node runs is host, keeper, the panel, Caddy, the tunnel and the meter.** The panel is
on every node because CI's notice reaches host through it, on the panel's own name; the resolver
answers the house's LAN and stays on the node that has one.

**Each node's host has a token of its own**, `HOST_TOKEN_<NAME>` in the repository's secrets, made
the first time the node is brought up: the token is root on its machine, so one leaked stays one
machine. The token GitHub's Actions are read with is shared, since it can only read what CI built.

## The nodes

What each node is declared to be is [`nodes/nodes.toml`](../../nodes/nodes.toml), and nowhere else.

**`gvx` and `bru` have no public IPv4**, inbound or, from late 2026, outbound -- `ipv4 = false` in
`nodes.toml`. On such a node only one thing needs IPv4: host and keeper asking GitHub for a run and
downloading what it built. Images arrive as archives, never pulled from a registry, and the tunnel,
the tailnet and the package mirrors all speak IPv6.

**Every node with IPv4 runs an egress proxy, and a node without it asks them in turn.** tinyproxy, set
up by `mise run node`, listening on the node's tailnet address alone and passing on HTTPS to
GitHub's names and nothing else. A node with `ipv4 = false` gives host and keeper `EGRESS_PROXIES`,
every such proxy by its tailnet address, and `libs/deploy`'s GitHub client tries them in order. The
tailnet is what makes this work: it carries IPv4 between nodes over an IPv6 path, so a container
with IPv4 alone reaches a proxy at a `100.x` address on a machine with no IPv4 of its own.

Rejected: **NAT64 with DNS64**, which needs IPv6 inside every container, and Docker's bridges have
none; and **a tailnet exit node**, which sends all of a node's traffic through one other machine.
A proxy is a stopgap rather than the end: once nodes fetch what CI built from each other --
platform's `spec/architecture/relay.md` -- a node without IPv4 needs no GitHub at all.
