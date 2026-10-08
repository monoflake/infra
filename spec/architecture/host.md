# `host`: deploying to the machine at home

`apps/deploy/host` is a small deployment platform for the always-on machine on the home network -- the one
milestones D1 and D2 in web's `spec/todo/milestones.md` assume. An app in this
repository or in the platform's declares that it runs there, and a push to that repository's `main`
then reaches it without anybody logging into the machine. An image somebody else publishes comes in
the same way, by a directory of its own -- see "An upstream image is adopted, not rebuilt".

The machine has no inbound public address. It reaches out through a Cloudflare tunnel, and it is on
the tailnet. Everything below is shaped by that and by there being exactly one user. Which services
are placed on it, and how names and APIs reach them, is platform's `spec/architecture/services.md`.

## One name inside, and a domain label outside

An app has one name, and every place inside the node it appears is that name: `geo` is the app in
host, the container, `/data/apps/geo/` on the machine, its network and its logs. **What it is
reached by from outside is a DNS label of its own**, `[interface] domain` in its `service.toml`,
which is the name unless it says otherwise: gemini is `gemini.internal.ixc.one` and `gemini.canmi.app`.
A service's name is what code and people use; the label is what the address says, and it can
change without the service being renamed.

- A name and a label are DNS labels: lowercase letters, digits and hyphens.
- **A display name is for people, and for nothing else.** `display_name` in `service.toml` is what
  the console writes for the app -- `Geolocation` for `geo` -- in Title Case, spaces allowed,
  and two apps may share one. It never reaches code: a binding, a path, a container, a label and a
  log line all say the name, so a display name changes without anything being renamed. An app
  without one is shown by its name.
- Apps from every repository a node deploys from, and images from elsewhere, share the one
  namespace of names, and apps
  and routes share the one namespace of labels: no two things answer on one label.
- `host` and `keeper` are reserved names for the two programs below, `meter` for what samples the
  machine ([meter.md](meter.md)), `api` for the API host, `caddy` for the door host deploys and
  `tunnel` for the way in from Cloudflare (both below), `infra` as a label for host's door, `resolver`
  for the house's DNS, and `cloudflared` because the tunnel ran under that name before host
  deployed it. `gateway` is not among them: the platform's gateway is deployed here as an app.
- **Labels are reserved too, for what is on its way**: `cms`, for the editor, which keeps its own
  address until it moves. No app or route may take a reserved label.
- **The private suffix is a mirror of part of `.app`, and nothing else.** Every label is on `.app`, reached
  from the public side behind Access and from the LAN alike; `internal.ixc.one` carries a label only as a copy
  of what `.app` answers on it, same paths, same service, for reaching it on the LAN without
  Access. A label may be left off `internal.ixc.one` -- `lan = false` on an interface, `private = false` on a route -- and nothing
  is ever on `internal.ixc.one` alone. So keeper's whole interface is on `keeper.canmi.app` behind Access as
  well, and the API host answers every scope on both, the private ones included: the public never
  reaches a private scope, since Access stands in front of `api.canmi.app` and the gateway passes
  on only the scopes in its table, and our Workers reach every scope through the one VPC service.

**A label is declared in the repository, and the console will write it there.** The rule for every
setting a declaration holds: git is the one record, and a change made in the console becomes a
pull request against the repository the app's declaration lives in, which a bot opens, deployed like any other change once it is
merged. Until then the console shows it as pending. The bot is a GitHub account of its own, a
collaborator given access to the repository like a person, so what it proposes is reviewed like
anyone's and its token opens pull requests and nothing more. It is not built yet; until it is, a
label is changed in `service.toml`.

`internal.ixc.one` is private and `.app` is public, and what each admits is
platform's `spec/architecture/services.md`, "A domain says who can reach it, not what is behind it".

## One version runs, and a failed deploy puts the last one back

**Each app runs exactly one container.** A deploy stops it, snapshots its directory, starts the new
image and checks it. If the check fails, the directory is put back from the snapshot and the
previous image is started again. A deploy costs a few seconds of the app being down, which one user
accepts; what it buys is that two versions never write the same data at once.

**Rejected: Vercel's model of immutable deployments behind a moving alias.** It runs old and new side
by side, which is what makes its rollback instant and its releases gapless. Both are worth something
to many users sharing a service. Here they cost two copies of every app, and two processes over one
SQLite file, to save a pause nobody else sees.

### An app chooses how it is rolled out, and keeping nothing earns a gapless one

That rejection rested on two things: an app's state in a file on its node, and one user. Both are
going -- the platform's state moves into its database and its buckets, and friends use it -- so
**`rollout` in `service.toml` names one of three**:

- **`replace`**, the default: everything above. The app stops, its directory is snapshotted and the
  new version starts on it, a few seconds down, and a failed check puts both back. Every app with
  state on its node stays here.
- **`beside`**, for an app that keeps nothing on its node: no `[data]`, no sidecar, no socket, no
  role. The new version starts beside the running one as `<app>_next`, a name no app may take, and is
  checked and routed by its address on the app's network, which stays its own when it is renamed to
  `<app>` after; Caddy's route moves to it, reloaded without dropping a connection, an open socket
  kept thirty seconds; the old one is given thirty seconds after `SIGTERM` to finish what it is
  answering, and is removed. A failed start or check removes the new one and leaves the old one
  routed, so nothing is snapshotted and going back costs nothing. Both run at once for a moment, so
  a node whose `MemAvailable` is short of the app's ceiling and 256 MiB more falls back to `replace`,
  and the event says why. The event's stages after `starting` are `checking`, `switching` and
  `draining`.
- **`manual`**: a run's notice deploys nothing of it, and the operator deploys it a node at a time,
  as the platform's Postgres needs -- standbys first, the primary last; platform's
  `spec/architecture/databases.md`, "Upgrades are pinned, reported, and rolled by hand".

A version that runs beside its predecessor runs against the same database, so a change of schema
that both must survive is made in two steps, the old shape kept until the old version is gone --
platform's `spec/issues/scheduling.md`, "Schema changes go through the platform". And gapless on one
node is gapless for the service only when its nodes do not all switch at once, which is what a
version reaching one node first gives -- [../todo.md](../todo.md). Decided on 2026-10-07.

The snapshot is taken after the container stops, so it is never of a database halfway through a
write. `/data` is btrfs and **each app's directory is its own subvolume**, so the snapshot is atomic,
covers exactly that app, and costs nothing until something is written. The directory is therefore
created with `btrfs subvolume create`, never `mkdir`: a plain directory cannot be snapshotted alone.

