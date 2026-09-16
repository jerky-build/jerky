# Linking package binaries into `node_modules/.bin`

**Date:** 2026-09-16
**Status:** Accepted
**Issue:** #20
**Depends on:** #29 (which mode bits survive extraction), #68 /
`docs/research/2026-09-14-pnpm-bin-executability.md` (how pnpm makes a bin
executable)

## The gap

`jerky install` produces a tree in which no locally-installed tool is
executable. A package's `bin` field is read by nothing, `node_modules/.bin`
does not exist, and a `package.json` script calling `tsc` finds nothing to run.

This also silently blocks #8. `jerky build` is specified as calling "the swc
installation for the project", which is a `.bin` shim that does not exist.

## 1. Where `bin` is read from: the abbreviated packument

`bin` rides in the abbreviated packument — the same response `dependencies`,
`os` and `peerDependencies` already come out of. Verified against the live
registry on 2026-09-16:

```
$ curl -H 'Accept: application/vnd.npm.install-v1+json' \
      https://registry.npmjs.org/typescript | jq '.versions["5.3.3"]'
{ "name": …, "version": …, "bin": {"tsc": "bin/tsc", "tsserver": "bin/tsserver"}, … }
```

So it is read into `VersionMetadata` beside `os` and `cpu`, carried on
`ResolvedPackage`, and recorded in the lockfile.

**The alternative was reading it off the package's own `package.json` in the
content store**, which is what npm and pnpm do. It is rejected, and the reason
is not the extra read — it is that the registry's copy is *more* correct than
the tarball's. Registry metadata is the manifest as `normalize-package-data`
left it at publish time, and that normalization is what expands the string form
and folds in `directories.bin`. The tarball carries whatever the publisher
wrote. Reading the store would mean reimplementing npm's normalizer to catch up
with a file the registry has already normalized for us.

It also keeps the linker's inputs pure: a `Plan` stays a function of the graph,
and the linker's I/O stays linking.

### It is recorded in the lockfile, for the reason peers and `os` are

An install whose importers all still match resolves nothing and builds its
graph out of the lockfile alone. A lockfile that did not name each package's
bins would link none of them on exactly the most common install there is —
the same cache-hit hole `declaredPeers` and the `optionalDependencies` block
were written to close, applied to `.bin`.

**Consequence, stated rather than hidden:** a lockfile written before this
change records no `bin`, and nothing distinguishes "recorded nothing" from
"declares no bins". Such a project links no bins until something makes it
re-resolve. Per the standing pre-1.0 rule the format changes in place with no
version bump and no migration — the fix is to touch a manifest or delete the
lockfile, and the CHANGELOG says so.

### Both published shapes are accepted

npm documents `bin` as either an object or a bare string, where the string form
takes the package's own name with any scope stripped — `@babel/cli` publishing
`"bin": "./bin/babel.js"` means a bin called `cli`.

In practice the registry appears to normalize the string form away: across
`coffee-script`, `uglify-js`, `browserify`, `nodemon`, `jshint` and `mocha` —
every version of each, including ones published in 2010 — not one abbreviated
entry carries a string. Both shapes are parsed anyway, and this is not
speculative generality. `Packument::versions` is a `BTreeMap<String,
VersionMetadata>`, so a single version that fails to deserialize fails the
*whole* packument and makes the package unresolvable at any version. The
asymmetry is total: a dozen lines against a package that cannot be installed.

### `directories.bin` is not read, and cannot be

npm's legacy `directories.bin` links every file in a named directory. The
abbreviated packument does not carry `directories` at all — `npm`'s own entry
has `{"doc": …, "man": …}` in the full packument and no `directories` key in
the abbreviated one — so reading it would mean abandoning the abbreviated
format for every package on the registry to serve a field npm itself normalizes
into `bin` at publish. Not implemented, and the packages that use it are served
by that normalization rather than by us.

## 2. Where the links go

Two destinations, which are the two `node_modules` jerky writes.

**An importer's `node_modules/.bin`** gets a link for each bin of each of that
importer's *direct* dependencies. Direct only, matching npm and pnpm: a
transitive dependency's CLI is not something the project declared and not
something it should be able to call by name.

**A store entry's private `node_modules/.bin`** gets a link for each bin of
each of *that package's* dependencies. This is the half the issue notes is
unaffected by the executability question. It matters for the same reason the
private `node_modules` matters at all: with no ambient hoisting, a package that
shells out to a dependency's CLI has nowhere else to find it. It is also what
#23 will need the moment lifecycle scripts run, since npm puts the package's
own `.bin` on `PATH` for them.

