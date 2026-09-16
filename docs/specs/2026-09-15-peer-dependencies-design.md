# Peer Dependencies — Design

**Date:** 2026-09-15
**Status:** Approved
**Issues:** #34 (this spec); spins off `optionalDependencies`; interacts with #41
**Builds on:** `docs/specs/2026-09-08-resolver-and-lockfile-design.md`,
`docs/specs/2026-09-10-workspace-design.md`,
`docs/specs/2026-09-13-bare-install-and-dev-dependencies.md`

## 1. Context: the gap spec 2 named out loud

Spec 2 shipped transitive resolution and the lockfile, and listed peers in its
out-of-scope table with an explanation rather than a shrug:

> **Peer dependencies are absent, and that will be visible.** Real trees declare
> them, and ignoring them means jerky installs trees that npm would warn about.
> This is a known gap, not an oversight — resolving peers correctly interacts
> with the layout and deserves its own design.

That is this document. `VersionMetadata` reads `dependencies` and nothing else,
so `react-dom` arrives in a tree with no `react` linked beside it, and jerky
says nothing. The package then resolves `react` by climbing out of its own
`node_modules` — and in a virtual store there is nothing above it to find.

The reason this is a design and not a patch is that a peer is not an edge. It
is a constraint on the *dependent's environment*: `react-dom` declaring
`peerDependencies: { react: "^18" }` means "whoever installs me must give me a
compatible react", and the right answer depends on what the consumer already
has. Two consumers can legitimately give different answers, which means one
published `react-dom@18.2.0` can need two different directories on disk. That
is a change to what identifies a package, and identity is load-bearing in three
places at once: the virtual store directory name, the lockfile key, and the
resolver's dedupe.

In a hoisted `node_modules` none of this arises, because a peer is satisfied by
whatever landed higher up the tree — that is most of why hoisting existed. The
virtual store buys strict encapsulation and pays for it here.

## 2. Settled decisions

**jerky never fabricates a dependency edge.** A peer is satisfied from what the
consumer already resolved, or it is not satisfied. npm 7+ auto-installs missing
peers on the consumer's behalf; jerky does not. The lockfile's central pairing
is `(specifier, kind) -> resolution`, and `Importer::matches` measures staleness
in declared specifiers. An auto-installed peer has no specifier in any manifest,
so it would need a second, parallel staleness story running alongside the first.
Auto-install remains available later as a flag; it is not the default, and it is
not this spec.

**A peer is satisfied by the nearest ancestor on the dependency path, and a
package's own dependency satisfies its own peer.** Walk up from the dependent
toward the importer; the first ancestor declaring that name among its own
dependencies wins, and the importer's own dependencies are the last resort. A
package declaring the same name in both `dependencies` and `peerDependencies` —
the usual way an author says "I will take yours, but I ship a fallback" —
satisfies its peer from its own dependency, checked before any ancestor. This
is the rule the ecosystem's published peer ranges were authored against. An
importer-only rule would be simpler to implement and would report unsatisfied
peers on trees pnpm installs clean, which is the #34 complaint inverted: noise
instead of silence is still the wrong answer.

**An unsatisfied required peer warns; the install succeeds.** Under the rule
above there is exactly one candidate per peer, so there is no conflict case —
only *satisfied*, *missing entirely*, or *present but outside the declared
range*. The latter two warn and exit 0. Making them fatal would render jerky
unable to install large parts of the live ecosystem, where peer ranges routinely
lag a major release; refusing a tree that works is not a stricter kind of
correct, it is an unusable one. `--strict-peers` belongs with #41's
one-version-per-workspace flag, because both are *policy on top of a resolution*
and both need the same configuration surface. Designing two is how a project
ends up with two.

**One published version can become several nodes, keyed by the peers it
resolved against.** This is forced, not chosen. With no ambient hoisting, the
link target at `.jerky/<entry>/node_modules/react` differs per consumer, so one
directory cannot serve both. The alternative — one copy per `name@version`, with
an error on the second peer resolution — is not a simpler design but a wrong
one: two build tools each pinning a different `typescript` is routine.

**The identity change is paid now, while the format is young.** `PackageId`
gains a peer context, and the lockfile format changes to match without a
version bump — jerky has shipped no 1.0, so there is no committed population a
version number could distinguish. Retrofitting identity after a release would
cost strictly more.

