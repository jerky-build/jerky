# Workspaces — Design

**Date:** 2026-09-10
**Status:** Approved
**Issues:** #10 (this spec); reshapes #11, interacts with #12, unblocks #13/#18
**Amends:** `docs/specs/2026-09-08-resolver-and-lockfile-design.md` §8
**Builds on:** `docs/specs/2026-08-24-package-manager-design.md`

## 1. Context, and a gap worth naming

jerky's first design opened with what it is:

> jerky is a Rust CLI that fuses two products: an npm-compatible package
> manager and a **monorepo** build/task orchestrator.

That sentence is the only time the word appears in roughly a thousand lines of
design. "Workspace" appears zero times. The
ambition was stated and then partitioned away, because the same paragraph goes
on to say the spec covers "only the first slice of the package-manager half".

The partition was the mistake. It treats *package manager* and *orchestrator*
as two halves to build in sequence, but **workspaces are not in either half —
they are a property of the thing being managed.** A package manager for a
monorepo is not a single-project package manager with a task runner bolted on.
The multiplicity reaches into manifest loading, the lockfile format, and the
`node_modules` layout.

By now the single-project assumption is load-bearing in code:

```
init(project_dir)                     one package.json
install(project_dir, …)               one project
Manifest::load(project_dir)           one manifest
lockfile::save(graph, project_dir)    one lockfile
root: BTreeMap<String, String>        one set of declared ranges
```

This spec settles the question before a released lockfile format makes it
expensive. jerky manages **every project in the monorepo from the root**.

## 2. Settled decisions

**One workspace, one lockfile, at the root.** npm and pnpm both landed here and
the reason is dependency resolution, not tidiness: a shared lockfile is what
lets two projects that need the same package agree on one version, and what
makes a single `jerky install` at the root reproduce the whole repo. Per-project
lockfiles would resolve each project in ignorance of its siblings.

**Importers are keyed by workspace-relative directory.** This is pnpm's shape,
verified against real lockfiles — vitest has 39 importers keyed `.`, `docs`,
`examples/basic`, …; vue's core has 17.

A directory key is deliberately **not** a package identifier, and the
distinction matters. An importer is a *consumer*: a thing that declares
dependencies and receives a `node_modules`. `name@version` identifies a
*resolved artifact*: an immutable thing fetched from a registry. A local
package is usually both, but as a consumer its identity is its location,
because location is where `node_modules` goes. Rename the package and it is
still the same importer.

A single-project repo is the degenerate case: one importer, keyed `.`.

**Each importer declares a specifier and a resolution.** Also pnpm's shape:

```yaml
'@vue/reactivity':
  specifier: workspace:*
  version: link:../../packages/reactivity
```

The pair does double duty. `specifier` is what the manifest asked for, which is
what makes staleness detectable. `version` is what that resolved to — a
concrete version for a registry package, or `link:<path>` for a local one. So
local and registry dependencies need no separate mechanism, and the top-level
linker can read the resolved identity instead of re-deriving it.

**Members are discovered from `package.json`'s `workspaces` field.** npm and
yarn's mechanism, so an existing monorepo is a jerky workspace with no new file.
§4 works through what that implies.

**One virtual store, at the workspace root.** Every importer's `node_modules`
holds symlinks into `<root>/node_modules/.jerky/`. Two projects depending on the
same version share one hard-linked copy, which is the whole point of the store
and would be lost with per-project stores.

**Local packages are linked, never fetched.** A dependency naming a workspace
member resolves to a relative symlink at that member's directory. It has no
tarball and no integrity hash, because there is nothing to verify — the bytes
are in the repo.

## 3. Scope

### In scope

- Workspace discovery: which directories are members
- One root `jerky-lock.json` with an `importers` map
- Per-importer `node_modules`, over one shared virtual store
- Local (`workspace:`) dependency resolution and linking
- `jerky install` operating on the importer you are standing in
- `jerky install` at the root installing every importer

### Explicitly out of scope

| Deferred | Why |
|---|---|
| `--filter <pkg>` selection | Needs a selector language; the cwd covers the common case |
| Publishing and version bumping | A separate product surface |
| Per-importer overrides / resolutions | Wants a design of its own |
| Task orchestration across the graph | #13, #18 — this spec is what unblocks them |
| Nested workspaces | Rare, and pnpm does not support them either |

