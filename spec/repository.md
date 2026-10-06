# The repository

The infrastructure of the author's system, the layer everything else is deployed by: `host`, which
runs every app on a node, and `keeper`, which replaces host; `panel`, host's interface; `meter`,
which samples the node; and `caddy`, `tunnel` and `resolver`, the doors a request comes in by.
`libs/deploy` is what host and keeper share, and `libs/urls` the addresses of all of it. Why the
system is cut into this layer, the platform's and the services', is the workspace's
`spec/architecture/layers.md`, the picture of all
four repositories; the rules every repository keeps are the workspace's `spec/`, and what is here is
what infra alone decides.

## `apps/` is deployed, `libs/` is imported

Every app has a directory under `apps/<group>/` with its `service.toml` and, for one host runs, its
`Dockerfile`; every library one under `libs/`. A crate is listed in `Cargo.toml` by hand and a
package found by `pnpm-workspace.yaml`'s globs, so a TypeScript-only directory never breaks Cargo.

**The apps are grouped by what each does**, as the platform's are, so the two repositories read
the same way:

| Group     | Apps                    | What they are                                     |
| --------- | ----------------------- | ------------------------------------------------- |
| `deploy`  | host, keeper, panel     | what deploys every app, and the interface over it |
| `network` | caddy, tunnel, resolver | the doors a request comes in by                   |
| `observe` | meter                   | the node watched from inside                      |

**A group is a directory and nothing more.** An app's name is still its directory's own: host
chooses a container's shape by the app's name and keeps its data under `/data/apps/<name>`, and
an image and a container are named for the app, so moving an app between groups changes nothing on
the node. The tools find an app by `apps/*/<name>`.

## `nodes/` is the machines

What a node is declared to be, `nodes.toml`, and the files `mise run node` puts on the machine
itself -- the firewall, the units that load it and that hold Docker until `/data` is mounted -- one
directory per family of system. See [architecture/nodes.md](architecture/nodes.md).

## The other repositories are named, never linked

The platform is `monoflake/platform` and the site `canmi21/web`, each cloned beside this one in the
workspace. A rule of theirs is cited by name -- `platform's spec/architecture/cron.md` -- and `refs`
resolves it in that repository when it is cloned beside this one, so a renamed section still fails
here. A relative link across a repository resolves only while both are cloned side by side, so a
spec here never writes one.

**The platform's declarations are read from copies.** host and `libs/deploy` are tested against
the apps they run, and those are the platform's: `libs/deploy/fixtures/` keeps a copy of each
declaration a test reads. A platform change that a reader here must accept is copied in with the
change that makes it accept it.

## Reaching the LAN from a browser that cannot

**`mise run reach [name]` answers on `http://localhost:26520` for `<name>.internal.ixc.one`**, host's
panel when no name is given. macOS asks before a program reaches the local network, and a browser
an agent drives, like node from mise, is refused; the system's own `ssh` and `curl` never are.
So [`reach.ts`](../apps/deploy/host/scripts/reach.ts) has ssh carry the node's port 443 to a loopback
port and speaks to that alone, sending every request as the name would arrive: TLS with the name
as SNI and as `Host`, so Caddy routes it. Caddy's guard sees the node's own address, which is a
LAN one; a tunnel to the node's loopback is refused by the same guard, which is why the far end
is the LAN address.

Plain HTTP on this side, because localhost is a secure context: the session cookie's `Secure` is
kept and still sent. The port is `REACH_PORT` in `libs/urls`, beside the pinned ones and outside
their map, since what answers there is not an app.

## Publishing

**`@monoflake/urls` is dated**, `YYYY.MDD.N`, and published by `.github/workflows/release.yml`
whenever a push changes it, as the lib repository publishes its own -- its `spec/repository.md`,
"Versions and publishing", holds the scheme and why. Here it is read as TypeScript from `src/`;
`publishConfig` points what is published at `dist/`, which tsdown builds, because a consumer's node
does not strip types inside `node_modules`. Its first version was published by hand, as `0.0.0`,
since npm attaches a trusted publisher only to a package that exists.

This repository continues the history of `canmi21/web`, which was `canmi21/lattice`, from the commit
the three repositories split at; everything before it is shared with the other two.