**Version selection never depends on peer resolution.** This falls out of the
first decision and is the load-bearing consequence of it: because peers are
satisfied only from already-resolved nodes, peer handling can run entirely
*after* version resolution, as a pure transformation of a finished graph. It
adds no registry traffic and needs no change to `Walk`.

## 3. Scope

### In scope

- `peerDependencies` and `peerDependenciesMeta` read off the abbreviated packument
- Optional peers: declared-optional and unsatisfied is silent, not a warning
- A peer resolution pass over the resolved graph, duplicating nodes by peer context
- `PackageId` carrying a peer context, and the directory / lockfile-key spelling of one
- The lockfile records resolved peers and declared peers
- Warnings for unsatisfied required peers, surfaced on every install
- A committed pnpm parity oracle for the resolution rule

### Explicitly out of scope

| Deferred | Goes to |
|---|---|
| `optionalDependencies` | Its own issue, spun off by this spec |
| Auto-installing missing peers | Later, as a flag |
| `--strict-peers` / failing on unsatisfied peers | With #41's config surface |
| Peer resolution across the workspace boundary | Out — an importer is the root of its own path |
| `peerDependencies` on a workspace member | Later; members link in place and have no store entry |

**`optionalDependencies` is a separate mechanism, not a sibling feature.** Spec 2
deferred "peer and optional dependencies" on one line, which reads as one task
and is two. Peers are an identity-and-constraint problem; optional dependencies
are a failure-tolerance problem — a package whose tarball fails to fetch or
unpack is skipped rather than fatal. Nothing in this spec helps build that.
Optional *peers* are in scope, because `peerDependenciesMeta.optional` is a
property of a peer and only reachable through the machinery here.

## 4. Reading peers off the registry

`VersionMetadata` gains two fields, both `#[serde(default)]`:

```rust
#[serde(default, rename = "peerDependencies")]
pub peer_dependencies: BTreeMap<String, String>,
#[serde(default, rename = "peerDependenciesMeta")]
pub peer_dependencies_meta: BTreeMap<String, PeerMeta>,

pub struct PeerMeta {
    #[serde(default)]
    pub optional: bool,
}
```

No new request and no full packument: the abbreviated form jerky already asks
for (`application/vnd.npm.install-v1+json`, `src/registry.rs`) carries both.
Verified against the live registry — `react-dom@18.2.0` returns
`peerDependencies: {"react": "^18.2.0"}`, and `vite`'s latest returns a
`peerDependenciesMeta` marking seven peers optional.

The same fetch also carries `devDependencies` and `optionalDependencies`, and
`VersionMetadata` continues to drop both. The existing comment states the
discipline — *a field that exists is a field someone will read* — and it applies
unchanged: `optionalDependencies` is out of scope in §3, so it must not be
parseable, or someone will read it and half-implement it.

A peer named in `peerDependenciesMeta` but not in `peerDependencies` is ignored.
It is meaningless rather than malformed, and the registry has published worse.

## 5. Identity: `PackageId` grows a peer context

```rust
pub struct PackageId {
    pub name: String,
    pub version: String,
    /// What this node resolved *against*. Empty for the overwhelming majority
    /// of packages, and empty is what makes this change invisible to them.
    pub context: BTreeMap<String, PackageId>,
}
```

**A context, not a peer map**, and the distinction is load-bearing. It holds two
kinds of entry: this node's own resolved peers, and *each dependency that itself
carries a context*.

The second kind was missed when this spec was first written, and the omission
was a silent-corruption bug rather than an incompleteness. Consider a `wrapper`
that declares no peers at all but depends on a `plugin` that peers `react`, with
two apps above it supplying different reacts. `plugin` correctly becomes two
nodes — but `wrapper` must point at a different one in each subtree, and with
only its own peers in the key both copies render `wrapper@1.0.0`. Since
`ResolvedGraph::packages` is keyed by `PackageId`, the two collide and one
subtree is dropped with nothing reported. Folding the dependency's context in is
what gives them different keys:

```
wrapper@1.0.0(plugin@1.0.0(react@17.0.0))
wrapper@1.0.0(plugin@1.0.0(react@18.2.0))
```

This is also why the values are full `PackageId`s: the fold has to nest.

The map value is a full `PackageId`, not a version string, because a peer may
itself have been duplicated by *its* peers, and two contexts differing only at
that depth are genuinely different contexts. `BTreeMap` for the same reason the
graph already uses one: two machines resolving the same tree must serialize
identically, and an unsorted peer context would hand that property straight
back.

