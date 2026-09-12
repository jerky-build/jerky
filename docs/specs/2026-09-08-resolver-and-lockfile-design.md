# Transitive Resolution and the Lockfile — Design

**Date:** 2026-09-08
**Status:** Approved
**Scope:** Spec 2 of 3 for jerky's package-manager spine
**Issues:** #6, #11 (this spec); enables #19, #21, #22
**Builds on:** `docs/specs/2026-08-24-package-manager-design.md`

## 1. Context

Spec 1 built the walking skeleton: registry client, integrity verification,
hostile-input extraction, the content-addressed store, the virtual store
linker, and `jerky install <pkg>` for a package with no dependencies of its
own. All of it is on `main` and installs real packages from the real registry.

Spec 2 makes jerky useful on a real project. `jerky install express` has to
walk express's own dependencies, resolve the ranges they declare, and write
down what it resolved so the next machine gets the same answer.

Two issues cover this and they are deliberately one design, not two. #11 states
the reason: a lockfile's format *is* the serialization of the resolver's output
type. Designing it against spec 1's case — one entry, zero edges — would mean
designing it against the input that exercises none of its structure.

## 2. Settled decisions

**The semver crate is `js-semver`.** Answered by differential testing against
npm's own `semver` 7.8.5 over 531 ranges: `js-semver` 531/531, `nodejs-semver`
515/531, Cargo's `semver` 431/531. Full data in
`docs/research/2026-09-08-npm-semver-crate-selection.md`.

The control matters for what it says about the failure mode, not just the
score. Of Cargo `semver`'s 100 failures, 55 are refusals to parse and **45 are
silent wrong answers** on prerelease ranges. A resolver built on it would not
fail loudly; it would resolve a different tree than npm and the lockfile would
record that divergence as though it were correct. That is the specific
disaster this spec is designed to avoid, so it is worth naming.

`js-semver` is 0.4.0 and may break its API on a minor bump. Two mitigations,
both adopted below: it is wrapped behind jerky's own `range` module rather than
used directly by the resolver, and the conformance suite ships in the repo so
that replacing it is one test run rather than a repeat investigation.
`nodejs-semver` is the documented fallback; its only known gap is build
metadata on a partial version, and it refuses to parse rather than answering
wrongly.

**The lockfile is JSON.** Measured against YAML on a realistic 2000-package
lockfile: JSON parses in 1.8 ms against 12.5 ms, serializes in 0.6 ms against
9.2 ms, and costs no new dependency because `serde_json` with `preserve_order`
is already in the tree. YAML is about 13% smaller on disk, which is the real
tradeoff and the only axis it wins.

The deciding factor is not speed, though. A lockfile is a security artifact
that carries integrity hashes, and YAML's implicit typing is a hazard for one:
`serde_norway` emits a package literally named `no` as a bare scalar, which any
other YAML reader takes as boolean `false`. jerky's own `Manifest` already
parses into an untyped `Value` to preserve unknown fields, so the pattern that
would hit this is one jerky already uses. The mainstream Rust YAML serde crates
are also deprecated or unmaintained.

**`jerky install` now writes a caret range.** `jerky install lodash` records
`"lodash": "^4.17.21"`, matching npm and pnpm. Spec 1 pinned exactly for a
stated reason that this spec removes:

> jerky should not yet: spec 1 has no range resolution, so a caret would put a
> constraint in `package.json` that the tool cannot honour on the next install
> — the file would claim more than jerky can do. **Switch to caret in spec 2,
> when ranges actually resolve.**

This is a user-visible behaviour change and needs a changelog note. It also
means the manifest now contains ranges the resolver must handle, so the root
project is just another node with ranges, not a special case.

**The lockfile is named `jerky-lock.json`.** #11 called it `jerky.lock`, by
analogy with `Cargo.lock` and `yarn.lock`. Since the format is JSON, the
extension is worth having: editors syntax-highlight it, `jq` and every other
JSON tool work on it without being told what it is, and a reviewer opening it
in a diff gets folding and structure for free. `package-lock.json` is the
closer precedent anyway. Deciding this now is cheap; deciding it after anyone
has committed one is a migration.

**Parallel downloads are deferred.** Spec 1's design placed them here, but
spec 2 is already a transitive resolver plus a lockfile format, and both are
things worth getting right before making fast. Resolution is serial in this
spec. The `RegistryClient` trait keeps the seam open, and the honest cost is
that installing a large tree will be visibly slower than npm until the
follow-up lands. That is acceptable for a spec whose job is correctness.

## 3. Scope

### In scope