**Two rollbacks, and only one restores data.** A deploy that fails its check restores the snapshot,
since the new version may already have changed the data. Going back to an older version days later
starts the older image on the current data, since restoring would erase everything written since. The
second is a choice offered, never a default.

host keeps the previous image of every app and the last few snapshots, and removes the rest. Nothing
else on the machine needs tending.

## The machine pulls; nothing pushes into it

**Rejected: CI building, then SSH-ing in to run `docker compose`.** Two reasons, each sufficient. The
credential CI would hold is root on the machine and a way onto the tailnet, so any compromise of CI --
a poisoned dependency, a hostile action, a leaked secret -- is a compromise of the house. And a
compose file can ask for anything: `privileged`, the host's root as a volume, the Docker socket. A
channel that accepts one has no boundary to enforce.

So the direction is reversed, and trust is moved off the channel.

- CI builds only the apps a push changed -- `.mise/tasks/deployable` reads the change against the
  crate graph, and a change to `Cargo.lock` or the root `Cargo.toml` rebuilds only the apps whose
  resolved dependencies it moved, read from the lockfile and checked with `cargo tree` for what each
  image compiles, every image when it cannot tell -- once for `linux/arm64` and once for `linux/amd64`, each natively on a runner of that
  architecture and each as its image archive beside its `service.toml`, and uploads
  them as artifacts of that workflow run. **Nothing is published**: no release, no package, no
  registry. This is one repository holding many apps, and a publishing ritual per app is exactly the
  cost that stops small apps being written. The build is `.mise/tasks/image`, the same one a local
  deploy runs, and CI checks the workspace out around this repository so the compiler is the one
  its `rust-toolchain.toml` names.
- When the run ends, **GitHub's webhook**, set on `monoflake/infra` and `monoflake/platform` alike,
  tells the platform's hook on `api.monoflake.com`, which checks the delivery's signature and passes
  the run's number and its repository to the node's host, and to keeper -- see platform's
  `spec/architecture/services.md`, "Every node is the same node".
  There is no polling and no step in the workflow for it; the event says exactly which run ended,
  and that it succeeded.
- host asks GitHub about that run with `GITHUB_ACTIONS_TOKEN`, a fine-grained token owned by the
  monoflake organization with Actions read on its sources, and **runs nothing unless the answer is
  one of its sources' `.github/workflows/deploy.yml`, on `main`, finished and successful** -- the
  one path `libs/deploy`'s `WORKFLOW` accepts. **It reads the declarations before any image**: the
  run's `declare` job uploads every planned app's `service.toml` apart, as `<app>.toml` in one small
  artifact named `declarations`, so host passes over an app placed elsewhere, rolled out by hand
  and not named, or held stopped, without downloading it. Then it downloads one image per app it
  takes, `deploy-<app>-<arch>` with `arm64` or `amd64`: its own architecture's, or the one the app
  asks for. A run without `declarations`, from before it existed, is read the old way, each
  declaration from its image. Every artifact is held to the digest GitHub recorded, the
  declarations too, and a declarations zip that is not what CI uploads -- past 256 entries, a
  declaration past 64 KiB, or not a zip -- fails the notice rather than falling back to downloading
  every image. Each image still carries its own `service.toml`, which keeper reads. The
  notice is a hint, not an authority: a forged one can at worst redeploy what `main` already built.
- **A declaration changed alone is applied without a new image.** An app's own `service.toml` is not
  an input of its image unless its binary includes it outside a test -- host's does, and keeper's
  includes host's -- so CI lists such an app apart, `deployable --declared`, and uploads its
  declaration alone. host keeps the image it runs and applies the change as narrowly as it can:
  `display_name`, `placements`, `rollout` and health are stored; `interface`, `api` and `schedules`
  are stored and Caddy and cron re-rendered; memory is updated live; and any other field, one host
  does not know included, recreates the container on the same image through the ordinary deploy.
  The new declaration is a new version over the same image, so a rollback returns to the old one.
  A node the app is not on yet, or one asking another architecture, takes the newest image built
  within 7 days, and skips saying so when there is none. A re-declaration of host, keeper or Caddy
  that restarts it waits for the canary; one that only stores or renders does not.
- **A run whose notice never came is taken anyway.** At start and every 15 minutes host lists each
  source's successful `deploy.yml` runs on `main` from the 7 days CI keeps artifacts, and a run it
  has no row of, newer than the oldest run of that repository it holds, was missed -- a host that
  holds none, freshly set up, misses none. Missed runs are taken oldest first through the path a
  notice takes, each app only from the newest run that built it, so an older build never replaces
  a newer one; an older run's copy is recorded as passed over, naming the run that is deployed. A
  missed run that built host goes to keeper on host's network. A run some of whose apps failed
  counts as taken: this recovers notices, not deploys. rdu lost platform's run 37710256103 on
  2026-10-08 while keeper was recreating its host, which is why.
- **An image is rebuilt only when what goes into it changed.** `deployable` compares each input as a
  build reads it, between the run's two revisions: Rust source by its tokens, so a comment or a
  reformat is no change; a Dockerfile, `.dockerignore` and the image task without their comments; a
  declaration by its value, so a comment there neither rebuilds nor re-declares; a file a binary
  includes, and what a build script reads, byte for byte. Prose, licenses and a crate's tests,
  benches and examples go into no image, and an app whose Dockerfile copies the workspace alone is
  its crate's binary, so nothing else in its directory is an input. The cost is accepted: a binary
  not rebuilt after a comment or a reformat keeps the line numbers it was built with, so a panic's
  location and `line!()` may name a line the source has since moved. The platform's `deployable`
  does the same.
- **A node's sources are its own to name, each with the scope its runs deploy into**, in
  `DEPLOY_SOURCES` in the node's `.env`, as `owner/name=scope`: today
  `monoflake/infra=infra monoflake/platform=platform canmi21/cue=canmi`, `infra` being infra's own
  and no scope -- platform's `spec/architecture/scheduling.md`, "A scope is an organization, and
  only the boundary isolates". The scope is the node's to say and never a declaration's. A run is
  numbered within its repository, so a notice names the repository with the run, and one that
  names a repository the node does not list is refused before GitHub is asked. Without the list a
  node deploys nothing rather than guessing; a bare `owner/name`, as nodes wrote before scopes,
  deploys into the scope it is named as. The hook keeps a list of its own, `DEPLOY_SOURCES` exported
  by the platform's `@monoflake/sdk`, to pass on only what some node might take; the node's
  decides. A repository outside the organization is read with its owner's own token,
  `GITHUB_ACTIONS_TOKEN_<OWNER>` in the node's `.env` -- `_CANMI21` for `canmi21/cue` -- as the
  platform's deployer names it, and with `GITHUB_ACTIONS_TOKEN` otherwise. A notice that names no
  repository, from before they did, means the node's source only on a node with one; a node with
  two refuses it.
