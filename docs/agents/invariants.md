# Engineering invariants

Standing rules for jerky's implementation. Each exists because breaking it
causes a specific failure, so each is written with the failure rather than as a
preference. Check them against a diff rather than from memory — the last
section is how.

## Dependencies

**Nothing outside `src/range.rs` may name `js_semver`.** This is the
containment boundary for a 0.4.0 dependency and the reason a future swap is one
file. A `use js_semver::` anywhere else is a bug, not a shortcut.

**Prereleases are excluded unless the range mentions one.** This is npm's rule,
and the exact place Cargo's semver crate diverges silently.

## Determinism

**`BTreeMap`, never `HashMap`, anywhere that reaches the lockfile.** Iteration
order is serialization order, and two machines resolving the same tree must
produce byte-identical files. Sorted-by-construction is how that is guaranteed
rather than remembered. Internal caches that never reach disk may use a
`HashMap`.

## Workspaces

**A workspace of one goes through the same code path as a workspace of many.**
A `package.json` with no `workspaces` field is a workspace with a single
importer keyed `.`. There is no single-project branch to keep working, because
the common case is the degenerate case of the general one. A test that only
ever exercises one importer is not exercising this.

**Only `main.rs` locates the workspace root.** It is the one place allowed to
read the current directory, so walking up for the root manifest happens there
and every path below it is a parameter. This is what keeps the workspace tests
free of a real filesystem layout.

**Symlink targets are computed from the importer's depth, never hardcoded.** A
link in the root's `node_modules` needs no `../`; one in
`packages/ui/node_modules` needs three to reach the root's virtual store. Any
literal `../` outside the function that computes depth is a bug waiting for a
nested importer.

## Resolution

**Every importer's `devDependencies` are followed; no registry package's ever
are.** The rule as usually phrased — "only the root project's" — does not
survive workspaces: in a monorepo every member is a first-party project, not
just the one keyed `.`. The second half is a correctness requirement rather
than an optimisation, because following a dependency's dev dependencies pulls
in most of the registry, and it is enforced by `VersionMetadata` having no
field to read them from rather than by the resolver remembering. So the
resolver stays kind-blind: a devDependency resolves exactly as a dependency
does, and `Kind` is carried only so the lockfile can record which section
asked.

**The resolver performs no I/O beyond the `RegistryClient`.** No filesystem, no
`$HOME`, no cwd. This is what makes it testable with no filesystem at all.

## Untrusted input

**Paths that arrive from outside are validated before they are used.** Importer
keys come from a lockfile that may have been hand-edited and member patterns
come from a manifest; both can name a directory outside the workspace, and
neither may. `ImporterPath` owns this question — a second rule elsewhere would
have to be kept in agreement with it forever.

**A tar header chooses the bytes, never the permissions.** Exactly one bit of
an entry's recorded mode reaches disk — owner-execute, which marks a CLI entry
point — and everything else is replaced: files become `0o644` or `0o755`,
directories `0o755`. The reason to replace rather than mask is the store. It is
machine-global and hard-linked into every project, so an entry carries one set
of permissions shared by all of them at once; a group- or world-writable file
there is writable in every project simultaneously, and rewriting it changes
what all of them import.

Two things about this are easy to get wrong, and both were got wrong once:

- **Whether an entry is a directory is read from the filesystem, not from the
  type flag.** `tar` honours an old BSD rule that makes a non-ustar entry whose
  name ends in `/` a directory while its flag still says `Regular`. Trusting
  the flag puts that directory at `0o644`, which cannot be entered.
- **Directories jerky creates itself are covered too, and `src/directory.rs`
  is where that is decided.** `create_dir_all` takes its mode from the process
  umask, so the parents `extract` fills in, the staging directory the store
  renames into place, the tree the linker recreates inside a project and the
  levels of `~/.jerky/cache` are all `0o777` under a permissive umask unless
  set explicitly. Normalising only what a tarball named leaves the file rule
  intact and this open, and a writable directory in the store lets another user
  add entries to a package every project imports. The rule had four
  implementations once, kept in agreement by a grep; it now has one module, and
  `archive`, `staging`, `linker` and `metadata_cache` reach it through
  `directory::create` and `directory::create_all`. It returns plain
  `io::Error`s and each caller maps them into its own type, which is what the
  three incompatible error types had previously made look impossible.

  It sets the mode only on levels it actually created. A directory that was
  already there belongs to whoever made it, and re-asserting a mode on it is
  both a decision jerky has no business making and — in the extractor, once
  per ancestor per entry — several hundred thousand syscalls on a large
  package that change nothing.