Workspace members are linked like any other direct dependency, with their bins
read from their own manifest rather than from a packument — a member has no
tarball and no registry entry, and a monorepo whose `packages/cli` cannot be
called from `apps/web` is missing the case monorepos exist for.

A bin link is a **symlink**, not a generated shim script. pnpm symlinks on
POSIX; a shim only earns its keep on Windows, which `docs/agents/invariants.md`
rules out in as many words.

### A real file already in `.bin` fails the install, and that is the existing rule

`place_symlink` refuses to overwrite anything that is not a symlink, anywhere.
Applied to `.bin` this has a consequence worth naming, because it is not
hypothetical: **yarn classic writes shell scripts into `node_modules/.bin`
rather than symlinks**, so the first `jerky install` in a repository migrated
from yarn stops with

```
error: …/node_modules/.bin/rimraf already exists and is not a symlink jerky can replace
```

Kept rather than special-cased. The two alternatives are worse in the two
directions jerky already has positions on: clobbering the file silently deletes
something jerky cannot prove it wrote, and skipping the bin leaves the project
without a tool it declared while reporting success. Refusing names the file and
leaves the remedy — remove `node_modules` — to the person who knows whether
anything in it mattered.

Note the asymmetry with §5, which *keeps* a yarn or npm shim it finds. That is
the same rule seen from the other side: convergence leaves alone what the plan
does not name, and this refuses to overwrite what the plan does name. A shim
for a package you no longer depend on is left where it is; one standing exactly
where jerky must write is a conflict it will not resolve on your behalf.

### Collisions are resolved by name, deterministically, and reported

Two dependencies may both declare a bin called `tsc`. The plan holds one
`BTreeMap` per `.bin` directory, so the loser is silently overwritten unless
something decides. **The first declaring dependency in sorted order wins**,
which makes the tree a function of the graph and not of iteration luck, and the
collision is reported as a warning beside the unsatisfied-peer ones. Not an
error: a project with two tools of one name is unusual but not broken, and the
one that wins is at least the same one on every machine.

**Only an importer's `.bin` reports one.** The resolution is identical inside a
store entry's private `.bin`, but the telling is dropped: the person running
the install chose neither of the colliding dependencies, and the directory a
report would name is a `.jerky/...` path they have no reason to open. A warning
nobody can act on is noise, and this one would fire on trees where nothing is
wrong.

## 3. Executability: chmod the target, `| 0o111`

Settled by `docs/research/2026-09-14-pnpm-bin-executability.md`, whose argument
is not repeated here. In short: packages routinely ship `bin` targets at
`0o644`; the file in the virtual store shares an inode with the machine-global
store entry, so chmodding it chmods the store; and that is nonetheless right,
because the mutation only ever *adds* execute bits and because everyone sharing
a jerky store entry has the same package at the same version and therefore
declares the same bins.

Three properties, each load-bearing:

- **Raise, never set.** `mode | 0o111`, not `= 0o755`. Monotonic and
  idempotent, so two concurrent installs cannot disagree about the result.
- **Read before writing.** A target that already has all three bits is left
  alone, which makes the warm case one `stat` rather than a `stat` and a
  `chmod`.
- **`NotFound` is tolerated.** Nothing serializes bin linking across concurrent
  installs, and whoever removed the package writes an equivalent file and
  chmods it in turn. pnpm tolerates it for the same reason.

### A workspace member's own files are never chmodded

Decided while implementing, and the one place this departs from npm, which
chmods a workspace package's bin like any other.

The store argument above does not reach a member: nobody else shares that
inode, so there is no "every consumer already agrees" to lean on. What there is
instead is version control. A member's bin is a file in the user's repository,
and raising its execute bit is a change `git status` reports and a reviewer has
to explain — caused by a command that was supposed to install dependencies.

A member shipping a non-executable bin therefore gets a shim that reports
`EACCES`, and `chmod +x` on it is the repository's own to make. That is a
worse failure than npm's for exactly one case and a better one for the case
where jerky would otherwise be writing to files it does not own.

### The `0o100` / `0o111` divergence is settled by leaving it alone

The research note flags that `archive::normalise_mode` consults owner-execute
only (`& 0o100`) where pnpm treats any execute bit as executable (`& 0o111`),
and asks for it to be decided deliberately when #20 lands, on the grounds that
#20 is what makes the bit load-bearing.

It is decided by observing that #20 makes it *less* load-bearing, not more.
Before this change, whether a bin could be run depended entirely on what the
tarball recorded. After it, every bin jerky actually links is chmodded at link
time regardless of what the tarball said — a collision's loser gets no shim and
so no chmod, which is the same answer one step earlier — so
widening `normalise_mode` would change the mode of files that are *not* bins,
on the strength of a group-execute bit the publisher probably did not mean, and
buy nothing for the case that prompted the question. `normalise_mode` stays at
`0o100`.

