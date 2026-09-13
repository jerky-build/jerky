# Bare install and devDependencies — Design

**Date:** 2026-09-13
**Status:** Approved
**Issues:** #19, #21 (this spec); unblocks #18; interacts with #20, #22, #40
**Amends:** `docs/agents/invariants.md` — the `devDependencies` rule
**Builds on:** `docs/specs/2026-09-10-workspace-design.md`,
`docs/specs/2026-09-08-resolver-and-lockfile-design.md`

## 1. Context: the command that does not exist

Every install issue so far covered `jerky install <pkg>`. The more common
command — the one a developer runs immediately after cloning a repo, and the
one CI runs on every build — has never had an implementation. `spec` is a
required argument (`src/cli.rs`), so there is currently no way to say *install
what this repo says it needs*.

It was correctly deferred. Bare install has nothing to do without a resolver to
interpret the ranges already in a real `package.json`, and nothing to
*reproduce* without a lockfile to read back. Both landed: #6 and #11 closed
with spec 2, and #47 made the lockfile something jerky reads and enforces
rather than only writes.

`devDependencies` arrives in the same slice rather than after it. Bare install
must decide what it installs before it can install anything, and answering that
question twice means building bare install twice. The two issues are one
design.

The machinery is closer than either issue suggests. `reusable_importers` and
the reuse-or-re-resolve split inside `install()` already do the hard part:
compare each importer's declared specifiers against what the lockfile recorded,
reuse what matches, re-resolve what does not. Bare install is that function
with no request folded in. What this spec mostly settles is semantics.

## 2. Settled decisions

**Bare install is convergent, not additive.** After it runs, each importer's
`node_modules` contains what its manifest declares — missing dependencies
installed, *and links for dependencies that were removed from `package.json`
deleted*. The alternative leaves a stale symlink that still resolves, so
`require` keeps working for a dependency the project no longer declares, and
the tree quietly disagrees with the manifest. The lockfile is already pruned on
every write (spec 2 §12); this is the same property applied to disk.

**The lockfile records the kind.** Each importer gets `dependencies` and
`devDependencies` blocks, which is pnpm's shape. The lockfile stays a complete
description of what to install, a production install can be planned from it
alone, and a dependency moving between sections shows up as a real diff rather
than as nothing. `lockfile_version` stays `1`: the format is unreleased, so
this costs a find-and-replace now and would cost a migration later.

**Bare install always covers the whole workspace, whatever the cwd.** The
resolver already seeds from every member and the lockfile already describes
every importer, so linking only the importer the user is standing in would
write a lockfile claiming importers whose `node_modules` does not exist —
directly against *nothing is recorded that is not already true on disk*.
Narrowing is #40's job, where a selector language belongs rather than a flag.

**A production install removes devDependency links, and never writes the
lockfile.** Convergence is not selectively applied. Not writing is the more
consequential half: the lockfile describes the full graph, so pruning it to
whatever a production install happened to link would make a CI run rewrite the
file it exists to reproduce, and the next developer install would put it all
back. Lockfile churn driven by whichever flag someone last used is exactly what
a lockfile is for preventing.

**Convergence removes only what jerky can prove it owns.** An entry in
`node_modules` is removable if it is a symlink whose target resolves inside
this workspace's virtual store or onto a workspace member. Anything else — a
real directory left by a previous `npm install`, a symlink pointing somewhere
else, an unrecognised file — is left alone and reported. jerky's layout makes
ownership provable, so convergence is total over what jerky manages without
ever deleting what another tool or a human put there. The first `jerky install`
in a repo that has seen npm must not be a destructive surprise.

## 3. The `sync` seam

The body of `install()` becomes one operation, and both commands become thin
callers of it:

```rust
pub enum Mode {
    /// Every kind, every importer. Resolves what is stale; writes the lockfile.
    Develop,
    /// `dependencies` only. Requires a lockfile that already matches every
    /// manifest; never writes one.
    Production,
}

pub enum Kind { Prod, Dev }

/// One dependency to add on top of what the manifests declare.
pub struct Request {
    pub importer: ImporterPath,
    pub name: String,
    /// What the registry is asked: a range, an exact version, or a dist-tag.
    pub seed: String,
    pub kind: Kind,
}

pub fn sync(
    workspace: &Workspace,
    store: &Store,
    registry: &dyn RegistryClient,
    mode: Mode,
    request: Option<&Request>,
) -> Result<Outcome, InstallError>;
```

