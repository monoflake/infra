# The repository

The infrastructure of the author's system, the layer everything else is deployed by: `host`, which
runs every app on a node, and `keeper`, which replaces host; `panel`, host's interface; `meter`,
which samples the node; and `caddy`, `tunnel` and `resolver`, the doors a request comes in by.
`libs/deploy` is what host and keeper share, and `libs/urls` the addresses of all of it. Why the
system is cut into this layer, the platform's and the services', is
[architecture/layers.md](architecture/layers.md).

## `apps/` is deployed, `libs/` is imported

Every app has a directory under `apps/` with its `service.toml` and, for one host runs, its
`Dockerfile`; every library one under `libs/`. A crate is listed in `Cargo.toml` by hand and a
package found by `pnpm-workspace.yaml`'s globs, so a TypeScript-only directory never breaks Cargo.

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

## Publishing

**`@monoflake/urls` is dated**, `YYYY.MDD.N`, and published by `.github/workflows/release.yml`
whenever a push changes it, as the lib repository publishes its own -- its `spec/repository.md`,
"Versions and publishing", holds the scheme and why. Here it is read as TypeScript from `src/`;
`publishConfig` points what is published at `dist/`, which tsdown builds, because a consumer's node
does not strip types inside `node_modules`. Its first version was published by hand, as `0.0.0`,
since npm attaches a trusted publisher only to a package that exists.

This repository continues the history of `canmi21/web`, which was `canmi21/lattice`, from the commit
the three repositories split at; everything before it is shared with the other two.