- **An app belongs to the scope that first deployed it, and no other scope's run replaces it.**
  host records each app's scope at its first deploy and refuses, before stopping anything, a deploy,
  a re-declaration or a by-hand deploy of it from a source of another scope; infra's own names are
  `infra`'s alone, and keeper takes runs of `infra` only. An upload of a new app names its scope.
  The apps on the nodes before scopes were recorded as `infra`'s for infra's own names and the
  platform's otherwise. Two scopes holding one name is the next step -- [../todo.md](../todo.md), "A
  deploy belongs to a scope".

**Rejected: verifying a Sigstore attestation of each archive.** An attestation proves an artifact came
from a given repository's workflow on a given ref, which is what matters when the artifact is taken
from somewhere else -- a registry, a mirror. Here it is taken from GitHub's API, for a named run whose
repository, workflow, branch, event and outcome that same API states, over the same TLS. Both prove
the same thing, and the attestation's half is a certificate chain, a transparency log and a trust
root to verify in Rust, the most intricate code on the path for no guarantee the run record lacks.

An artifact expires after its retention period. That does not matter to a deploy, since the image is
on the machine once loaded, and a rollback uses the image host kept.

A local deploy is `wrangler deploy`'s shape: built on the Mac, which is arm64 like the machine at home -- `PLATFORM=linux/amd64` builds for an
x86 node -- into
the same archive, and uploaded by a mise task to an interface that answers only on the LAN and the
tailnet. What admits it is the token below, not an attestation. So there is one artifact format with
two sources, and no registry anywhere.

## What a deployment may ask for is host's decision

An app declares what it needs; host turns that into a container and decides which capabilities exist
at all. A network of its own shared with Caddy alone, the app's own directory and nothing else, no
`privileged`, no host network, no Docker socket, resource limits always. This translation is the
whole of what host adds over a compose file, and it is why a manifest declares rather than executes.

### A role is asked for by the app and granted by the node

**A service above infra that needs more than a sandbox asks for a role, and the node grants it.**
The roles are what host can make of a container beyond the sandbox: the scheduler, which is given
every job's table and each socket service's directory; the steward, which runs as root with the
machine's D-Bus socket; the reporter, which is given host's account of the services and the
meter's readings; the peer, which talks to the same app on every other node -- its declared port
published on the machine at the same number on every address, which the firewall admits from the
tailnet alone (see [nodes.md](nodes.md), "Nothing comes in but over the tailnet"), and joined to
host's own network to read its node's host with the read token; a peer that names a `port` of its
own in `[shape]` has that one published instead, and its declared port stays on the node for host
to check, which is how the platform's Postgres publishes 5432 and keeps its keeper's HTTP to itself;
`ports = [...]` names several in place of `port`, one program serving the tailnet on more than one,
as Postgres beside Patroni's REST and etcd's client and peer ports, and host refuses a peer whose
port another peer on the node already publishes before it stops anything; the identity, a hostname
and a MAC address of its own that every version keeps, for a program that takes a new machine for a
new device -- `hostname` and `mac_address` under `[container]`, the MAC unicast and locally
administered, so no real card holds it, set on the app's own network alone; the proxy, which every
app reaches by name -- sandboxed on a network of its own, answering on a port and publishing none,
and joined by host to every app's network but infra's own, before each new version of an app
starts, when host starts, and to all of them once the proxy itself is deployed, a join that fails
being logged rather than failing the deploy; the driver of a kind, whose image
every sidecar of that kind runs; and a claim on hostnames, which Caddy routes and the resolver
answers. An app asks in its `service.toml` --
`[shape] kind = "scheduler"`, `[driver] provides = "objects"` -- and the node's `.env` grants, in
`GRANTS`, as `app:role` pairs: `cron:scheduler objects:objects`. An app with a MAC of its own is
never rolled out beside itself, since two versions would share it on one network, and host refuses,
before stopping anything, one whose MAC another app on the node already holds.

**Both keys, always.** A declaration ships with an image CI built, and anything that reaches CI
could write one, so asking alone grants nothing: an app that asks for a role the node does not
grant it is refused before anything is stopped, rather than run sandboxed to fail somewhere later.
A grant alone does nothing either, since an app that asks for no role gets none. So a role is the
operator's decision as every privilege here is, and host knows roles rather than the names of the
services that hold them -- which is what lets infra be built without naming anything above it. See
the workspace's `spec/architecture/layers.md`, "What the package graph cannot see".

Infra's own are the exception, shaped by name as before: host and keeper, the meter, Caddy, the
tunnel and the resolver are what the node is made of, and naming them is infra naming itself. **host's own network admits keeper and Caddy, by name, and an app run as a peer**, by the shape it is
actually run in, so an app of infra's own granted `peer` is not let in by the grant.

**An app's directory belongs to the user its image runs as.** host creates it as root, and an image
that runs as someone else -- the meter and the resolver as 65532 -- could not
write to it. So before each version starts, host reads the image's `USER` and, when it is a number
other than root, gives the directory itself to that user and group; what is inside is left alone,
since it was written by the app. A user named rather than numbered would need the image's own user
table, which host does not read, and is left as root's.

**Every container has a memory ceiling, and no swap past it.** A declaration states `memory_mb` and
host gives 512 without one; host's and keeper's own containers get theirs the same way, and swap is
set equal to the limit, since a ceiling that can be exceeded into swap only makes the machine
slower. Each figure is the container's measured use with room above it: geo, measured at 290 MiB
held and 130 more pushed into swap against a 512 limit it met sixty times, has 768; host 128,
keeper 64, the meter 32 and Caddy 128, against 50, 9, 19 and 20 measured on 2026-09-28.
**A ceiling is set against the peak, never the idle figure**: keeper idles at 9 MiB and passed 16
fetching host's image, and at a ceiling of 16 it was killed mid-deploy.

**A large file is written in chunks that leave the page cache as they land.** Pages a container
dirties count against its own memory ceiling, so writing a 347 MiB image through host's 128 had
the kernel kill host on `tyo` on 2026-10-07 with 16 MiB of its own in use. Every file host or keeper
writes from a stream -- an upload, an artifact's zip, the image taken out of it, a report -- is
flushed every few MiB and the flushed range dropped from the cache, and a large file read back
whole, as an image is when loaded, drops what it has read the same way. The ceiling stays what the
process needs, not what the files it handles weigh.

### An image is built for speed, and for any node of its architecture