### The spelling

`Display` owns the encoding, because the rendered string is not cosmetic — it is
the virtual store directory name (`install.rs` passes `id.to_string()` as
`dir_name`) and the lockfile's `packages` key. One encoder, one decoder, one
place to change.

The form follows current pnpm's, which the ecosystem can already read:

```
react-dom@18.2.0(react@18.2.0)
@storybook/react@7.6.0(@babel+core@7.24.0)(react@18.2.0)
a@1.0.0(b@2.0.0(c@3.0.0))(d@4.0.0)
```

- Each peer is parenthesised, and peers are rendered in sorted order.
- **Parenthesised rather than delimited, because the context nests.** An earlier
  draft of this spec specified pnpm's older flat form — peers joined with `+`,
  the suffix introduced by `_` — and that form cannot express what the next
  bullet requires: `a@1.0.0_b@2.0.0+c@3.0.0` cannot say whether `c` is a peer of
  `a` or a peer of `b`. pnpm abandoned the flat form for this reason, and the
  argument that the flat form is what the ecosystem reads was simply out of
  date. `_` survives only as the collapsed-hash marker below, where it stays
  unambiguous because a readable suffix always opens `(`.
- A scoped peer's `/` is written `+`, since the suffix must stay inside one path
  component. The base name keeps its `/` and its second component, which the
  linker already expects.
- Each peer renders as its own full `Display`, recursively.
- **The recursion needs no cycle guard, because a cyclic id cannot be built.**
  An earlier draft required one, reasoning that peer cycles are real — they are,
  in the babel and eslint plugin ecosystems — and that the encoder would
  otherwise not terminate. But `peers` owns its values, so a `PackageId` is
  necessarily a finite tree and a guard would be unreachable code. The
  obligation is real and simply sits elsewhere: whatever *builds* these ids from
  a cyclic graph must terminate, which is §6's memoization.
- **The empty-peer-set spelling is byte-identical to today's `name@version`.**
  This is a requirement, not an observation. It keeps the change invisible for
  every package without peers — which is nearly all of them — so a lockfile
  regenerated after this change differs only where peers actually appear, and
  every existing test asserting a key keeps asserting the same string.

### The hash fallback

Recursive rendering is unbounded, and a single path component cannot exceed 255
bytes on the filesystems jerky targets. When the rendered key would exceed
**200 bytes**, the entire suffix collapses to `_` plus a short hash over the
exact recursive context. The threshold leaves headroom rather than sitting on
the limit.

The bound is measured over the whole rendered key rather than per path
component, which for a scoped package is conservative — its `@scope/` prefix
counts toward a limit it does not actually consume, so scoped packages collapse
slightly earlier than they need to. That is the right way to be wrong here:
the cost is a few keys hashed that need not have been, against a bound that
cannot be quietly exceeded.

Hashing is a readability loss and is therefore last-resort, not the default.
A spec that hashed every suffix would produce lockfiles no human can review.

### Parsing it back

`split_name_and_version` is `rsplit_once('@')` and is used for keys that are
now a strict subset of what can appear. It is left alone and given a peer-aware
sibling, `split_key`, which finds where the suffix begins and then *delegates*
to it — one encoder, one decoder. Overloading the existing function would
silently change `alias_target`'s behaviour, which shares it and must not learn
about peers.

`split_key` returns the name and version only; the suffix is discarded. Nothing
needs to parse it back, because §7 records resolved peers in a field of their
own and the suffix's only job in the key is to keep two resolutions of one
version apart. That is also what makes the hash fallback possible at all — a
collapsed suffix is not reconstructible, and nothing asks it to be.

Finding where the suffix begins is not quite "the first `_`": npm permits `_` in
a package name. A readable suffix is found by its opening `(`, which a name may
not contain; a collapsed one is recognised only as a fixed run of hex at the
very end of the key.

## 6. The peer resolution pass

`Walk` is not modified. It produces today's peer-blind graph, and a second pass
transforms it:

1. Walk down from each importer, carrying the path of ancestors.
2. At each node, for each declared peer, look for a provider: the node's own
   dependencies first, then the nearest ancestor declaring that name, then the
   importer's own dependencies.
3. A provider whose version satisfies the peer range is recorded in the node's
   peer context. A provider outside the range, or no provider at all, records a
   diagnostic — unless the peer is declared optional, in which case an absent
   provider is silent.
