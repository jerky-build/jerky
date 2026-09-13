# Changelog

Notable user-visible changes. Anything that alters what jerky prints, writes,
or refuses belongs here; internal refactors do not.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- The lockfile is now read back, not only written. An importer whose recorded
  specifiers still match its `package.json` is reused verbatim; one that does
  not is re-resolved. Staleness is per importer, so editing `apps/web` does not
  invalidate what the root already resolved, and a repeated install of an
  unchanged workspace reaches the registry not at all
  ([#47](https://github.com/jerky-build/jerky/issues/47)).
- A lockfile entry's integrity hash is authoritative. If the registry later
  reports a different hash for a version the lockfile already pins, the install
  stops rather than proceeding — this is the trust-on-first-use anchor spec 1
  explicitly went without, and it is what makes the lockfile a security
  artifact rather than a cache. The comparison happens before the store is
  consulted: a store hit proves only that the bytes match their *own* hash,
  which says nothing about whether that hash is the one the lockfile pinned.
  A republished tarball whose bytes another project already placed in the
  machine-global store is exactly that case.
- `jerky install` now resolves the whole workspace in one walk and links every
  importer, rather than installing a single package into a single project.
  Members come from the root `package.json`'s `workspaces` field, which is
  what npm and yarn already read, so an existing monorepo needs no new file
  and no migration step ([#46](https://github.com/jerky-build/jerky/issues/46)).
- `jerky install <pkg>` operates on the importer you are standing in, found by
  walking up for the workspace root and then taking the nearest enclosing
  member. The command now reports which importer it installed into.
- Dependencies declared with the `workspace:` protocol — `workspace:*`,
  `workspace:^1.0.0` — resolve to the member of that name and are linked
  straight at its directory. The registry is never asked. Naming a package
  that is not in the repo is an error rather than a fall back to the registry,
  because it is a typo.
- `jerky-lock.json` is written at the workspace root, keyed by importer. There
  is one lockfile per workspace, not one per project
  ([#47](https://github.com/jerky-build/jerky/issues/47)).

### Changed

- Two importers wanting the same version now share one store entry and one
  directory in the virtual store, which is why the store lives at the
  workspace root. Importers wanting different versions each get their own;
  agreement is deliberately not forced.
- A missing package is now reported through resolution rather than directly
  from the registry, since version selection is what asks.

### Removed

- Nothing.

### Fixed

- Symlink targets are computed from where the link and its target diverge
  instead of assuming one shape, so links from nested importers and links
  between packages inside the virtual store now resolve. Previously only a
  link in a project's own top-level `node_modules` was correct
  ([#45](https://github.com/jerky-build/jerky/issues/45)).

### Notes

- **`jerky install lodash` still records `"4.17.21"`, not `"^4.17.21"`.** The
  resolver can honour a range now, which is what spec 1 was waiting for, but
  the default is a pin rather than a caret: a caret is standing permission for
  some later install to choose a version nobody asked for, and it is exercised
  on whichever machine happens to re-resolve first. Widening a pin later is an
  edit; discovering that a dependency already drifted is not.
- A range you *ask* for is recorded as you wrote it: `jerky install
  lodash@^4.0.0` puts `"^4.0.0"` in `package.json`, not the version it selected
  today. Every range form npm accepts counts — `~4.17.0`, `4.x`, `>=4 <5`, a
  bare `4` — because the test is whether the request parses as a range at all,
  not which operator it used. The pin is the default for a request that named no version, not an
  override of one that did. A dist-tag still pins — `jerky install lodash@latest`
  records the version `latest` meant, since a tag in a manifest is a moving
  pointer rather than a constraint.
- Lockfile entries no importer can reach are dropped on write. A full
  resolution only ever produced the reachable set, so this keeps that property
  true under partial reuse rather than deciding lockfile pruning as a policy;
  the broader question in the resolver spec's §12 stays open.

### Errors

- Running `jerky install <pkg>` from a directory inside the workspace that
  belongs to no member is an error naming the members, rather than a silent
  install into the root. This applies only where the workspace has more than
  one member: ambiguity needs alternatives to exist.
