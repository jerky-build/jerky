# Optional Dependencies — Design

**Date:** 2026-09-16
**Status:** Approved
**Issues:** #107 (this spec), spun off by
`docs/specs/2026-09-15-peer-dependencies-design.md`
**Builds on:** `docs/specs/2026-09-08-resolver-and-lockfile-design.md`

## 1. Context: the other half of spec 2's deferral

Spec 2 deferred "peer and optional dependencies" on one line. The peer spec
took the first half and spun this off, on the grounds that the two are
different mechanisms: peers are an identity-and-constraint problem, optional
dependencies are a failure-tolerance problem. Nothing built for peers helps
here, and `VersionMetadata` still drops `optionalDependencies` deliberately so
that it could not be half-implemented in the meantime.

The question this document has to answer first is what "failure" means, because
`optionalDependencies` in 2026 is not the feature it was written to be. It was
added so that a native addon whose `node-gyp` build failed on an unsupported
machine could be skipped rather than kill the install. **jerky runs no
lifecycle scripts and builds nothing**, so that failure cannot occur here at
all. What is left in real trees is almost entirely one shape:

```json
"optionalDependencies": {
  "@esbuild/darwin-arm64": "0.21.5",
  "@esbuild/linux-x64": "0.21.5",
  ... twenty-one more
}
```

with each of those packages declaring `"os": ["darwin"], "cpu": ["arm64"]`.
That is not a failure being tolerated. It is a package saying in advance which
machines it is for, and an installer being asked to read it. esbuild, rollup,
swc, lightningcss, sharp and `@napi-rs/*` all ship this way, and `fsevents` —
the canonical optional dependency — is `"os": ["darwin"]` with no build step
jerky would run.

So this is a smaller feature than the name suggests, and most of the design
work is in refusing the larger one.

## 2. Settled decisions

**A declared platform mismatch is the only thing that skips.** A fetch error,
an integrity mismatch and an unpack failure are all fatal on an optional
dependency exactly as they are on a required one. The line is drawn between
*resolution* and *materialisation*: a platform mismatch is a fact about the
package that is known from its metadata, decided before a byte is fetched, and
recorded in the lockfile; everything else is an accident of the network, the
registry or the disk. Four arguments, because the four candidates in #107 are
genuinely not one class:

- **An integrity mismatch is never skippable.** It is the one thing the
  lockfile exists to catch. `optional: true` is the *publisher's* statement
  about whether their functionality is required; it is not the *consumer's*
  waiver on whether the bytes are who they say they are, and reading it as one
  hands anybody who can serve bad bytes for `fsevents` the power to remove
  `fsevents` from the tree with no output at all. That converts the loudest
  signal jerky has into the quietest, and it does so on exactly the packages —
  platform-specific native binaries — where the bytes matter most. jerky
  already refuses an install over `LockedIntegrityMismatch` with a sentence
  about republished or tampered tarballs; a silent skip one function along
  would contradict it.
- **A fetch error is a property of the network, not of the package.**
  Tolerating one makes the installed tree a function of whether the wifi
  blipped: one lockfile, one platform, two different `node_modules`. Every
  other decision in this project runs the other way, and there is no way to
  record "skipped because a fetch failed" in a file that is committed and
  shared — the next machine would have to reproduce the outage to reproduce the
  tree.
- **An unpack failure is one of those two, one step later.** A malformed
  tarball that nonetheless hashes correctly is a corrupt artifact, which is the
  integrity argument with the detection moved; an `ENOSPC` or a permissions
  failure is about the machine, and skipping a package because the disk is full
  produces a tree that silently differs from the one the lockfile describes.
- **An unmet `os`/`cpu` is not a failure at all.** Nothing went wrong. The
  package said which machines it is for, this is not one of them, and the
  dependent declared it optional precisely so that this could be the answer.

The cost is that jerky fails where npm shrugs, on a 404 for an optional
dependency. That cost is paid at the one moment something is genuinely wrong,
and it buys an install whose result does not depend on the weather.