**The same source builds the same image, byte for byte**, so a node can tell an unchanged rebuild
from a change: **an image the app already runs, under the same declaration, is not deployed
again.** host reads the archive's ids -- `index.json`'s manifest digest, the image's id under the
containerd store the nodes run, and `manifest.json`'s config digest, its id under the older one --
from the tar headers before loading anything, and when one is the image the app runs, its
container is running, it is not held and its declaration is equal, the row closes as skipped,
"unchanged: run N built the image this node already runs", nothing loaded or restarted. Such a row
settles the run's app and counts as deployed in the canary's verdict, and keeper does the same for
host. The first build after reproducibility changed every image once. `.mise/tasks/image` builds with `SOURCE_DATE_EPOCH=0` -- fixed, not the commit's
time, so an app rebuilt by a later commit without changing comes out the same -- and rewrites every
layer's timestamps to it; the Rust image is pinned by digest for each toolchain, and a toolchain
with none pinned stops the build; and rustc runs through sccache by a wrapper of its own, since
sccache wrapping the C compile of a crate such as mimalloc would drop the epoch and stamp the wall
clock into the binary. Measured on 2026-10-08: two clean builds of each of infra's images came out
identical. A machine whose cache mounts predate this holds objects stamped with an older time, and
its local builds differ from CI's until `docker buildx prune --filter type=exec.cachemount`.

Every Rust program's image compiles its binary with the `container` profile in this repository's `Cargo.toml`, as in the platform's: full
optimization with fat LTO and one codegen unit, no debug information and no symbols. Speed is
chosen over size because a server pays for its binary on every request and for its bytes never;
the build is slower, and it runs on the Mac, where nobody is waiting on a request. No `target-cpu`
is set, so an image is not tied to the chip of the node it was first built for. The profile is its
own rather than `release`, which a local release build would otherwise inherit and pay for.

**An arm64 image runs on ARMv8.0, the oldest arm64 node, `rdu`'s Cortex-A72.** It has no LSE atomics,
which `tyo`'s Neoverse N1 has, so nothing is built for more than `armv8-a`: no `target-cpu`, no
`-march` above it, and what is faster on a newer core is chosen at run time, as Debian's own builds
do -- Postgres's CRC32C and its atomics among them. An adopted image is proved on `rdu` before it
is trusted, since an upstream built for ARMv8.2, as ClickHouse's is, stops there on its first
instruction from beyond. Such an image may still run, placed where the cores are new enough.

**Every Rust image compiles through sccache, so a crate built for one image is a hit for the next.**
A CI runner's builder starts empty, so a cache mount alone kept nothing between jobs and every image
compiled its dependencies again. The backend is whatever the environment starting the build names:
GitHub Actions' cache in CI, its credentials handed to the build as a secret by
[`.mise/tasks/image`](../../.mise/tasks/image) and never written to a layer, and a cache mount on a
machine with none named. It moves to a bucket of the platform's own once there is one -- platform's
`spec/architecture/scheduling.md` -- by naming it in the workflow, with no Dockerfile changed. A
cache that cannot be reached leaves rustc compiling alone; it never fails a build. The binary is the
same either way.

**A Rust program's image is the binary on `scratch` and nothing else.** Each is linked statically against
musl, so it needs no C library from the image, and the image holds the binary and, for geo, its
data. musl's own allocator is slow under many small allocations, so every program sets mimalloc as
its allocator; without it the static binary would be the slower one. **mimalloc is compiled with
transparent huge pages off**, `-DMI_DEFAULT_ALLOW_THP=0` in each image's build: left on, it asks the
kernel for 2 MiB pages, and every 2 MiB its heaps touch then stays resident whole, which held geo at
40 to 55 MB of memory around a heap of 2.5 MB on 2026-10-07, against a ceiling of 64. Off, mimalloc
also turns the pages off for the process, which a node whose kernel uses them always needs; set at
compile time, it holds for a binary run outside its image too. host and keeper make btrfs's
ioctls themselves rather than running `btrfs`, which is what let them leave Debian: there is no
`btrfs` in an empty image, and no command line to inject into once there is no command. The
target follows the platform being built, so the same Dockerfile serves an x86 node.

### An app may ask for one architecture

**`arch = "arm64"` in `service.toml` runs the app's arm64 image on every node it is placed on**, natively
on an arm64 node and emulated on an x86 one that declares `emulate = ["arm64"]` -- [nodes.md](nodes.md),
"An x86 node may run arm64 images, emulated, and never the other way". Without it a node runs the
image of its own architecture, as before. host fetches the run's artifact of the asked architecture,
and refuses, before anything is stopped, to place an app on a node that can run that architecture
neither natively nor by emulation. Only `arm64` may be asked.

### The declaration is `service.toml`, beside the Dockerfile

An app states what it needs in `apps/<name>/service.toml` and ships it with its image. host is a
program deployed apart from the file it reads, so the file carries a `version` and host refuses one
it does not know before reading anything else, while a key it does not know is ignored -- see the
workspace's `json.md`. What the keys are is
[manifest/mod.rs](../../libs/deploy/src/manifest/mod.rs); `libs/deploy`'s tests read the copies of the
platform's declarations under `libs/deploy/fixtures/` -- see [../repository.md](../repository.md) --
so a change there is copied in with the reader that accepts it.

An upload is written to disk whole before anything is stopped, so a transfer cut short never leaves
an app down.

## One token, behind two doors

No account system: there is one user. Reaching host from the public goes through the tunnel, Caddy's
door and Cloudflare Access in front. Behind that, and directly on the LAN and the tailnet, **one long
API token is required everywhere** -- the one exception to the apps behind Caddy authenticating
nothing. host holds the Docker socket, so its token is root on the machine, and a device on the LAN
without it gets nothing.

- It lives in this repository's `secrets.json`, so a mise task deploys without asking. On the
  machine it lives in host's own `.env` and is read back over SSH when forgotten. A browser keeps it
  as a saved password.
- **It never goes to GitHub.** The one secret GitHub holds is the webhook's, and what it signs can
  do nothing but ask host to look at a run it will check for itself. A compromise of CI must not be
  a compromise of host, or the reversal above bought nothing.

## host never updates itself; keeper updates host

An updater cannot be the thing it updates: a broken update leaves nothing running that could undo
it. So there are two programs, and each updates the other, never itself.

- **keeper** is small and rarely changes. It deploys host by the same stop, snapshot, start and check
  as any app -- one procedure, in `libs/deploy`, that both programs call -- against host's
  `/health`, which answers only once host reads its own database and reaches Docker. On failure it
  puts the previous host back.
- **host** deploys everything else, keeper included.