- npm range parsing and max-satisfying selection, behind jerky's own `range` type
- Abbreviated packument fetching, so a package's full version list is available
- Transitive dependency resolution: walk the graph, dedupe, handle cycles
- Virtual store wiring for nested dependencies — each package sees its own deps
- `jerky-lock.json`: deterministic, diffable, versioned, integrity-bearing
- Reading the lockfile back to skip re-resolution when it is still valid
- `jerky install <pkg>` recording a caret range

### Explicitly out of scope

| Deferred | Goes to |
|---|---|
| Bare `jerky install` from the manifest | Spec 3 (#19) |
| `devDependencies` / `--save-dev` | Spec 3 (#21) |
| Scoped packages | Spec 3 (#22) |
| `node_modules/.bin` linking | Spec 3 (#20) |
| Parallel downloads | Later slice |
| Peer and optional dependencies | Later |
| Lifecycle scripts | Later (#23) |
| Bundled dependencies | Later |
| `npm shrinkwrap` compatibility | Not planned |

**Transitive `devDependencies` are not merely out of scope, they are wrong.**
A dependency's `devDependencies` must never be followed. Only the root
project's are, and that is spec 3. #21 states this as a correctness
requirement rather than an optimisation, and it is: following them would pull
in most of the registry. The resolver in this spec reads `dependencies` only,
which makes the spec 3 change additive rather than a correction.

**Peer dependencies are absent, and that will be visible.** Real trees declare
them, and ignoring them means jerky installs trees that npm would warn about.
This is a known gap, not an oversight — resolving peers correctly interacts
with the layout and deserves its own design.

## 4. Commands

`jerky install <pkg>[@<version>]` gains transitive behaviour. The command
surface does not change; what changes is how much it installs and what it
leaves behind.

```
$ jerky install express
resolved 57 packages
added express@4.21.2
```

`package.json` gains `"express": "^4.21.2"`. `jerky-lock.json` gains 57 entries.
`node_modules/express` symlinks into the virtual store as before, and each of
the 57 packages gets its own virtual store directory with its own dependencies
linked as siblings.

## 5. Architecture

Three new modules, and one existing module gains a method.

```
src/
  range.rs        jerky's range type, wrapping js-semver
  resolver.rs     the transitive walk; produces a ResolvedGraph
  lockfile.rs     ResolvedGraph <-> jerky-lock.json
  registry.rs     + packument() for the full version list
  commands/
    install.rs    orchestration, now over a graph rather than one package
```

**Why `range` is a module and not a type alias.** It is the containment
boundary for the 0.4.0 dependency. The resolver, the lockfile, and the install
command all speak `range::Range` and `range::Version`; none of them names
`js_semver`. Swapping the crate is then one file, and the conformance suite
decides whether the swap is acceptable.

**Why the resolver produces a value rather than performing installs.** The
walk is pure given a `RegistryClient`: it fetches metadata and returns a graph.
It writes nothing to disk. That keeps it testable against the fixture registry
with no filesystem at all, and it makes the lockfile a straight serialization
of the resolver's output — which is the property #11 asked for.

## 6. Data flow and types

```rust
// range.rs — the containment boundary
pub struct Range(js_semver::Range);
pub struct Version(js_semver::Version);

impl Range {
    pub fn parse(input: &str) -> Result<Range, RangeError>;
    /// The highest published version satisfying this range.
    pub fn max_satisfying<'v>(&self, versions: &'v [Version]) -> Option<&'v Version>;
}

// registry.rs — the new method
pub struct Packument {
    pub name: String,
    /// Every published version's metadata, abbreviated form.
    pub versions: BTreeMap<String, VersionMetadata>,
    pub dist_tags: BTreeMap<String, String>,
}

pub trait RegistryClient {
    fn version_metadata(&self, name: &str, version: &str) -> Result<VersionMetadata, RegistryError>;
    fn packument(&self, name: &str) -> Result<Packument, RegistryError>;   // new
    fn fetch_tarball(&self, url: &str) -> Result<Vec<u8>, RegistryError>;
}

// resolver.rs
pub struct PackageId { pub name: String, pub version: String }   // the graph's node key

pub struct ResolvedPackage {
    pub id: PackageId,
    pub resolved: String,        // tarball URL
    pub integrity: Integrity,
    /// Edge labels: what this package calls a dependency -> which node it resolved to.
    pub dependencies: BTreeMap<String, PackageId>,
}

pub struct ResolvedGraph {
    /// The ranges the root project declared, so staleness is detectable.
    pub root: BTreeMap<String, String>,
    pub packages: BTreeMap<PackageId, ResolvedPackage>,
}
```

`BTreeMap` throughout, not `HashMap`. Iteration order is the serialization
order, and #11 requires two machines resolving the same tree to produce
byte-identical files. Sorted-by-construction is how that is guaranteed rather
than remembered.

## 7. Resolution

### The algorithm

A worklist walk over `(name, range)` pairs, memoized on the resolved
`PackageId`.

```
seed the worklist with the root manifest's (name, range) pairs
while the worklist is non-empty:
    take (name, range)
    if (name, range) was already resolved: reuse the answer      # range cache
    fetch the packument for name                                  # packument cache
    pick the highest published version satisfying range
    if none: error, unsatisfiable
    if that PackageId is already in the graph: record the edge, do not recurse
    otherwise: add the node, then push its `dependencies` onto the worklist
```

Two caches, and they do different jobs. The **packument cache** avoids
re-fetching a package's version list once per dependent — with tens of
dependents on a popular package that is the difference between one request and
tens. The **range cache** maps `(name, range)` to the `PackageId` it selected,
so identical range strings resolve once.

**Cycles terminate because recursion is gated on node novelty, not on path.**
`a → b → a` adds `a`, adds `b`, then finds `a` already present and records the
edge without recursing. npm packages do contain cycles, so this is a
correctness requirement with a test, not a theoretical concern.

### What the virtual store gives us for free

Spec 1's layout decision pays off here, and it is worth being explicit about
what did *not* need designing.

There is no conflict resolution. When two packages need incompatible versions
of the same dependency, both versions become separate nodes and each dependent
links to the one it asked for. npm's hoisting algorithm exists to fit one
shared namespace; jerky has no shared namespace to fit. The design predicted
this:

> In a virtual store both versions coexist as separate directories and each
> dependent links to the one it asked for. The conflict never arises.

There is also no placement algorithm. Every node gets
`node_modules/.jerky/<name>@<version>/node_modules/<name>/`, and its edges
become symlinks *inside* that directory. The layout is a mechanical function of
the graph.

**One linker correction this forces.** Spec 1's `symlink_dependency` writes a
target with no leading `../`, correct for links in `node_modules/` itself. Links
between packages *inside* the virtual store sit one level deeper, so they do
need the `../` — which is exactly the case pnpm uses it for. The function gains a variant for intra-store links rather than being
changed.

### Selection rules

- Highest satisfying version wins, matching npm.
- Prereleases are excluded unless the range explicitly mentions one, which is
  npm's rule and the exact place Cargo's crate diverges silently.
- Dist-tags resolve through the packument's `dist-tags`, so `express@next`
  keeps working as it did in spec 1.
- An unsatisfiable range is an error naming the package, the range, and what
  versions do exist. "No matching version" without the candidate list is the
  kind of error that sends people to a browser.

## 8. The lockfile

```json
{
  "lockfileVersion": 1,
  "root": {
    "express": "^4.21.2"
  },
  "packages": {
    "accepts@1.3.8": {
      "version": "1.3.8",
      "resolved": "https://registry.npmjs.org/accepts/-/accepts-1.3.8.tgz",
      "integrity": "sha512-PYAthTa2m2VKxuvSD3DPC/Gy+U+sOA1LAuT8mkmRuvw+NACSaeXEQ+NHcVF7rONl6qcaxV3Uuemwawk+7+SJLw==",
      "dependencies": {
        "mime-types": "2.1.35",
        "negotiator": "0.6.3"
      }
    }
  }
}
```

Each of #11's four requirements maps to a concrete property:

**Deterministic key ordering** — `BTreeMap` everywhere, so ordering is
structural rather than a step someone can forget. A test resolves the same tree
twice and asserts the bytes are identical.

**Minimal diff noise** — packages are keyed by `name@version` at one level, so
adding a dependency appends one block and touches nothing else. This is why the
format is flat rather than a nested tree mirroring the graph: in a nested tree,
a change deep in the graph reindents everything under it.

**A format version field from day one** — `lockfileVersion`. Reading a
lockfile with an unknown version is a clear error telling the user to upgrade
jerky, never a best-effort parse. Real projects commit lockfiles; a format that
changes later without this has no migration path.

**Integrity per package** — the resolved concrete version, the tarball URL, the
integrity hash, and the dependency edges. The hash is what makes this a
security artifact rather than a cache, and it is what closes the gap spec 1
accepted:

> without a lockfile there is no trust-on-first-use anchor, so a republished or
> tampered tarball for an already-pinned version would go unnoticed.

When a lockfile entry exists, its integrity hash is authoritative. If the
registry later reports a different hash for the same version, that is an error
and the install stops. This is the whole point, so it gets a test that flips
the hash and asserts the refusal.

**The `root` block** records the ranges the manifest declared. It is what makes
staleness detectable: if `package.json` asks for something `root` does not
match, the lockfile is stale and resolution re-runs. Without it, jerky could
not tell an up-to-date lockfile from one written before someone hand-edited a
dependency.

> **Superseded 2026-09-10.** `root` became `importers`, keyed by
> workspace-relative directory, with each dependency recording both a
> `specifier` and the `version` it resolved to. jerky manages every project in
> a monorepo from the root, so one block of ranges could describe only one of
> them. See `docs/specs/2026-09-10-workspace-design.md` §6. The
> rest of this section — flat `packages` keyed `name@version`, deterministic
> ordering, minimal diff noise, the version gate, integrity per package — is
> unchanged.

### Reuse, not just writing

Spec 1's design noted the lockfile would be "written and never consumed" in
spec 1, and that untestability was part of why it was deferred. So spec 2 both
writes and reads it: when the lockfile is present and its `root` matches the
manifest, resolution is skipped for the parts it already covers.

This matters beyond speed — a format that is only ever written is a format
whose round trip is untested. Bare `jerky install` (#19) remains spec 3; what
this spec adds is reuse during `jerky install <pkg>`.

## 9. Error handling

New failure modes, each with a named error rather than a wrapped string:

- **Unsatisfiable range** — names the package, the range, and the available
  versions.
- **Packument malformed or missing** — distinct from a missing version, the
  same way spec 1 separates `PackageNotFound` from `VersionNotFound`.
- **Lockfile version unknown** — never a best-effort parse.
- **Lockfile integrity mismatch** — the registry now reports a different hash
  than the lockfile records. This is the tamper signal and reads as one.
- **Range unparseable** — from `range`, so `js-semver`'s error type does not
  leak into the resolver's signature.

The ordering discipline from spec 1 extends unchanged: nothing is recorded that
is not already true on disk. The manifest and the lockfile are both written
after every package is linked, so an interrupted install leaves an
installed-but-unrecorded tree rather than a lockfile describing a tree that
does not exist.

## 10. Testing

The fixture registry does the heavy lifting again, gaining packument support so
whole dependency trees can be declared inline.

Trees each test must cover:

- **A diamond.** `a → b, a → c, b → d, c → d` at the same version — `d`
  resolves once and appears once in the graph.
- **A conflict.** `b → d@^1`, `c → d@^2` — both versions exist as separate
  nodes, each dependent links to its own. This is the case that would be hard
  under hoisting and is nearly free here, so it is worth proving rather than
  assuming.
- **A cycle.** `a → b → a` terminates and produces two nodes.
- **A deep chain**, to catch anything accidentally recursive that would blow
  the stack on a real tree.
- **An unsatisfiable range**, asserting the error names the available versions.

Lockfile tests:

- Resolving the same tree twice produces byte-identical files.
- Adding one dependency to an existing lockfile changes only its own block.
- An unknown `lockfileVersion` is refused.
- A lockfile whose integrity hash disagrees with the registry stops the install.
- Round trip: write, read back, and get the same `ResolvedGraph`.

**The semver conformance suite ships in the repo**, generated from the research
harness. It is the guard on the 0.4.0 dependency, and its value is that it
answers "is this replacement acceptable" in one run.

An end-to-end test installs a real multi-dependency package from the live
registry, `#[ignore]`d like spec 1's live test so it runs on a schedule rather
than per-PR.

## 11. Roadmap and issue remapping

**Spec 3 — everyday use.** Bare `jerky install` (#19), `devDependencies` (#21),
scoped packages (#22), and `.bin` linking (#20). All four get materially
simpler once a resolver and a lockfile exist; #19 in particular is close to
trivial once bare install can read the lockfile this spec writes.

### Effect on existing issues

- **#6** becomes this spec's `range` module and resolver. Its "open question,
  needs a spike first" is answered and can be struck.
- **#11** becomes this spec's lockfile. Its four requirements map to the four
  properties in §8.
- **#19** gains a concrete prerequisite: it is bare `jerky install` reading the
  lockfile format defined here.
- **#24** (`package.json` indentation) becomes more visible, because installs
  now touch the manifest more often and write ranges rather than pins. Still
  not blocking.

### Issues that need filing

- **Parallel downloads**, deferred from this spec. Should name the bounded
  thread pool and the fact that `RegistryClient` is the seam.
- **Peer dependency support**, which this spec makes conspicuous by ignoring.

## 12. Open questions

**When does the lockfile get pruned?** Removing a dependency from
`package.json` should eventually remove its subtree from the lockfile, but
jerky has no `uninstall` command yet, so there is no operation that would
trigger it. Deferred until one exists.

*(The lockfile filename was an open question here and is now settled — see
§2.)*