**The lockfile records a skipped dependency in full, and is platform
independent.** Resolution happens for every optional dependency on every
platform; the skip is applied afterwards, when the graph is turned into a plan.
So the file records `@esbuild/win32-x64` with its tarball URL, its hash, its
`os` and its `cpu` whether or not this machine will ever link it, and a
lockfile written on a Mac installs on Linux without reading as stale. #107
names the alternative and its consequence — recording nothing makes the next
install on another platform look like a stale lockfile — and the consequence is
worse than it sounds: `Importer::matches` measures staleness in declared
specifiers, so a lockfile missing what this platform skipped would not merely
look stale, it would be re-resolved into a *different* file on each platform,
and a repository with a Mac and a Linux CI would have the two fighting over the
committed copy on every commit.

This is also what makes optionality survive a cache hit, which is the stronger
half of the argument. An install whose importers all still match resolves
nothing at all and builds its graph out of the lockfile alone. If the file did
not say which edges were declared optional, that install would have no way to
know `@esbuild/win32-x64` was skippable and would link it. The peer spec
records declared peers so a *warning* survives a cache hit; here it is the
tree.

**`reachable()` is untouched, and the skip is a second, later filter.** An
optional dependency is a real edge in the graph. Pruning the graph itself would
make the lockfile platform-dependent, since `reachable()` is what the lockfile
is written from — so the two decisions are the same decision. The filter,
`ResolvedGraph::for_platform`, is a reachability walk that declines to *enter*
a package through an optional edge when that package's declared platform does
not admit this one. Everything it does not reach is left out of the plan: the
skipped package gets no store entry and no link, and so does anything only it
led to. A package the skipped one shared with a package that is kept is
reached by the other path and stays, which is npm's rule (a node is optional
only if every path to it is) falling out of the walk rather than being stated
separately.

**A platform constraint is consulted only where a skip is available.** A
package reached by a required edge is installed whatever its `os` says, and the
field is not read. npm raises `EBADPLATFORM` here; jerky does not, for the
reason the peer spec declines to fail on an unsatisfied peer — refusing a tree
that works is not a stricter kind of correct, it is an unusable one, and `os`
is advisory metadata that publishers get wrong. It is silent rather than a
warning for the same reason: a warning that fires on a working tree is noise,
and there is nothing the user could do about it.

**One line of output, not one per package.** A skipped optional dependency is
reported as a count and the platform that did the skipping, alongside the
install summary:

```
skipped 23 optional packages unsupported on linux-x64
```

The peer spec rejects a bare summary count for unsatisfied peers, and this is
not a contradiction of it but the same reasoning on a different fact. An
unsatisfied peer is a problem the reader should fix, so it needs a name. A
platform-mismatched optional dependency is the mechanism working, and it is
*routine*: a project using esbuild, rollup and swc skips some seventy packages
on every install, forever. Seventy lines of correct behaviour per run is how a
project teaches its users to stop reading its output. The names are not lost —
they are in the lockfile, sorted and diffable, which is where an enumeration
belongs.

The count rides back on the install outcome and is printed in `main.rs`,
matching the peer warnings and the left-alone warnings. It goes to stdout
beside `installed N packages across M importers`, because it is a statement
about what the install did rather than a complaint about it.

## 3. Scope

### In scope

- `optionalDependencies`, `os` and `cpu` read off the abbreviated packument
- A platform match implementing npm's list rule, including negation
- `ResolvedGraph::for_platform`, the filter that drops what this platform skips
- The lockfile recording optional edges and declared platforms
- One summary line per install

### Explicitly out of scope

| Deferred | Why |
|---|---|
| `libc` | Cannot be answered from `std`; see below |
| An importer's own `optionalDependencies` | Needs a third `Kind`; see below |
| `--save-optional`, `--omit=optional` | Policy on top of a resolution, with #41 |
| Naming the skipped packages in the output | The lockfile already does |

**`libc` is not read.** `"libc": ["musl"]` is real and growing —
`@rollup/rollup-linux-x64-musl` and its siblings — but there is no way to
answer it from `std`, and every available answer is a guess: parsing `ldd
--version`, stat-ing `/lib/ld-musl-*`, or reading the interpreter out of the
running binary. A wrong guess here is worse than no answer in one direction and
harmless in the other. Not reading it means a musl machine installs the glibc
build alongside the musl one and the package's own loader picks; guessing wrong
means the machine installs *neither* and nothing works. An unread field is also
exactly what an absent `libc` already means today — "any libc" — so the
behaviour is consistent rather than special-cased. Filed as a follow-up.