## 4. Discovery

**Members come from the `workspaces` field of the root `package.json`.**

```json
{ "workspaces": ["packages/*", "apps/*"] }
```

This is what npm and yarn read, and the reason to match them is adoption: an
existing npm or yarn monorepo is a jerky workspace with no new file and no
migration step. The stated ambition is "an npm-compatible package manager", and
this is where compatibility is cheapest to keep. pnpm's own file was the
alternative, and it was rejected for exactly that reason — it would make trying
jerky on an existing repo a conversion rather than a command.

What that decision implies, spelled out so it is not re-derived per
implementation:

**Absence means a single-importer workspace.** A `package.json` with no
`workspaces` field is a workspace of one, keyed `.`. Every mechanism in this
spec applies unchanged, so there is no separate single-project code path to
keep working — the common case is the degenerate case, which is the property
worth having.

**Patterns are globs, matched against directories, and each match must contain
a `package.json`.** A glob matching a directory without one is skipped rather
than an error: `packages/*` in a repo with a stray `packages/.cache` should not
fail an install. A glob matching *nothing* is worth a warning, since it is
usually a typo.

**The root is always an importer, whether or not it declares dependencies.**
Tooling-only dependencies commonly live at the root, and treating it as an
ordinary member keyed `.` avoids a special case. This resolves the first of
§10's open questions.

**Membership is a set of directories, not names.** Two members declaring the
same package name is invalid, and the error names both directories rather than
complaining about a duplicate key — a name collision is found by looking at
paths, so the paths are what the message should carry.

**#12 extends, never redeclares.** jerky's config file describes projects and
their targets, which is the same set of directories seen through a different
lens. It may add orchestrator metadata to a member; it may not decide who the
members are. Two files disagreeing about membership would be a bug generator,
and the tie-break should never need to exist.

**A member is not required to be published.** `"private": true` packages are
ordinary members. Publishability is a property of a package, not of workspace
membership.

## 5. Layout

```
<workspace root>/
  package.json                       "workspaces": ["packages/*", "apps/*"]
  jerky-lock.json                    one lockfile, every importer
  node_modules/
    .jerky/                          the virtual store, shared by all importers
      lodash@4.17.21/node_modules/lodash/
    lodash -> .jerky/lodash@4.17.21/node_modules/lodash

  packages/ui/
    package.json
    node_modules/
      lodash -> ../../../node_modules/.jerky/lodash@4.17.21/node_modules/lodash

  apps/web/
    package.json
    node_modules/
      ui -> ../../../packages/ui
      lodash -> ../../../node_modules/.jerky/lodash@4.18.0/node_modules/lodash
```

Three things this diagram is making concrete.

**Symlink depth varies by importer.** A link in the root's `node_modules` needs
no `../`; a link in `packages/ui/node_modules` needs three to climb back to the
root's virtual store. The existing linker computes a fixed target, so it must
take the importer's depth as input. This lands next to the intra-store `../`
correction already scheduled as #45, which is convenient: both are the same
class of change, and doing them together avoids touching the linker twice.

**A local dependency is a symlink straight at the source directory** —
`ui -> ../../../packages/ui` — not into the virtual store. There is no store
entry because there is no tarball.

**Importers can disagree.** `packages/ui` uses lodash 4.17.21 and `apps/web`
uses 4.18.0, and both exist in the store. This is the same property that made
the resolver's conflict case free, now applied across projects rather than
within a tree.

## 6. The lockfile, revised

`root` becomes `importers`. Spec 2's `root` block was correct in intent —
record what was asked so staleness is detectable — but singular, and it stored
only the specifier.

```json
{
  "lockfileVersion": 1,
  "importers": {
    ".": {
      "dependencies": {
        "lodash": { "specifier": "^4.17.0", "version": "4.17.21" }
      }
    },
    "apps/web": {
      "dependencies": {
        "ui":     { "specifier": "workspace:*", "version": "link:../../packages/ui" },
        "lodash": { "specifier": "^4.18.0",     "version": "4.18.0" }
      }
    }
  },
  "packages": {
    "lodash@4.17.21": { "version": "4.17.21", "resolved": "…", "integrity": "…" },
    "lodash@4.18.0":  { "version": "4.18.0",  "resolved": "…", "integrity": "…" }
  }
}
```