**Infra's shape is chosen by name, never by a declaration.** host and keeper run privileged,
with the Docker socket and the whole of `/data`; `meter` runs as an observer, which
[meter.md](meter.md) describes; and every other app runs as the section on what a deployment may
ask for describes. Which shape a container gets is decided by the program deploying it from the
app's name -- host, keeper, the meter, Caddy, the tunnel and the resolver, and only those -- so no
`service.toml` can ask for infra's reach. host deploys keeper and the meter; keeper deploys host. Both start from the one `.env` in host's directory, which is why the token has
one home on the machine.

**keeper keeps no state.** Every container carries the version it runs in a label, so keeper reads
what host runs back from Docker; a host started by hand carries none, and the declaration keeper was
just sent stands in for it beside the image it really runs. Each program removes old images only of
what it deploys -- host of the apps and keeper, keeper of host -- so neither can remove the other's
way back.

**host rolls forward, not back.** keeper guards one failure only: a host that does not start.
Anything wrong with a host that does start is fixed by pushing the next host, because that update is
carried by keeper and never by the program that is broken. That is what lets keeper stay small
enough to read at once, and its stability comes from its size rather than from rules about it. CI
builds only what changed, so keeper's image moves only when keeper's code does.

**keeper has its own intake.** `mise run host deploy host` goes to `keeper.internal.ixc.one`, never to
host, and a notice about a run that built host goes to keeper too. Routed through host, a broken
host would stand between the fix and the machine. keeper's whole interface is on both suffixes,
like any app's: `keeper.internal.ixc.one` on the LAN and the tailnet, and `keeper.canmi.app` behind
Access. `/notice`, the path the Worker reaches it by, is one route of that interface rather than an
exception cut through it, and a request there can only ask it to look at a run.

**A run that built host is keeper's first, and host's only after.** The notice reaches both at once,
and the first run that built both acted on it at once: host replaced keeper while keeper was
fetching the new host, and the host it was about to put in place never arrived. So host leaves such
a run alone, and keeper, once host is replaced -- or put back, if the new one failed -- passes the
run on to host marked as done with host. The rest of the run, keeper included, is then deployed by
the host that run built.

**What keeper does to host is in host's history.** A recreate, and an upload through keeper,
reach host once it answers again as one finished row, app `host`, source `{"kind": "keeper"}`,
with keeper's start and end and its detail -- `POST /api/apps/host/history`, the full token alone,
on `app-host` and not through the door. It cannot be a running row, host being down for the part
that matters; keeper retries for a minute, so a row is lost only when no host answers at all. A
host replaced by a CI run is reported as it was, by the run.