4. A node whose peer context is non-empty becomes a distinct `PackageId`, and
   every ancestor on the path to it is rewritten to point at the new id — which
   means those ancestors' own identities change too, and the duplication
   propagates upward to the importer.

The pass is pure graph-to-graph. It fetches nothing, touches no filesystem, and
is therefore testable exactly the way `tests/resolve.rs` tests the walk — which
is the property `resolver.rs`'s module documentation names as the reason it is
shaped this way.

**It runs in three stages, and the split is what makes cycles safe.** First
*discover*: find every instance — one per (package, the environment its subtree
sees) — naming none of them. Then *identify*: give each instance its id, which
depends on its dependencies' ids. Then *emit*: build the package map, mapping
every edge through the ids just assigned.

Doing this in one stage is the obvious implementation and it is wrong. A node's
id is not known until its subtree is walked, so the edge that closes a peer
cycle has to be handed *something* mid-walk — and whatever it is handed will not
match the id the node ends up with. The result is an edge naming a node that is
never emitted: a dangling reference in a graph whose own documentation claims it
is reachable by construction. Splitting discovery from naming removes the
question, because by the time any edge is written every instance already has its
final id.

A cycle still cannot be spelled out inside a name — a `PackageId` owns its
context, so it is finite by construction. The edge that closes the cycle is
therefore left out of the *name* while remaining in `dependencies`, which is the
only honest split available: the name stays finite, and the edge still points at
a node that exists.

**Termination and identity share one key**, and getting it wrong is subtle. The
pass memoizes each node and treats a repeat as a hit — the same novelty gate the
main walk uses, `if self.packages.contains_key(&id) { return Ok(()) }` — so a
peer cycle terminates for the same reason a dependency cycle does. An iteration
cap would be guessing. The two cuts below are the same argument applied to the
two questions the walk does not answer, and not a second kind of reasoning.

What it keys on is *not* `(node, its own resolved peers)`, which was this spec's
first answer and is wrong for exactly the `wrapper` case in §5: a node with no
peers of its own has the same key in every context, so the first copy computed
comes back for all of them and the duplication never happens. The key is
`(node, the providers its whole subtree can see)` — every peer name reachable
from the node, itself included, mapped to **which copy** currently provides it.
Two visits agreeing on all of them must produce the same subtree, which is what
makes the key sound.

**"Which copy" and not "which `name@version`", and the difference is the whole
of #110.** This spec first said the providers were mapped to peer-blind ids,
and that key is under-fragmented whenever a provider is reachable only across a
peer edge: `host` peers `mid`, `mid` peers `leaf`, and two importers supply
different `leaf`s. `mid` correctly becomes two copies. `host` does not — it
*peers* `mid` rather than depending on it, so `leaf` never enters `host`'s
subtree, both visits see the same `mid@1.0.0`, and one `host` serves both
importers wired to whichever `mid` was named first. The environment therefore
names each provider as a copy — an instance, recursively — so the two `mid`
copies make two `host`s. The provider's copy is settled by what was above *the
provider*, which is also what keeps a nearer package of the same name, sitting
between the provider and the dependent, from making two different providers
look alike.

The alphabet is not what was wrong, and widening it across peer edges is not
the fix. It would key on names rather than copies, and it would look each name
up where the dependent sits rather than where the peer was answered, so the
shadowed case above stays broken. It stays a dependency-edge closure, and the
recursion carries what a peer edge reaches.

The converse does not hold, and the spec should not claim it: two visits that
differ somewhere in that environment often still produce the *same* node, when
the name they differ on is one the subtree always answers internally. The key is
therefore finer than strictly necessary — it costs extra instances, never a
missed duplication, which is the direction to err in. That claim is about a
key that names copies; it was **false** of the peer-blind one, which missed a
duplication outright, and that is the correction #110 records.

A name answered by the node's own dependencies is left out of the key
altogether. Every copy of a node answers it the same way, so it distinguishes
nothing, and leaving it out is half of what bounds the recursion: each entry is
resolved against a prefix of the ancestor chain no longer than the one that
asked for it, so the chain cannot grow as the lookups nest. A prefix of the
*same* length is allowed — that is what a provider in the immediately enclosing
frame takes — so the other half is a repeat guard on `(package, how much of the
chain was visible)`, which over a chain that cannot grow is a finite space. A
re-entered query contributes the package and no environment of its own, which
is the loop an importer supplying `a` makes while `a`'s subtree peers `a`. Same
direction to err in: fewer things telling two copies apart, never two copies
conflated that the loop itself distinguishes.