**An importer's own `optionalDependencies` is out of scope.** A workspace
member declaring one would need `Kind` to grow a third variant, which reaches
`Importer::matches`, both on-disk importer blocks, the `--production` filter,
and the CLI's `--save-dev` surface. The population is also different in kind:
the case this feature exists for is a *library* declaring platform variants,
and every real instance of it is transitive. Filed as a follow-up.

## 4. Reading it off the registry

`VersionMetadata` gains three fields, all `#[serde(default)]`:

```rust
#[serde(default, rename = "optionalDependencies")]
pub optional_dependencies: BTreeMap<String, String>,
#[serde(default)]
pub os: Vec<String>,
#[serde(default)]
pub cpu: Vec<String>,
```

No new request. The abbreviated packument jerky already asks for carries all
three — verified against the live registry: `esbuild@0.21.5` returns
twenty-three `optionalDependencies`, `@esbuild/darwin-arm64@0.21.5` returns
`"os": ["darwin"], "cpu": ["arm64"]`, and `fsevents@2.3.3` returns
`"os": ["darwin"]`.

The comment that made `optionalDependencies` deliberately absent goes with it.
The `devDependencies` half of that comment stays exactly as it is, and so does
the invariant behind it: this spec adds no field a dev dependency could arrive
through.

**A name in both `dependencies` and `optionalDependencies` is optional**, at
the optional range. That is npm's documented rule — "entries in
optionalDependencies will override entries of the same name in dependencies" —
and it falls out of queuing the optional block second.

## 5. The platform match

`src/platform.rs`, a module of its own, holding two types that are easy to
confuse and must not be:

```rust
/// The machine.
pub struct Platform { os: String, cpu: String }

/// What a package declared about the machines it runs on.
pub struct PlatformSupport { pub os: Vec<String>, pub cpu: Vec<String> }
```

`Platform::current()` maps Rust's `std::env::consts::{OS, ARCH}` onto Node's
spelling of the same thing, since `os` and `cpu` are published against
`process.platform` and `process.arch`. The interesting entries are `macos ->
darwin`, `windows -> win32`, `x86_64 -> x64`, `aarch64 -> arm64` and `x86 ->
ia32`. A name with no mapping is passed through unchanged, which is right for
`linux`, `freebsd`, `arm` and `s390x` and is the only honest answer for
anything not listed: the value is only ever compared for equality, so an
unmapped name matches a package that names it and matches nothing else.

This is not a `#[cfg]`. `std::env::consts` is a compile-time constant read at
runtime through an ordinary `match`, so there is one definition of the mapping
and every branch of it compiles everywhere — which is what the no-`cfg(windows)`
invariant is protecting.

`PlatformSupport::admits` implements npm's rule exactly, from
`npm-install-checks`, because this is another place where the correct answer is
defined by another implementation's behaviour rather than by a written
specification:

- An entry may be negated with a leading `!`.
- Any negated entry matching the current value rejects, whatever else is in the
  list.
- Otherwise the list admits if some non-negated entry matches, **or** if every
  entry was negated.
- An empty list admits, which falls out of the previous line, and so does a
  package with no `os` at all.
- The single entry `any` admits.

The mixed case is the one worth stating: `["!win32", "darwin"]` admits darwin
and nothing else, not "everything but win32". The rule is a whitelist the
moment any positive entry appears.

## 6. The graph

`ResolvedPackage` gains two fields:

```rust
/// Which of `dependencies` were declared in `optionalDependencies`.
pub optional: BTreeSet<String>,
/// What this package declared about the machines it runs on.
pub supports: PlatformSupport,
```

**A marker set over the edge names, not a second edge map, and not a flag on
the edge value.** An optional dependency that is installed is an ordinary
dependency in every respect — it resolves the same way, links the same way,
prunes the same way, and answers a peer the same way. Optionality changes one
thing only: whether a platform mismatch at the far end is tolerated. So every
existing walker over `dependencies` — `reachable`, the peer pass, the linker's
plan — stays single-branch and stays right, and the one pass that has the
question asks it. Putting a flag on the edge value would instead put it in
front of every consumer that has nothing to ask of it.