**A changed `.env` reaches host through keeper.** host reads its `.env` only when its container is
created, so keeper recreates it on request: `POST /host/redeploy`, admitted by the token like its
upload, replaces host with the version and image it already runs, reading the file on the way.
`mise run node` asks for it whenever it changes host's `.env`, and `mise run node recreate-host
<node>` asks by hand. keeper refuses with 409 while host lists an event running, since replacing
host mid-deploy would leave that app half replaced; the check and the replacement are not one step,
so a deploy that starts between them is cut short and closed as failed when host starts again. A
new host that fails its check is put back on the environment the old one ran with, read from its
container rather than the file, so a bad `.env` cannot take down both. keeper's own copy of the
file is read only when host redeploys keeper. keeper noticing a changed file by itself was weighed
and left for later: it would replace host at a moment nobody chose. host reloading itself was
rejected: a bad `.env` that crashed it would leave nothing outside it to replace it.

**A new host, keeper or Caddy reaches the canary first.** Those three are the door every notice
comes through, and a door that breaks on all seven nodes at once -- as one did on 2026-10-07 --
leaves nothing to deliver the fix. So one node, nrt, is the canary: `canary = true` in
`nodes.toml`, on exactly one node, which `mise run node` checks and writes into host's `.env` as
`CANARY` -- `self` on nrt, nrt's tailnet IPv4 on every other node. nrt is neither core nor short of
IPv4, an ordinary cloud node like most; a Caddy change that breaks only rdu's LAN side passes it,
and one broken node instead of seven is still the gain.

- **The canary takes such a run as it comes.** Every other node holds it: keeper before replacing
  host, and host for the whole run, each of its apps a row in stage `waiting`.
- **The others ask nrt, over the tailnet.** `GET /api/runs/<owner>/<name>/<run>?built=<apps>`,
  `built` naming which of the three the run built, with the read token, sent to nrt's tailnet
  address on port 80 as `canary.<private suffix>`. Only the canary's Caddy renders that name, and
  passes on that one path, a GET, from `100.64.0.0/10`; anything else asked of it is a 404, and no
  other node's Caddy changes. Only the canary's host answers the route. tailscale masquerades what
  it forwards from the tailnet into a container unless told not to, which would show Caddy the
  edge network's gateway rather than the asking node, so the canary's setup runs
  `tailscale set --snat-subnet-routes=false`; nrt advertises no routes, so nothing else changes,
  and no other node is touched, its peers' database and relay seeing sources as they do today.
- **The verdict is `passed`, `pending` or `failed`.** It is `failed` when nrt failed or passed over
  one of the apps for that run, and `pending` while one is not yet taken, still deploying, or not
  answering its health. It is `passed` once each is deployed by that run and keeper and Caddy
  answer their health, host answering being host's own. keeper tells host how its replacement of
  host went when it passes the run on, so host's history holds host's deploy as it holds any
  app's, and the verdict reads it there. A row names the repository of its run, so another
  repository's run of the same number is not it.
- **The others ask again** after 30 s, 60, 120 and 240, then every five minutes, for two hours. An
  ask that gets no answer counts as `pending`. On a pass the held rows move on to `admitting`. On a
  failure, or at the deadline, they close as skipped, saying why, and keeper, not replacing host,
  passes the run on saying so.
- **`mise run node deploy` overrides the hold.** Its notice says it is sent by hand; it is taken
  apart from the hook's notice of the same run, deployed at once, and a hold on that run gives up
  at its next ask, its rows closed as "deployed by hand instead".

Reaching nrt through its Caddy proves that Caddy routes to host. keeper's own door,
`keeper.<suffix>` through the tunnel, is not proven by the verdict: only the hook's next notice to
nrt's keeper proves it. A host that restarts while holding loses the hold, and its rows close as
failed. The run that brings the canary in is taken by hosts and keepers from before it, so it
reaches all seven at once.

It is reached through Caddy like everything else, which was chosen over binding keeper's port to the
machine's address directly. The cost is that a Caddy that is down makes keeper unreachable too;
accepted, because Caddy starts from the file host last wrote and fails independently of host, so
the case keeper exists for -- a broken host -- leaves Caddy standing.

**Rejected: two identical full instances, A active and B standby.** Two holders of the Docker socket
need a leader election, and the standby logic would live inside the program that changes most. The
variant where B catches up once A succeeds also discards the fallback at the moment a latent bug is
still invisible.

The first host is started by hand, from its `docker-compose.yml`, since nothing earlier exists to
start it. keeper is never started by hand: the first one is deployed by host.

## The control plane going down is not an outage

Containers are kept alive by dockerd's restart policy, and routes live in Caddy, not in host. So a
host or keeper that is down means nothing can be deployed, and nothing stops being served.

**Every container is given twenty seconds between `SIGTERM` and `SIGKILL`, whoever stops it**: host
and keeper stop and restart with that grace, and each container is created with it as its
`StopTimeout`, so dockerd stopping on a reboot or an upgrade gives it the same rather than its own
ten -- enough for the platform's database to stop in order.

**A node's Docker starts only once `/data` is mounted.** Every container binds a path under it, and
Docker started without it creates those paths empty on the root disk: Caddy comes up with no
configuration, host with a new database, and every app on nothing, all of it looking like a clean
start. `/data` keeps `nofail` in fstab, so a node with a failed disk still boots and answers ssh,
and a drop-in gives the Docker unit `RequiresMountsFor=/data`, so it waits for the mount and does
not start without it. Nothing needs starting in order beyond that: every container restarts on its
own policy, and Caddy starts from the file host last wrote whether or not host is up yet.

### Caddy is deployed like any app, and is the one door

**Caddy is `apps/network/caddy`, built here and deployed by host, in a shape its name alone gets: the
edge.** The official build with two modules, xcaddy's Cloudflare DNS provider for its certificates
and the rate limiter host renders each service's limits into; declared, versioned, rolled back and
shown in the console as every app is. The shape differs from an app's sandbox in four things:

- Its ports, 80, 443 and 443 over UDP, are published on the machine; no other container publishes
  any but the resolver, which publishes DNS's.
- It stands on the `edge` network, which the tunnel shares, rather than on one of its own, and
  after every deploy host joins it to each app's network again, since a new container is on none.
- Beside its `data/` -- certificates, and the admin socket -- it mounts `host/`, the configuration
  host writes, read-only, and `config/`.
- It keeps one capability, binding a low port; its root is read-only as an app's is.

Its health is its admin socket, `data/admin.sock`, asked `/config/` as the meter's is asked
`/health`, and that socket is where host loads every configuration. The Cloudflare token its DNS
provider proves certificates with is its `secret.env`, as any app's secret is. **Without the token
it does not start at all**, so a node's first Caddy deployed by host needs `secret.env` in place
before it, or every door closes; the one it replaces is brought back by hand from the compose file
beside it, `docker compose up -d`.

**A node with no LAN asks for no certificate.** Its LAN side serves the house's devices the node's
own names over TLS, so where host's configuration has no `LAN_ADDRESS` it renders neither that side
nor the certificates that serve it: such a Caddy answers its tunnel and the private API on its
Docker networks, both plain HTTP, and needs no DNS token. One token that can write DNS stays on one node rather
than on every one -- the direction platform's `spec/architecture/scheduling.md` sets for
certificates held in one place.

### The tunnel is deployed like any app, at the address Caddy trusts

**cloudflared is `apps/network/tunnel`, deployed by host in a shape its name alone gets**: sandboxed as an
app is, but standing on `edge` at `tunnel_source` from host's configuration, and at
`tunnel_source6`, `fd34:1053:16bd::20`, where `edge` has IPv6 -- the two addresses Caddy believes
`Cf-Connecting-Ip` from, so a visitor's address is only ever taken from the tunnel. Both are needed:
Workers VPC resolves `caddy` through the tunnel and reaches it over IPv6 when it can, and a Caddy
trusting the IPv4 alone refused every such request, as it did on all eight nodes on 2026-10-08. Its
routes are the dashboard's, a remotely-managed tunnel; its token is `TUNNEL_TOKEN` in its
`secret.env`; its health is its metrics server's `/ready`, on its port, which host reaches by
standing on `edge` too. A tunnel that is down closes the public side and the notices CI sends, so
the one it replaced is brought back by hand, and the LAN and the tailnet, which never pass it, are
how.

### Every node answers the private API, and sends on what is not its own

**Caddy answers `api.<private suffix>` on every node, in plain HTTP on its Docker networks.** When
Caddy joins an app's network it also answers there by that name, so a container asking
`api.internal.ixc.one/{scope}/...` reaches its own node's Caddy and never leaves the node to find
it. A scope deployed on this node goes straight to its service; any other is rewritten to
`/v{n}/{scope}/...` -- `v1` where the path names no version -- and sent over TLS to the public
gateway with `X-Internal` set to `INTERNAL_TOKEN` from Caddy's `secret.env`, over whatever the caller
sent, so a call across nodes is routed by the one gateway -- platform's
`spec/architecture/gateway.md`, "Inside a node, its own services answer locally". The route admits
only the node's app networks -- never the LAN or the tailnet, which keep the LAN side -- refuses
the tunnel's address, and drops any `X-Internal` a caller sent; the token is set only for a private
scope, and a public one goes on as any caller's request would. A node knows only its own apps'
declarations, so the private scopes placed elsewhere are named in its `.env` as `PRIVATE_SCOPES`,
and the app networks it admits as `APP_SOURCES`, Docker's default pool when left out. The public host is
`PUBLIC_API` in host's configuration, `api.monoflake.com` when left out.

**A scope says which sides carry it**, as `sides` under `[api]`: `private` and `tunnel`, both when
left out.

### The resolver serves the house, and answers nothing of its own

**The house's DNS is `apps/network/resolver`, CoreDNS adopted from upstream, in a shape its name alone
gets**: sandboxed as an app is, and publishing 53 over UDP and TCP on the machine, the one container
beside Caddy that publishes anything. host renders its whole configuration, as it does Caddy's, and
nothing about it is written by hand. It runs only on a node with a LAN.

**It answers no name with the node.** The gateway's names reach the house through Cloudflare as
they reach everybody -- platform's `spec/architecture/gateway.md`, "Inside a node, its own services
answer locally"; until 2026-10-07 it answered them with the node, and the house's gateway behind
them is retired. **A query passes down one chain, and a step that fails is skipped, never waited
on:**

1. **A filter**, when one is deployed -- an ad blocker, say -- asked first. It is to be in the chain
   because it runs, and out of it because it does not, with nothing changed by hand. The step is
   rendered and tested, but no deployment feeds it yet: host passes an empty list of filters. A
   name it blocks comes back blocked: that is an answer, not a failure, and is not asked again
   further down. Its own upstream is the router or a public resolver, never this one, or a query
   would go round in a circle.
2. **The router**, then **the public resolvers**, from host's configuration, since they are the
   node's.

Each is asked in that order, the first that answers wins, and one that is down -- timing out,
refusing, failing its health check -- is passed over until it answers again. A cache sits in front,
so the chain is walked once per answer's lifetime.

**The resolver is the house's first DNS, and the router its second.** DHCP hands both out: with the
node down, a device falls back to the router and loses only the filter. The router's own upstream is
never the node, for the same circle's sake. The tailnet asks it nothing: its split DNS for the
gateway's zones went with the house's gateway.

### An upstream image is adopted, not rebuilt

**An image from elsewhere becomes an app by a directory like any other**: its `service.toml`, and a
Dockerfile that is the upstream image at a pinned version, with only what the node needs said on
top -- its user by number, where its data and its port are. A new version is that one line changed,
and CI deploys it as it deploys everything. What it keeps secret stays in its `secret.env` on the
node and never reaches the repository. `tunnel` here, and the platform's `gemini`, came in this way; an app with a page
not at its root declares `home` under `[interface]`, and Caddy sends `/` there.

### host renders all of Caddy, and Caddy remembers nothing

Caddy's whole configuration is derived from host's state: every app, every name, every upstream that
is not a container -- the NAS behind `nas.canmi.app` is one. host renders it complete, writes it to
the file Caddy starts from, and then loads the same bytes through Caddy's admin API. Never a partial
patch, never Caddy's own autosave, never `--resume`. A full render is cheap, and one derivation means
the state host shows is the state being served.

**The file is what keeps this section true.** Pushed alone, a Caddy that restarts before host --
after a reboot, say -- would start empty and serve nothing until host came up. The file is written
by host and is the same bytes it pushes, so it is not a second record, and Caddy recovers alone.

The Caddyfile is retired with this. What it held by hand is entries in host.

**A route to a device that speaks only TLS is reached over TLS, unverified.** The UniFi router
answers on HTTPS alone, under a certificate it signed itself, and its route is written
`https://10.10.10.1`. Caddy connects over TLS and does not check that certificate: the only way to
check it would be to pin it, and a device that makes itself a new one on an update would then stop
answering with nothing to say why. The hop is the LAN, between two machines in the same house, so
what verification would guard against is not on the path.