`packages` is unchanged — flat, keyed `name@version`, shared by every importer.
Everything spec 2 established about it holds: deterministic ordering, minimal
diff noise, integrity per package, a version gate, and the validation that
refuses contradictory keys and dangling edges.

**Staleness becomes per-importer.** An importer whose recorded specifiers no
longer match its manifest is stale; the rest of the lockfile is still good. A
single `root` block could only be wholly current or wholly stale.

**This also closes a gap review found in spec 2.** With only specifiers
recorded, top-level linking had to re-match each declared range against the
loaded versions to discover what it resolved to. The `version` half makes that
a lookup.

**And it handles aliases, which are not hypothetical.** pnpm's own lockfile
contains `execa: { specifier: "npm:safe-execa@0.3.0", version: "0.3.0" }` — a
dependency whose local name differs from the package it resolves to. Spec 2's
review flagged this as latent and unreachable; it is neither. The
specifier/version pair records it naturally, with the importer's key being the
local name.

## 7. Commands

`jerky install <pkg>` operates on **the importer you are standing in**, which
is how npm and pnpm both behave and needs no new flag. The workspace root is
found by walking up for a `package.json` declaring `workspaces`; the importer is
the nearest enclosing member.

`jerky install` with no arguments at the root installs every importer. That is
spec 3's bare install (#19), now with a workspace meaning: the command someone
runs after cloning a monorepo.

`jerky init` in a subdirectory does not implicitly register the new package as a
member. Editing `workspaces` is the user's decision, and silently rewriting the
root manifest from a subdirectory would be surprising.

## 8. Resolution across importers

The resolver's shape is unchanged. It gains a wider set of roots: instead of one
importer's ranges, it takes every importer's, and each resolved node records
which importers wanted it.

Local packages short-circuit resolution. A specifier of `workspace:*` or
`workspace:^1.0.0` resolves to the member of that name without touching the
registry, and errors if no member has that name — a `workspace:` protocol
naming a package that does not exist in the repo is a typo, not a fallback to
the registry.

**Deliberately not deduplicating across importers.** If two importers want
incompatible versions, both are installed and each links to its own, exactly as
within a single tree. Forcing agreement is a policy some monorepos want, but it
belongs behind a flag rather than in the resolver.

## 9. Effect on existing work

**The lockfile reshape has landed.** PR #38 was held rather than merged and
superseded by #43: its determinism, diff-noise, version-gate and validation work
stands, but the top-level shape moved from `root` to `importers`. Doing it then
cost a find-and-replace; doing it after a released format would have cost a
migration and a compatibility shim. Member discovery followed in #44.

**Importer-relative symlink depth is #45**, grouped there with the intra-store
`../` correction because they are the same class of change. #46 installs across
the workspace on top of it, and #47 makes the lockfile something jerky reads
back rather than only writes.

### Issues

- **#10** becomes this spec. Its one line — "pick up all projects in the
  workspace and track them" — is exactly the `importers` map.
- **#11** keeps its four requirements; only the top-level block changes.
- **#12** must not redeclare membership. It extends the same directories with
  orchestrator metadata.
- **#13, #18** need to know the project graph to run tasks in dependency order.
  That graph is the `importers` map plus the `link:` edges between them, so this
  spec is their prerequisite.
- **#19** gains its workspace meaning: bare install covers every importer.

### Filed since

- **#40** — `--filter` selection, deferred from §3.
- **#41** — enforcing one version across importers, if it turns out to be
  wanted.

## 10. Open questions

**How should a member outside the workspace root be handled?** npm allows a
`workspaces` glob to escape the root with `../`. It is rare and arguably a
misconfiguration, but silently including a directory outside the repo has the
same shape as the tar-slip class spec 1 guards against, so it wants a decision
rather than a default.

**Should `jerky install <pkg>` in a non-member subdirectory be an error?**
Standing in `packages/ui/src` clearly means the `packages/ui` importer, but
standing in a directory that belongs to no member is ambiguous — it could mean
the root, or it could mean the user is lost. Erroring is probably kinder than
guessing.