This is the same split the lockfile already makes for importers, in the same
direction: the graph keeps one flat map, the two blocks exist only at
serialization. It is deliberately *not* the peer spec's `peers` decision, and
the difference is that resolved peers are a different relation from
dependencies, whereas an optional dependency is a dependency.

`ResolvedGraph::for_platform(&self, &Platform) -> (Self, Vec<PackageId>)`
returns the sub-graph this platform materialises and the packages it skipped.
It borrows rather than consuming, which is the one place this differs from
`reachable`: the caller needs both graphs at once, the full one for the
lockfile and the filtered one for the plan, and that is the entire point of the
split.

Two things it must do beyond dropping nodes, both of them about not leaving a
dangling link behind:

- An edge naming a dropped package is dropped from the surviving dependent.
  Without this the plan writes a symlink into a virtual store entry that was
  never created.
- A resolved peer naming a dropped package is dropped the same way, for the
  same reason.

## 7. The lockfile

`Entry` gains three fields, mirroring §4:

```rust
#[serde(rename = "optionalDependencies", default, skip_serializing_if = "BTreeMap::is_empty")]
optional_dependencies: BTreeMap<String, String>,
#[serde(default, skip_serializing_if = "Vec::is_empty")]
os: Vec<String>,
#[serde(default, skip_serializing_if = "Vec::is_empty")]
cpu: Vec<String>,
```

Every one of them is skipped when empty, so a package with no optional
dependencies and no platform constraint — which is nearly all of them — writes
exactly the bytes it writes today. The change is invisible where the feature is
absent, which is the same property the peer spec required of its key spelling.

The two dependency blocks are merged back into one map and a marker set on
load, exactly as the importer's two blocks already are. The dangling-edge check
and the identity rebuild both cover optional edges, because an optional edge is
an edge: one that names a package the file does not record is the same broken
install a required one is, and one whose target carries a peer context
contributes to its dependent's identity in the same way.

**The entry records what the package declared, never what this machine
decided.** There is no `"skipped": true`, and there is no entry-level
`"optional": true` of the kind npm writes. Both are derived state — the first
from the machine, the second from every path to the node — and a file whose job
is recording facts is the wrong place for either. The second is also what would
make the file platform-dependent through the back door, since "every path is
optional" is one graph edit away from changing.

**`LOCKFILE_VERSION` stays at `1`.** Same reason as every shape change before
it: jerky has not shipped a 1.0, so there is no committed population a version
number could tell apart. The format changes and a stale file is regenerated.

## 8. Testing

Registry-free tree-shape tests, in the `tests/resolve.rs` style, each a claim
about a decision above:

- an optional dependency this platform supports is resolved and linked like any
  other
- an unsupported optional dependency is skipped, and so is the subtree only it
  reached
- a package the skipped one shared with a kept one stays
- an unsupported package reached by a *required* edge is installed, and nothing
  is said about it
- a name in both blocks takes the optional range and is optional
- a corrupt tarball behind an optional dependency still fails the install
- the lockfile round-trips optional edges and declared platforms
- a reused lockfile still skips — the cache-hit case, which is the one that
  fails if optionality is not recorded

Integration tests pin the platform without depending on which machine runs
them, by declaring `os: ["win32"]` for the case that must be skipped and
`os: ["linux", "darwin"]` for the case that must not. jerky targets WSL, Linux
and macOS and has an invariant against Windows support, so those two are
constants rather than a guess about the test runner.

The unit tests for `PlatformSupport::admits` are where npm's list rule is
pinned, negation and mixed lists included.

## 9. Deliberately not done

**No `--force` to install a skipped package anyway.** There is nothing to
force: the package is not missing, it is inapplicable, and installing a
darwin-arm64 binary on Linux produces a tree that is wrong in a way jerky
cannot detect afterwards.

**No `EBADPLATFORM`.** §2 settles this — the constraint is consulted only where
a skip is available.

**No re-resolution when the platform changes.** The lockfile is the same on
every platform, so there is nothing to re-resolve: the same file yields a
different plan on a different machine, which is the property §2 bought.