Such a device also believes it is at its own address. UniFi refuses a WebSocket whose `Origin` is not
its own, so every live view of its interface failed behind the proxy while the pages themselves
loaded. Caddy therefore rewrites `Origin` for these routes -- but only an `Origin` that is exactly
the name it is served under, which becomes the device's own. Any other origin reaches the device
unchanged, so the check the device makes against another site opening its socket in the author's
browser still stands; it is translated, never switched off.

**A name may send its root elsewhere.** An application whose interface lives under a path -- gemini's
panel is under `/admin` -- is given a `home`, and a request for exactly `/` is redirected there with a
307 while every other path reaches the application untouched. Caddy sends the root to `home` and no
further; where the application goes from there is its own. It is a redirect and not a rewrite: the
browser then asks for the paths the interface expects, and an API under `/v1/` never meets it. A
`home` has to stay on its own name -- `//` and `/\` are another site to a browser -- or the name
becomes an open redirect.

**Every name is compressed at Caddy, and no app compresses for itself.** Each route host renders
encodes its answer with zstd or gzip, whichever the client prefers; an answer that arrives already
encoded is passed through. Compression is a property of the edge of the node, like TLS, so it is
configured once there rather than in every app and every vendor's image. A `text/event-stream`
answer is left uncompressed, since an encoder holds what it compresses and a stream has to arrive
as it is sent.

**Caddy's admin endpoint is a unix socket, never a port.** Every app shares a network with Caddy,
so an admin port would let any app rewrite every route. The socket sits in a directory only Caddy
and host mount.

**A Caddy container that is recreated, not restarted, comes back attached to no app's network.**
host attaches it to all of them again whenever it starts and whenever it is asked to reapply, so the
remedy is one request rather than a list of commands.

## host has no interface on the node, and a door Caddy keeps

**host has no interface of its own, and no node runs one.** The view of every node is the
console at the edge, web's -- web's `spec/roadmap.md`, "One console runs the system, and it is
this layer's" -- and a node keeps only host's API. The panel that was each node's interface,
`apps/deploy/panel`, retired on 2026-10-07. host holds the Docker socket and the whole of `/data`,
so nothing that faces a browser runs beside it.

- **host answers on its own network alone.** It binds its port to its address on `app-host`,
  which keeper and Caddy join; every app network host joins to check an app's health leaves that
  port out of reach.
- **Caddy keeps one door to it, `infra.<suffix>`, and an allowlist behind it**: `POST /notice`,
  which is how CI's notice reaches host, and the `GET` routes the console reads -- the node's now
  and its series, the apps, an app, its history, its series and its health, the events, the disk.
  Everything
  else on that name answers `404`; Caddy adds no authentication and passes `Authorization` through,
  so host's own tokens are the only check. The name is reserved, so no app or route can take it.
- **Everything else is asked over the tailnet**, by `mise run node <verb> <node>` on the author's
  machine: it reaches the node by SSH, forwards a local port to host's address on `app-host`, and
  hands curl the token on stdin, so it works with Caddy down and needs nothing above infra -- the
  workspace's `spec/architecture/layers.md`, "Four places, and which way they lean". Its verbs are
  what an operator does to an app below, plus `apps`, `events` and `deploy`.

**The token is taken from the `Authorization` header alone.** **A second token reads and does not
act**: `HOST_READ_TOKEN`, optional, admits `GET` and is refused anything else -- what the
console carries, since the token above is root on the machine; see web's
`spec/architecture/console.md`. From the public, Access stands in front as well.

**Notifications and more nodes -- [nodes.md](nodes.md) -- come after the first version**, which deploys the apps of this
repository and the platform's and adopts upstream images.

### What host keeps, and where

host's state is four SQLite files by what they hold, since a file costs nothing and one per
subject keeps each small, separately inspectable and separately backed up: `apps.db`, what runs
now and what is held stopped; `routes.db`, the names that reach something host does not run;
`history.db`, every event; and `images.db`, each image flagged for removal and since when. They sit in host's own data directory. A single `host.db` from before
the state was split into four files is read into them once and renamed aside.

**Every event is kept, and none is pruned.** A deploy, a redeploy, a rollback of either kind, a
start, a stop, a restart and a deploy skipped are each a row: which app, what started it -- a CI run
and its commit, an upload, or an operator -- the image, when it started and ended, how it ended, and
why when it failed, with the logs of the failure. They are paged fifty at a time, the newest first, as far
back as they go.

**A deploy is one row that moves through its stages as it happens**: `waiting` for the canary,
when the run is held -- "A new host, keeper or Caddy reaches the canary first" -- then `downloading`, `admitting` --
the declaration read, its placements and any hold weighed, `admit` passed -- `loading` and
`starting`, which runs from stopping the old container to the new one passing its health check. A
finished row keeps the stage it last reached, so a failure says where it happened. An artifact a run
carries for an app placed elsewhere, or held, is a `Skipped` row saying which, so every node can say
what it did with every run. `/api/events` pages every app's rows at once, for a view of the node
rather than of one app. A row host left `Running` when it stopped is closed as failed when it starts
again, since nothing will finish it.

**Every line an app writes is kept.** Docker does not rotate the logs of a container host runs, and
before a container is replaced its whole log is written to `/data/logs/<app>/`, one file per
version it ran, since removing the container would otherwise remove its log, and both are read on
the node. Clearing them out is a later decision,
made when the disk says so.

### An app's environment is two files

Configuration and secrets are both environment variables, given to the container when it starts.
They are two files in the app's own directory, outside what its container mounts, readable by root
alone and edited over SSH:

- `config.env` is configuration.
- `secret.env` is secrets, moved there through sops and a pipe and never printed.

They sit in the app's subvolume, so the snapshot a deploy takes holds them, and a failed deploy put
back puts the environment back with the code. A change applies when the container is next started
from its version, which a redeploy does.

**host adds one variable of its own, `NODE`, the node's name**, over whatever the two files say, so
an app knows where it runs without being told per node -- the probe records it as the place it
looked from.

### What an operator can do to an app

- **Redeploy** runs the current version again, as a deploy: snapshot, start, check, and the version
  before put back if the check fails.
- **Roll back** runs the previous version instead, keeping the data and the environment as they are
  now. It is the ordinary way back from a bad version.
- **Roll back with data** does the same and also restores the snapshot taken before the current
  version was deployed, so the data and the environment are as they were then. Everything written
  since is lost, so it is shown as the dangerous one. It is offered while that snapshot is among the
  ones kept.
- **Start, stop and restart** act on the container as it is.
- **Remove** stops an app and takes away its container, its network and its routes, keeping its
  history, and its data unless asked to drop it too -- for an app no longer placed here. host,
  keeper, Caddy and the tunnel are refused.

**host is listed beside the apps it runs**, read back from its own container's label, since keeper
keeps no record and host none of itself: its logs and its version are there, with no previous
version and no environment, which is its `.env` beside the compose file and read by nothing here.

**The node's own four -- host, keeper, Caddy and the tunnel -- are restarted and never stopped or
started.** Each stopped takes a way in or the way back with it: host answers, Caddy carries the
console's reads and CI's notices, the tunnel is the public side, and keeper is what replaces host.
host does not redeploy or roll itself back either, since keeper is the one that replaces it; the
other three are redeployed and rolled back like any app. A restart of host or Caddy -- what answers
the request and what carries it -- is answered first and done half a second later. host's own is recorded as done when asked, because nothing of it is left to finish
the record once it restarts.

**A stop holds until a start.** A stopped app stays stopped through a reboot -- Docker's own
restart policy does that -- and through a deploy: while it is held, a CI run that built it is recorded
as skipped rather than started. A start runs the version it was stopped at; a redeploy, a rollback
or an upload is a choice to run something, and ends the hold.

**A restart limit holds an app that keeps failing.** Under `[container]`,
`restart = { attempts = 3, within = "10m" }` declares one, both values the app's own: `attempts`
from 1 to 100, `within` written as `every` is, both or neither, and an app without it is restarted
however often it fails. Docker's
`unless-stopped` policy still restarts the app, so it comes back while host is down. host watches
Docker's events for the container ending on its own -- a `die` no `kill` asked for -- with a code
other than 0, and records each as an `exit` in the app's history, which is how the count outlives
host's own restart and how the console shows the crashes. Once the last `attempts` of them since
the app was last deployed, started or restarted all fall within `within`, host holds the app and
stops it, recording a stop that says `held: 3 failing exits within 10m`. dockerd may start it once
more before host's stop lands; the hold stands either way and ends as any other does, with a start,
a redeploy or a deploy by hand. An exit while host is down is not seen, and one while the app is
held is not counted.

**A container goes with its anonymous volumes.** Every removal, host's and keeper's, asks Docker for
`v`, which takes the volumes an image's `VOLUME` made for that container and nothing else: a named
volume and the bind of the app's directory stay. Docker gives each new container fresh ones, so
nothing in them would ever be read again; what an app keeps belongs in its directory.

### An image is kept while something could run it

**What an image is kept for is decided in host's background, and is only read.** Once a minute, or
at once when asked, host scans the images and says why each stays: what an
app runs, what it would go back to on a rollback, what any container is made from -- whoever started
it -- and host's own, which keeper keeps and collects. The scan is held in memory, so asking answers at
once; Docker's measure of its images on disk, the slow part, is taken outside the deploy
lock.

**An image nothing needs is flagged, and removed an hour after.** The moment it was first found
collectable is kept in `images.db`, so a restart does not start the hour again; one that is needed
again before its hour is up -- the target of a rollback, a container started from it -- is
unflagged. A dangling image a newer build left, an image of an app no longer deployed, an upstream
image a compose file once pulled: each goes on its own.

**What is asked of the images is a task in a queue, never a request that waits.** Removing one
now, or collecting all of them now, is answered with the queued task; host's background does them
in order and then scans again. Every task and every sweep takes the deploy
lock, so an image a deploy has loaded and not yet started is never taken for one nothing needs.

A deploy still collects on its own what it made unneeded, as it always has.

## Trying host on this machine

**A node is a privileged `docker:dind` container with a btrfs file mounted at `/data`.** host needs
btrfs for every app's directory and Docker Desktop has none it will share: a path inside its VM is
refused as a bind source, and one under `/var` is taken for the Mac's `/private/var`. Inside a dind
container the paths are Linux's own, so host binds what it would bind on the machine:

1. Format a file as btrfs in a throwaway container, into a named volume: `truncate -s 4G`, then
   `mkfs.btrfs`.
2. Start `docker:dind` privileged with that volume and host's port published, and inside it
   `mount -o loop` the file at `/data`.
3. `docker exec -i ... docker load` host's and the apps' archives from `mise run image`, and start
   host inside it as the compose file does, with a token of its own; Caddy's absence is logged and
   ignored.
4. Deploy through its API as `mise run host deploy` would.

A copy of the machine's four databases, read over SSH, gives it the real apps and history to read; `docker cp` cannot see into the btrfs mount, so they go in through `docker exec -i`.
