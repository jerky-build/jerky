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
- `jerky install --production` installs `dependencies` only, across every
  importer, and **requires a lockfile that already matches every manifest**.
  This is the CI form — jerky's `npm ci` — and it gives the frozen-lockfile
  story without a third flag to design. Requiring the match is what earns the
  read-only property rather than enforcing it: with every importer reusable
  there is nothing to resolve, so there is nothing to write, and the lockfile
  is left byte-identical. It performs no metadata requests at all; the only
  traffic is tarballs the store does not already hold. The match is over
  **both** sections, so editing a `devDependencies` entry without reinstalling
  fails even though no devDependency would have been linked — a mode that
  overlooked that would let CI pass on a lockfile that is genuinely out of
  date. A missing lockfile, or one that disagrees, is an error naming the
  dependency and both values, raised before anything is linked — as is a
  lockfile recording an importer the workspace no longer has, from a member
  dropped out of `workspaces` without reinstalling. Convergence is
  not selectively applied: a `node_modules` from an earlier ordinary install
  has its devDependency links *removed*. `--production` conflicts with a
  package argument and with `--save-dev`, rejected by clap at parse time
  ([#58](https://github.com/jerky-build/jerky/issues/58)).
- `jerky install --save-dev <pkg>`, or `-D <pkg>`, records the package under
  `devDependencies` rather than `dependencies`, in the workspace member you
  are standing in. It *moves* a package already declared in the other section
  rather than declaring it twice, and a section it empties by doing so is
  removed rather than left behind as `"dependencies": {}`. Without the flag the
  section is still whatever the manifest already says, so `jerky install
  lodash@4.18.0` remains a version change and never a promotion — and a
  manifest that declares the name in *both* sections keeps both, because
  settling that contradiction is not something a version change was asked to
  do. The lockfile
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

- File permissions from a package tarball are no longer applied as recorded.
  Every extracted file becomes `0o644`, or `0o755` when the archive marked it
  owner-executable, and every directory becomes `0o755`; nothing else from the
  header survives. Directories jerky creates itself are covered
  too — the ones a tarball omits, the store entry's own root, and the ones the
  linker recreates inside a project — since those took their mode from the
  process umask and a permissive umask made them world-writable.

  A package can no longer put a group- or world-writable file or directory
  into the content store, which matters more there than elsewhere because the
  store is machine-global and hard-linked into every project: one set of
  permissions is shared by all of them at once, so anyone who can rewrite that
  file, or add an entry to that directory, changes what every project imports.

  The setuid and setgid bits were already dropped before this change, by the
  tar reader rather than by jerky; they are now jerky's own guarantee and a
  test fails if that stops being true
  ([#29](https://github.com/jerky-build/jerky/issues/29))
- Packuments are fetched concurrently during resolution, up to sixteen at a
  time. The walk now proceeds a level at a time: it takes the whole frontier,
  fetches every packument that frontier will ask for at once, and then walks it
  exactly as before against a warm memo. Resolution discovers its own work, so
  unlike the tarball half this cannot be one flat work list — but a tree has
  few levels and wide ones, and express's 69 packages sit in seven, so seven
  rounds of requests replace sixty-eight. On that tree a cold install goes from
  3.00s to **1.04s**, and the re-resolution an edited `package.json` forces —
  warm store, stale lockfile, the common developer install — from 2.60s to
  **0.70s**. An install that reuses its lockfile is unaffected, because it
  asks the registry nothing ([#71](https://github.com/jerky-build/jerky/issues/71)).
- Tarballs are downloaded concurrently rather than one after another, up to
  sixteen at a time. Once the graph is settled every URL is known, so this half
  is a fixed work list with nothing to wait for — unlike resolution, which
  discovers its own work and is parallelised differently, above. On a cold store this is the
  difference between a first `jerky install` that reads as slow and one that
  does not; a repeat install is unaffected, because it already downloaded
  nothing. The cap exists so a large tree does not open a connection per
  package and get the machine rate-limited
  ([#33](https://github.com/jerky-build/jerky/issues/33)).
- A lockfile whose recorded integrity disagrees with the registry is now
  refused before *any* tarball is fetched, rather than after every package
  ahead of it in the graph had been. Serially "halfway through" at least had
  an order to it; with sixteen fetches in flight there is no ahead or behind,
  so the gate became a pass of its own. The check and its message are
  unchanged, but its *precedence* is not: a corrupt tarball on an early
  package used to be reported ahead of a locked-integrity mismatch on a later
  one, and now the mismatch always wins. That is the better answer of the two
  — a republished tarball is a claim about the lockfile, and it should not
  depend on where in the graph it landed.
- `jerky install` is now convergent rather than additive: after it runs, each
  importer's `node_modules` holds what its manifest declares and nothing else.
  A dependency you delete from a `package.json` loses its link on the next
  install, and its unpacked tree under `node_modules/.jerky` goes with it.
  Previously the link survived and still resolved, so `require` kept finding a
  package the project no longer declared
  ([#56](https://github.com/jerky-build/jerky/issues/56)).
- Only what jerky can prove it wrote is removed — a symlink pointing into this
  workspace's virtual store or at one of its members. A real directory left by
  a previous `npm install`, or a symlink into an `npm link` checkout, is left
  exactly where it is and reported as a warning naming the path. The first
  `jerky install` in a repository that has seen npm is not a destructive
  surprise. The machine-global content store under `~/.jerky/store` is never
  touched: it is shared by every project on the machine, so nothing
  project-local gets to decide one of its entries is dead.
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