So `jerky install` is `sync(.., Develop, None)`, and `jerky install <pkg>` is
`sync(.., Develop, Some(request))` followed by a manifest write.

**Adding a package is bare install over a manifest with one more line.** The
map that seeds everything is

```rust
declared: BTreeMap<ImporterPath, BTreeMap<String, String>>
//        importer path  ->  package name -> specifier verbatim
```

and a request is applied by inserting its `(name, seed)` pair into
`declared[request.importer]` *before* anything else runs. Downstream, nothing
knows a request happened. `reusable_importers` sees that importer no longer
matches what the lockfile recorded, marks it stale, and re-resolves it — which
is precisely what the current code hand-rolls by mutating `stale` after
computing reuse, and what the `if let Some(deps) = stale.get_mut(importer)`
branch exists to arrange.

**One reconciliation survives, and gets a name.** `jerky install lodash` must
seed the dist-tag `latest`, because that is what the registry is asked, but
what gets *recorded* is the exact resolved version — `declared_range` returns
`None` for a tag on purpose, so a manifest never carries a moving pointer. Seed
and recorded genuinely differ there, and the graph's specifier has to be set to
the recorded value before the lockfile is written, or the next install finds its
own lockfile stale and re-resolves a workspace nobody touched. This is inherent
to pinning by default rather than an artifact of the current structure, so it
survives the refactor. It moves from three scattered points in a 300-line
function into one place with a name.

**Why extract rather than add an `Option<PackageSpec>` parameter.** The smaller
change would thread `if let Some(spec)` through three separate points of the
hardest function in the codebase. The seam is also what #18 and #40 need: #18's
"verify the dependencies for that task are up-to-date and install them if they
are missing" is `sync` with no request, and #40 is `sync` over a subset of
importers. Extracting it before three callers exist is cheaper than after.

`Outcome` reports what was linked, what was removed, what was left alone, and —
when there was a request — the specifier to record. The manifest write stays in
the command layer, after `sync` returns, which preserves the existing ordering
rule: a failure anywhere leaves at worst an installed-but-unrecorded package,
never a `package.json` claiming a dependency that is not on disk.

## 4. Kinds through the stack

**The resolver does not change, and stays kind-blind.** A devDependency
resolves exactly like a dependency; only its input widens to include them.
A registry package's devDependencies remain unreachable because
`VersionMetadata` has no field for them, which is where that correctness
requirement is enforced — not by the resolver remembering a rule.

