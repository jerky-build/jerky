# Changelog

Notable user-visible changes. Anything that alters what jerky prints, writes,
or refuses belongs here; internal refactors do not.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- `jerky install` with no package installs what the workspace declares. This is
  the command you run after cloning a repo, and until now there was no way to
  say it: `spec` was a required argument, so jerky could add a dependency but
  not reproduce one. Every importer is covered, whatever directory you run it
  from, and with a lockfile that already matches the manifests it reaches the
  registry not at all. It reports what it did as `installed 12 packages across
  3 importers` — packages rather than links, because two importers on one
  version share a store entry and counting links would make the same install
  read differently in a monorepo
  ([#19](https://github.com/jerky-build/jerky/issues/19)).
- A bare `jerky install` works from a directory that belongs to no workspace
  member, such as `tools/scripts`, where `jerky install <pkg>` is still an
  error. Ambiguity needs alternatives, and a command that acts on every
  importer has nothing to guess.
- `devDependencies` are now installed. Every workspace member's are followed,
  not only the root project's — in a monorepo each member is a first-party
  project, and a `packages/ui` declaring nothing but devDependencies previously
  installed nothing at all. A dependency's own dev dependencies remain
  unreachable, as they must: following them pulls in most of the registry.
  The lockfile records which section asked, in a `devDependencies` block beside
  `dependencies` for each importer, so moving a package between the two shows
  up as a real diff and reinstalls rather than reading as no change. A project
  with no devDependencies gets no new block. `lockfileVersion` stays `1`; the
  format is unreleased ([#21](https://github.com/jerky-build/jerky/issues/21)).
  Recording a package *into* `devDependencies` from the command line is
  `--save-dev`, below; a name appearing in both sections is resolved as a
  production dependency, which is npm's answer, rather than refused — the
  manifest may not be the user's to edit.
- `jerky install --save-dev <pkg>`, or `-D <pkg>`, records the package under
  `devDependencies` rather than `dependencies`, in the workspace member you
  are standing in. It *moves* a package already declared in the other section
  rather than declaring it twice, and a section it empties by doing so is
  removed rather than left behind as `"dependencies": {}`. Without the flag the
  section is still whatever the manifest already says, so `jerky install
  lodash@4.18.0` remains a version change and never a promotion. The lockfile
  is rewritten to agree, so the move is one diff rather than a manifest change
  the next install notices and repeats. `--save-dev` with no package is an
  error rather than a bare install that ignores the flag
  ([#57](https://github.com/jerky-build/jerky/issues/57)).
- The lockfile is now read back, not only written. An importer whose recorded
  specifiers still match its `package.json` is reused verbatim; one that does
  not is re-resolved. Staleness is per importer, so editing `apps/web` does not
  invalidate what the root already resolved. Repeating an install that named a
  version or a range — `jerky install lodash@4.17.21`, `jerky install
  lodash@^4.0.0` — then reaches the registry not at all. A bare `jerky install
  lodash` or a dist-tag still asks every time, because only the registry can
  say what `latest` means today; that is the request rather than a shortcoming
  ([#47](https://github.com/jerky-build/jerky/issues/47)).
- A lockfile entry's integrity hash is authoritative. If the registry later
  reports a different hash for a version the lockfile already pins, the install
  stops rather than proceeding — this is the trust-on-first-use anchor spec 1
  explicitly went without, and it is what makes the lockfile a security
  artifact rather than a cache. The comparison happens before the store is
  consulted: a store hit proves only that the bytes match their *own* hash,
  which says nothing about whether that hash is the one the lockfile pinned.
  A republished tarball whose bytes another project already placed in the
  machine-global store is exactly that case. Note that the check compares the
  hash algorithm as well as the digest, so an entry locked from a package's
  legacy sha1 `shasum` will report a mismatch if the registry later serves a
  sha512 `integrity` for it; the fix for now is to delete that lockfile entry
  and reinstall.
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
  resolution only ever produced the reachable set, so this keeps a property the
  file already had, which reuse would otherwise end: merging a reused
  importer's packages with a re-resolved one's accumulates entries nothing
  references. A dependency deleted from a `package.json` by hand therefore
  loses its subtree on the next install. This settles the resolver spec's §12,
  which had deferred pruning until an `uninstall` command existed to trigger
  it.

### Errors

- `jerky install <pkg>@*` is refused rather than installed. A range that rules
  nothing out has no good answer: recording `"*"` would put the widest possible
  drift permission in a manifest, and pinning instead would answer a question
  that was not asked. Every spelling is caught — `x`, `X`, `*.*.*`, `>=0.0.0` —
  because the check is on what the range admits rather than on how it was
  typed. Bounded ranges are untouched: `^0.0.0`, `0.x` and `>=1.0.0` each rule
  something out and each still install.
- Running `jerky install <pkg>` from a directory inside the workspace that
  belongs to no member is an error naming the members, rather than a silent
  install into the root. This applies only where the workspace has more than
  one member: ambiguity needs alternatives to exist.