The setuid and setgid bits are dropped by `tar::Entry::unpack` before any of
this runs, since `preserve_permissions` defaults to false. That is the tar
crate's behaviour and not jerky's, so it is pinned by a test rather than relied
upon — a crate bump would otherwise reopen it in silence.

## Writing to disk

**Nothing is recorded that is not already true on disk.** The manifest and the
lockfile are both written after every package is linked.

**Nothing stays on disk that is no longer recorded.** The converse, and the
half that makes the first one more than permission to leave things lying
around. Every install converges each importer's `node_modules` and prunes the
virtual store, so a dependency dropped from a `package.json` loses its link
and its unpacked tree. Without it the link still resolves, so `require` goes
on finding a package the project no longer declares and the tree quietly
disagrees with the manifest — the exact drift a lockfile that is pruned on
every write exists to prevent, applied to disk.

**Convergence removes only what it can prove jerky wrote.** A symlink whose
target, normalized lexically, lands in the virtual store or on a member. A real
directory a previous `npm install` left, or a link into someone's `npm link`
checkout, is reported and kept: the failure on that side is deleting a user's
files, which is worse than leaving some. Lexical rather than `canonicalize`,
which fails on a dangling link and would make a broken link of jerky's own
making permanent. The one non-symlink it removes is a scope directory it
emptied itself, which it removes only because it has just taken the last
package out — an `@foo` that was already empty on arrival is someone else's and
stays.

The virtual store under `node_modules/.jerky` is the deliberate exemption:
everything in it was written by `populate_virtual_store`, so ownership there is
settled by location and the pruner removes whatever the graph no longer names
without asking about link targets. The rule is about an importer's
`node_modules`, where jerky and other tools share a directory.

## Checking them

Every change ends with all three of these passing:

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
```

Then, against the diff:

- `grep -rn 'js_semver' src/ tests/` returns `src/range.rs` only.
- No `HashMap` on any path that reaches the lockfile.
- No module below `main.rs` reads the cwd or `$HOME`.
- No literal `../` outside the linker's depth calculation.
- `grep -rn 'devDependencies' src/` finds the name *read* in exactly two
  modules: `manifest.rs`, which reads a member's sections, and
  `lockfile.rs`, where the on-disk block is named by a `serde(rename)` beside
  the existing `lockfileVersion`. Every other hit must be **prose or a test
  fixture** — a comment saying why a dependency's are never followed, or a
  fixture manifest that happens to declare some. The check is on what the
  *code* reads, so count the reads and ignore the rest; do not try to keep a
  list of permitted files, which goes stale the first time someone writes a
  comment. A third place that reads the name, and a `VersionMetadata` field
  for it above all, is the bug this rule exists to prevent.
- At least one test exercises more than one importer. A suite that only ever
  sees `.` is not testing workspaces.
- `grep -rn 'fs::create_dir' src/` finds the call *outside* a `#[cfg(test)]`
  module in `src/directory.rs` only. Everything else must be a test building a
  layout of its own. A second production site is the bug this rule exists to
  prevent: it is a directory reaching the store or a project at whatever the
  umask allowed, and the reason the rule needs one owner rather than a grep
  over four copies. Directories aside, no `unpack` writes a path that reaches
  the store or a project without an explicit mode set after it. Run the suite
  under `umask 0` as well as the default — a strict umask hides every
  directory-mode hole, so the usual run proves nothing about them.
- No `#[cfg(windows)]` or `#[cfg(not(unix))]`. jerky targets WSL, Linux and
  macOS; a fallback for a platform nothing runs on is a second definition to
  keep in agreement with the first, for nobody.
- Any user-visible behaviour change is noted in `CHANGELOG.md`, under
  `## [Unreleased]`.
