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

## The nodes

| Node  | Tier         | Failure domain | Expiry |
| ----- | ------------ | -------------- | ------ |
| `tyo` | `datacenter` | `oci`          | 2036   |
| `nrt` | `datacenter` | `oci`          | 2036   |
| `hnd` | `datacenter` | `oci`          | 2036   |
| `gvx` | `datacenter` | `azure`        | 2030   |
| `bru` | `datacenter` | `azure`        | 2030   |
| `buf` | `datacenter` | `racknerd`     | 2027   |
| `rdu` | `home`       | `home`         | --     |

**`gvx` and `bru` have no public IPv4**, inbound or, from late 2026, outbound. What only IPv4 reaches
-- GitHub's API, from which host takes every build, and `ghcr.io` -- reaches them through a node that
has it, over the tailnet, which runs on IPv6 -- see [../issues.md](../issues.md).