## 4. `bin` is untrusted input

Neither the name nor the target comes from jerky, and both become paths.

- **A bin name containing a separator escapes `.bin`.** `{"../../evil": "x"}`
  plants a link outside the directory. Names are restricted to a single path
  component that is neither `.` nor `..`.
- **The lockfile is checked on the way in, not only the packument.** §1 makes
  the lockfile the *primary* source of bins rather than a secondary one, since
  an install whose importers all still match reads nothing else — and the file
  is checked in and arrives through pull requests. Validating only what the
  registry served would leave the whole check bypassable by editing a file in
  a branch. `lockfile::load` re-runs the same validation.
- **A target climbing out of the package points anywhere on the machine.**
  `{"tsc": "../../../../etc/cron.daily/x"}` yields a `.bin/tsc` aimed at a file
  the package does not own — and then jerky chmods `+x` on whatever it found.
  Targets are required to stay inside the package directory, lexically.

A declaration failing either check is dropped rather than fatal, which is the
same call `Packument::versions_sorted` already makes about a malformed version:
one bad entry must not make an otherwise-installable package refuse to install.
Dropping is safe in the direction that matters — the cost is a missing shim,
against a link jerky should never have written.

## 5. Convergence

`.bin` is a real directory inside an importer's `node_modules`, so without a
case for it convergence would report it as something jerky declined to touch,
on every install, for every importer — the same lie `.jerky` is skipped to
avoid.

It is not merely skipped, though. `node_modules/.bin` is converged *against the
bins the plan names for that importer*, by exactly the ownership test the rest
of convergence uses: an entry is jerky's when it is a symlink whose target,
normalized lexically, lands inside the virtual store or on a member. That is
what makes "nothing stays on disk that is no longer recorded" true of a
dropped dependency's shim, and it leaves a shim npm wrote — which points into
`node_modules/<pkg>`, not into `.jerky` — reported and untouched.

A `.bin` this pass empties is removed, on the same reasoning that removes an
emptied scope directory: jerky never creates one it does not immediately fill,
so one it has just taken the last link out of is provably its own debris.

**Inside the virtual store nothing is converged**, because there is nothing to
converge: an entry's directory name encodes its identity *and* its peer
context, so the set of dependencies it links — and therefore the set of bins —
is a function of the key. An entry whose edges would change is a different
entry with a different name.

## 6. Order within `Plan::apply`

Two passes are added, each immediately after the links it depends on:

1. Entries — materialised across the pool, unchanged.
2. Store-internal edges.
3. **Store-internal `.bin` links**, which need the entry they point into.
4. Importer links.
5. **Importer `.bin` links.**
6. Convergence, which now also converges `.bin`.
7. The prune.

Both new passes are serial like the ones they sit beside. They are a handful of
syscalls per bin, and bins are rare — the fan-out in step 1 is there because
hard-linking a whole package tree is a different order of work.

## 7. Tests

Written before the code, at the seams the existing suite already uses.

- `registry` — a version declaring the object form, one declaring the string
  form, one declaring neither, all off one abbreviated packument; and the
  string form of a scoped package taking the unscoped name.
- `bin` name and target validation — a separator in the name, `..` in the name,
  a target climbing out, a target that is merely deep. Dropped, not fatal, and
  the rest of the package's bins survive.
- `lockfile` — a round trip preserving bins; a v2 entry with no `bin` key
  loading as a package with none.
- `plan_for` — the plan names a `.bin` link for a direct dependency's bin, none
  for a transitive one, one in the depending entry's private `.bin`, and one
  for a workspace member's own bin. More than one importer, per the standing
  invariant.
- `linker` — the link lands where the plan says and resolves to the real file;
  a target shipped `0o644` is `0o755` afterwards and the store entry is too; a
  mode is raised rather than replaced; a second apply rewrites nothing; a
  member's file is linked but not chmodded; a removed dependency's shim is
  converged away with its emptied `.bin`; an npm-written shim is left alone and
  reported.
- End to end — a real tarball whose bin ships non-executable, installed, and
  then *executed*. This is the test that would have caught every mistake above
  and the only one that proves the feature.

A note on how these are checked. Every one of them passed the first time it was
run, which proves nothing on its own, so each claim was confirmed by deleting
the code that answers it and watching the right test — and only the right test
— fail. That is what caught the validation test asserting with `Path::exists`,
which follows a symlink and reports `false` for a dangling one: it passed
unchanged with the whole of §4 removed, because the links it was looking for
dangle by construction. `symlink_metadata` is the check that means anything
here.