**Three cuts, not one.** The pass memoizes a node and treats a repeat as a hit,
which is the termination argument above; naming a node cuts a loop a second
time, and so does the environment. They are three because they answer three
different questions — which copies exist, what each is called, and which copy
answered a peer — and a single gate cannot serve all three: the first must
happen before any name exists, and the last must happen while a name is being
computed. What they share is the direction they err in.

The set of names is a least fixed point over the peer-blind graph, computed
once before the walk down. A fixed point rather than a recursive walk because
the dependency graph has cycles, and the sets only grow over a finite alphabet,
so it terminates.

**A resolved peer contributes the provider's final name to the dependent's
id**, not the id the provider was found under. The two differ exactly when the
provider's own subtree carries a context — which is the duplicated case — so
naming the found id spells two distinct copies of a dependent identically, and
the map they are emitted into keeps one.

Where the provider's final name is the one being computed — a peer pointing
back up at an ancestor — the fallback is a *cut* name and not the found id: the
provider spelled the same way, over its own stack, so that the only thing left
out is the loop. Cutting to the found id instead reproduces #110 exactly, one
peer cycle along, and it is worth being plain about why: two copies of a
dependent are told apart by which copy of the provider answered, and the found
id is peer-blind, so both copies spell the same. The cut falls at the second
visit to an instance rather than the first, which is what leaves everything
short of the loop — the provider's own resolved peers, and the contexts its
dependencies carry — inside the name. It costs one more unrolling than a
first-visit cut: a two-node loop is spelled twice and then closed.

### Why not fold peers into the walk

Keying the walk's memo on peer context would make it one pass instead of two.
It would also make `selections` almost never hit, since context differs per
path — so the walk would lose its dedupe, the level-order packument concurrency
would lose its batching, and the cycle-termination argument would have to be
rewritten. Two cheap passes beat one expensive one, and the second pass costs no
network at all.

## 7. The lockfile

`Entry` gains two fields:

```rust
/// The peers this entry resolved against: local name -> the key of the node.
#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
peers: BTreeMap<String, String>,
/// What the package *declared*, range and all, so diagnostics can be
/// recomputed without re-resolving.
#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
declared_peers: BTreeMap<String, DeclaredPeer>,
```

`ResolvedPackage` carries `peers` — what this node's own peers resolved to —
alongside `declared_peers`. It is deliberately not derived from `id.context`,
which *also* holds the dependencies folded in to keep two copies apart, and a
name can legitimately appear in both. The lockfile records what a package
resolved as a peer, so the graph has to keep the two apart rather than making
this issue reconstruct one from the other.

**Resolved peers are their own map, not extra entries in `dependencies`.**
Merging them would leave the linker and the format untouched, which is
tempting and wrong: the recorded `dependencies` block would stop corresponding
to what the package actually published, so anyone diffing jerky's lockfile
against a real `package.json` sees edges the package never declared. Provenance
is also what a future `--strict-peers`, or a `why` command, has to read. The
cost is one union in the linker, and notably *not* a change to
`ResolvedGraph::reachable` — under §2's nearest-ancestor rule a peer target is
always already reachable through whoever declared it.

**Declared peers are recorded because warnings must survive a cache hit.** This
is the non-obvious one. `reusable_importers` skips resolution entirely when
every recorded `(specifier, kind)` still matches the manifest — so a project
would warn about an unsatisfied peer on first install and go silent on every
install afterwards, with nothing about the project having changed. Recording
what each package declared makes the warning a pure function of the lockfile,
so install is idempotent in what it *says* and not only in what it writes.
Recording the diagnostics themselves instead would put derived state in a file
whose job is recording facts, and it would go stale against a hand-edited
manifest.

**`LOCKFILE_VERSION` stays at `1`.** jerky has not shipped a 1.0, so there is
no population of committed lockfiles for a version number to tell apart — the
format simply changes, and a stale file is regenerated. Bumping it would spend
the version on a distinction nothing can observe and leave a number in the
file's history that never separated anything.

This is the precedent the bare-install spec set, keeping `lockfile_version` at
`1` through a shape change on the grounds that "the format is unreleased, so
this costs a find-and-replace now and would cost a migration later." It is also
why #104 was closed: there is no version 1 population to migrate *from*, and
with the version unchanged there is not even a version to migrate *to*.