This is worth stating plainly because the rule as usually phrased ("only the
root project's devDependencies are followed") does not survive workspaces. In a
monorepo *every member is a first-party project*, not just the one keyed `.`.
The rule is: **every importer's `devDependencies` are followed; no registry
package's ever are.**

**`Manifest` gains kind-aware reads.** `dependencies()` currently returns
`dependencies` only, with a comment saying devDependencies are deliberately
absent. It grows a companion for `devDependencies`, and callers ask for what
they want.

**`Dependency` gains a `kind` field; the graph stays flat.** One map per
importer, keyed by name, with kind as a field rather than two maps. Lookups
stay single-branch, and a name can only mean one thing regardless of section.
The split into two blocks happens at *serialization* — the on-disk format is
pnpm's, the in-memory shape is jerky's, and neither constrains the other.

**`Importer::matches()` compares `(specifier, kind)` pairs.** This is what makes
the separate-blocks decision pay: a dependency moved from `dependencies` to
`devDependencies` at the same specifier makes its importer stale, so the
lockfile is brought into agreement rather than silently disagreeing about a
section.

## 5. Convergence and ownership

A new `linker::converge`, run once per importer after linking:

```rust
/// Remove the links in `importer_dir/node_modules` that `expected` does not
/// account for. Returns the entries left alone.
pub fn converge(
    importer_dir: &Path,
    workspace_root: &Path,
    expected: &BTreeSet<String>,
) -> Result<Vec<Unowned>, LinkError>;
```

**The ownership test.** An entry is jerky's to remove when it is a symlink
whose target, resolved relative to the link's parent and normalized lexically,
lands either inside `<workspace_root>/node_modules/.jerky` or exactly on a
workspace member's directory. Both cases are required: `symlink_dependency_from`
produces the first, `symlink_local` the second. Normalization is lexical, not
`canonicalize`, consistent with how the linker already compares paths — and
because a dangling link must still be recognised as jerky's rather than
preserved forever.

**The virtual store is pruned too.** Entries in `node_modules/.jerky` that are
not in `graph.packages` are removed. They are provably jerky's, and without
this, deleting a dependency would leave its unpacked virtual store directory in
the project forever. The machine-global content store at `~/.jerky/store` is
explicitly **not** touched: it is shared across every project on the machine,
so a project-local convergence has no basis for deciding an entry is dead.
Collecting it is a separate problem, filed as #52 — and an explicit command
rather than something install does, because silently deleting bytes in a
directory shared by every project on the machine is a bad surprise.

**Scoped names are handled structurally.** Convergence treats an `@`-prefixed
directory as a container, descends one level, and removes a scope directory that
empties. This is written now even though #22 means nothing scoped installs yet,
because the alternative is #22 discovering that convergence silently ignores
half the tree.

**`.bin` needs nothing.** It is a real directory, so the ownership test already
leaves it alone. #20 will extend convergence to own the shims it creates rather
than having to undo anything here.

## 6. Production installs

`jerky install --production` converges every importer to its `dependencies`
only, and **requires a lockfile that already matches every importer's
manifest**.

Requiring the match is what *earns* the read-only property rather than
enforcing it as a special case: with every importer reusable, there is nothing
to resolve, so there is nothing to write. Falling back to resolution would mean
computing a result that must then be deliberately thrown away — a shape that
invites someone to later "fix" it by saving.

**The match is over both kinds, not only the ones being installed.** A
production install that ignores a stale `devDependencies` block would pass in
CI on a lockfile that is genuinely out of date, and the whole point of the mode
is to refuse exactly that. So editing a devDependency without reinstalling
fails `--production` even though no devDependency would have been linked. This
is npm's behaviour and it is the right harshness: the fix is to run
`jerky install` and commit the lockfile, which is what should have happened
anyway.

It also means a production install performs no metadata requests at all. The
only network traffic is tarballs for packages the store does not already hold,
which is both the fast path and the narrow one.

This is `npm ci`'s guarantee, and it gives the frozen-lockfile CI story without
a third flag to design.

## 7. CLI surface

```
jerky install                     every importer, both kinds
jerky install <pkg>               + records under dependencies in the cwd's member
jerky install --save-dev <pkg>    + records under devDependencies          (-D)
jerky install --production        dependencies only, every importer
```

`spec` becomes `Option<String>`. `--production` conflicts with both `<pkg>` and
`--save-dev`, expressed with clap's `conflicts_with` so a contradictory
invocation is rejected at parse time rather than partway through an install.

`-D` is included as an alias because it is universal muscle memory. No
`--omit=dev` alias: one spelling per concept until someone asks.

**The cwd rule falls out.** `main.rs` calls `importer_for` **only when there is
a spec**. Bare install and `--production` need `find_root` and nothing else,
because "which importer did you mean" has no answer to get wrong when the
answer is every one of them. So standing in a non-member directory like
`tools/scripts` still errors for `jerky install lodash` — `importer_for`
already raises `NotInAMember` there — and works fine for bare `jerky install`.
That asymmetry is correct rather than inconsistent: ambiguity needs
alternatives, and a command that acts on everything has none.

## 8. Errors

Three new variants, all on the production path:

- **`ProductionLockfileMissing`** — names the expected path and says to run
  `jerky install` first.
- **`ProductionLockfileStale { importer, name, declared, locked }`** — names
  the one dependency that disagrees, and both values. The failure this guards
  is a manifest edited without reinstalling, so the message has to identify the
  edit rather than report that a mismatch exists somewhere.
- **Flag conflicts** — handled by clap, not by hand.

**Unowned entries are not errors.** They are reported through `Outcome` and
printed by `main.rs`, following the precedent `Workspace::warnings()` already
set for workspace patterns that matched nothing. A half-migrated repo should be
told what jerky declined to touch, not stopped.

## 9. Testing

Every change ends with `cargo fmt && cargo clippy --all-targets -- -D warnings
&& cargo test`, plus the diff checks in `docs/agents/invariants.md` — one of
which this spec amends (§10).

Behaviour worth a test:

- Bare install with no lockfile resolves and links every importer.
- Bare install with a matching lockfile makes **zero registry calls**. The test
  fixture can count them, so this is assertable rather than assumed.
- Removing a dependency from a `package.json` and reinstalling leaves the link
  gone, the `.jerky` entry gone, and the lockfile pruned.
- A dependency moved from `dependencies` to `devDependencies` at an unchanged
  specifier makes its importer stale.
- A member's `devDependencies` are installed; a registry package's are not.
- `--production` against a matching lockfile removes devDependency links and
  leaves the lockfile **byte-identical**. Asserted on bytes, because bytes are
  the actual guarantee.
- `--production` against an edited manifest errors and writes nothing.
- An unowned real directory, and a symlink pointing outside the workspace, both
  survive convergence and are reported.
- A dangling symlink into `.jerky` is recognised as jerky's and removed.

Every one of these runs on a workspace of more than one importer. A suite that
only ever sees `.` is not testing workspaces.

## 10. Effect on existing work

**An invariant narrows.** `docs/agents/invariants.md` currently reads:

> **Never follow a dependency's `devDependencies`.** Only the root project's,
> and that is spec 3.

with the diff check "`devDependencies` is read nowhere, and `VersionMetadata`
has no field for it". Both must change, because after this spec
`devDependencies` *is* read. The rule becomes: read only from a workspace
member's manifest, never from `VersionMetadata`, which still has no field for
them. The check becomes: `grep` for `devDependencies` returns `manifest.rs`,
where manifests are read, and `lockfile.rs`, where the on-disk block is named
by a `serde(rename)` alongside the existing `lockfileVersion` — and their
tests. Anywhere else, including `registry.rs`, is still a bug.

A second invariant gains a clause. *Nothing is recorded that is not already
true on disk* now has a converse worth stating: nothing stays on disk that is
no longer recorded.

**Issues.** #19 and #21 close together. #18 gains the seam it needs — its
"verify the dependencies for that task are up-to-date" is `sync` with no
request. #40 gains the same seam, over a subset of importers.

**Filed by this spec.** #52, garbage collection of `~/.jerky/store`.
Convergence makes it conspicuous: pruning a project's virtual store leaves the
machine-global content store growing without bound, and nothing currently ever
removes an entry from it. Settled there as an explicit command rather than
something install does, and noting that the store already carries its own
reference count — `hard_link_tree` means the filesystem's link count is
maintained for free and stays accurate even when a project is deleted with a
plain `rm -rf`, which no index could track.

## 11. Deliberately out of scope

**`.bin` linking (#20) and scoped layout (#22)** are untouched. Convergence is
written so that neither has to revisit it.

**`--filter` (#40)** stays deferred. Bare install covering everything is what
makes a selector a real feature rather than a workaround for the wrong default.

**Lifecycle scripts (#23)** remain unrun. A convergent install that also
executed arbitrary package code on every run would be a much larger security
surface than this spec is sized for.

## 12. Open questions

**Should convergence fail when an unowned entry shadows a declared
dependency?** Reporting is the decision above, and it is right for the general
case. But an unowned `node_modules/lodash` directory shadowing a declared
`lodash` means Node resolves the old copy and the install silently did not take
effect — which is the one case where reporting may be too quiet. Left open
because it wants evidence from real half-migrated repos rather than a guess.

**What removes a dangling link whose target left the workspace?** The ownership
test recognises links into `.jerky` even when dangling, but a `link:` to a
member that has since been deleted resolves nowhere and matches no member, so it
reads as unowned and is preserved. This is probably wrong, and the fix likely
belongs with whatever handles a member disappearing from `workspaces`.
