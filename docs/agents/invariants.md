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

**Never follow a dependency's `devDependencies`.** Only the root project's, and
that is spec 3. This is a correctness requirement rather than an optimisation:
following them pulls in most of the registry.

**The resolver performs no I/O beyond the `RegistryClient`.** No filesystem, no
`$HOME`, no cwd. This is what makes it testable with no filesystem at all.

## Untrusted input

**Paths that arrive from outside are validated before they are used.** Importer
keys come from a lockfile that may have been hand-edited and member patterns
come from a manifest; both can name a directory outside the workspace, and
neither may. `ImporterPath` owns this question — a second rule elsewhere would
have to be kept in agreement with it forever.

## Writing to disk

**Nothing is recorded that is not already true on disk.** The manifest and the
lockfile are both written after every package is linked.

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
- `devDependencies` is read nowhere, and `VersionMetadata` has no field for it.
- At least one test exercises more than one importer. A suite that only ever
  sees `.` is not testing workspaces.
- Any user-visible behaviour change is noted in `CHANGELOG.md`, under
  `## [Unreleased]`.