Versioning becomes real work at 1.0 and not before.

## 8. Diagnostics

Unsatisfied required peers ride back as a field on the install outcome and are
printed in `main.rs`, matching how the workspace-pattern and left-alone warnings
already work. Nothing inside the resolver prints.

One line per unsatisfied peer, to stderr, in the existing `warning: ` style,
with missing and out-of-range distinguished:

```
warning: react-dom@18.2.0 wants peer react@^18.2.0, but the nearest provider has react@17.0.2
warning: @testing-library/react@14.0.0 wants peer react-dom@^18.0.0, which nothing provides
```

Warnings are deduped by `(dependent name@version, peer name)`. Without this the
§6 duplication multiplies one logical complaint by the number of cloned copies,
which turns a real signal into scroll. A bare summary count is unactionable, and
hiding the lines behind `--verbose` reproduces the silence #34 was filed about.

An optional peer that is simply *absent* never warns. That is the entire
content of `optional: true` — and it is narrower than "optional peers never
warn", which is what an earlier draft of this section said, contradicting §6
two pages earlier. A provider that is present but out of range warns whether
the peer is optional or not: `optional` says the peer may be missing, not that
any version of it will do.

**An unsatisfied peer is left unlinked.** Whether it was missing or merely out
of range, it does not enter the node's context and nothing is linked for it, so
a package gets nothing rather than a version it explicitly rejected. An
unparseable peer range fails the same way and is reported — treating nonsense
as satisfied would silently record a resolution nobody asked for.

## 9. Testing

**Tree-shape tests**, in the `tests/resolve.rs` style — registry-free, over
`FixtureRegistry`, each a claim about the design:

- a peer satisfied by the importer's own dependency
- a peer satisfied by the nearest ancestor, with a further ancestor also
  declaring it, to pin *nearest* rather than *any*
- a package's own dependency satisfying its own peer, ahead of an ancestor that
  also provides one
- two importers giving one package different peers, producing two nodes
- duplication propagating upward through an intermediate dependent
- a peer cycle terminating
- an optional peer, unsatisfied, producing no diagnostic
- an out-of-range provider producing a diagnostic distinct from a missing one
- the empty peer context rendering byte-identically to `name@version`

**A pnpm parity oracle**, generated once and committed as a fixture, in the
pattern `tests/fixtures/semver-oracle.json` already establishes. A set of real
trees is resolved by pnpm, and the test asserts jerky's peer-keyed node set
matches pnpm's lockfile.

This is not belt-and-braces. Peer resolution is the one area of this project
where the correct answer is defined by another implementation's behaviour rather
than by a written specification, so fixture tests can only ever prove jerky
matches *this document's reading* of the rule. The oracle is what tests the
reading. pnpm is already available in the benchmark environment (#84).

Live-registry integration tests are deliberately skipped: they would make the
suite depend on registry uptime for no signal the oracle does not already give.

## 10. Effect on existing work

- **#41 (one version per workspace)** becomes the home for `--strict-peers`;
  the two flags are one configuration surface (§2).
- **Spec 2 §3's deferral table** is discharged for peers. The
  `optionalDependencies` half is spun off as its own issue rather than left
  implied.
- **`docs/agents/invariants.md`** is unaffected. Nothing here changes the
  `devDependencies` rule; a dependency's peers are read, a dependency's dev
  dependencies still are not.

## 11. Deliberately out of scope

**Peer resolution across the workspace boundary.** An importer is the root of
its own dependency path, and a peer is not satisfied by a sibling importer's
dependencies. Workspaces are independent consumers; making one importer's tree
depend on another's declarations would reintroduce ambient resolution in a new
place.

**`peerDependencies` declared by a workspace member.** A member is linked in
place and has no store entry to key, so duplicating it by peer context is
meaningless. Members declaring peers are rare and the failure mode is a missing
warning, not a broken tree.

## 12. Open questions

**Does the collapsed form ever collide?** The readable form is exact — it
renders the full recursive context, and there is no guard truncating it — so
collisions are possible only among keys long enough to hash, and then only at
the 64 bits the short hash keeps. That is not a number worth worrying about,
but the pnpm oracle is where it would show up if the reasoning is wrong.

**Is 200 bytes the right hash threshold?** Chosen for headroom under a 255-byte
component limit. Real trees will say whether it fires often enough to hurt
readability or rarely enough not to matter.
