# Lifecycle Scripts — Design

**Date:** 2026-09-16
**Status:** Approved
**Issues:** #23 (this spec)
**Builds on:** `docs/specs/2026-09-16-bin-linking-design.md`,
`docs/specs/2026-09-16-optional-dependencies-design.md`

## 1. Context: the feature that makes the store a problem

Lifecycle scripts are how native packages build themselves. A package manager
that never runs them cannot install a meaningful slice of the registry —
`sharp`, `esbuild`'s older releases, every `node-gyp` addon, `@parcel/watcher`,
`bare-fs`. #23 has sat open since the beginning for two reasons, and only one
of them is the usual one.

The usual one is that this is arbitrary code execution triggered by installing
a dependency, which makes it as much a security design as a feature. §4
answers that, and the answer is an allowlist.

The unusual one is specific to jerky and is the reason this document exists at
all rather than a paragraph in spec 1. **jerky's virtual store is hard links
into a machine-global content store.** `linker::hard_link_tree` shares inodes:
the file at `node_modules/.jerky/sharp@0.33.2/node_modules/sharp/index.js` and
the file at `~/.jerky/store/v1/<integrity>/index.js` are one inode with two
names, and so is every other project's copy on the machine. A build script
that writes to a file in place therefore does not modify this project's copy of
the package. It modifies **the store**, and with it every project that has ever
installed that package.

npm does not have this problem because npm copies. pnpm has exactly this
problem and answered it with a side-effects cache. §3 is jerky's answer.

The precision matters, because the hazard is narrower than "a build mutates
things" and the narrowness is what makes the cheap answer viable. The
*directory* in the virtual store is this project's own — it is created by
`populate_virtual_store` and holds nothing but links — so a build that only
**adds** files, which is what a `node-gyp` build mostly does when it writes
`build/Release/binding.node`, is already safe. So is any tool that writes a
temporary file and renames over the target, since the rename replaces a
directory entry and leaves the old inode alone. What is not safe is a true
in-place write, and a `chmod`, which the invariants already single out: a store
entry's permissions are shared by every project at once.

That set is small but it is neither enumerable nor detectable. A script is a
shell command and may do anything. So the design cannot be "identify the
dangerous builds"; it has to be "make building unable to reach the store".

## 2. Settled decisions

These were decided before this document was written and are recorded here as
answers rather than argued a second time below.

1. **Both halves in one spec.** A workspace member's own scripts and a
   dependency's scripts. The registry slice #23 is about is entirely
   dependencies, so a spec covering only members would not be the feature.
2. **Copy instead of link, only for packages with an install script.** §3.
   Not a side-effects cache; §9 records why and what would reopen it.
3. **An allowlist, empty by default.** §4. Nothing a dependency declares runs
   until it is named in the root manifest.
4. **A required dependency's build failure fails the install**, the tree is
   left on disk, and the lockfile is not written. §6.
5. **An optional dependency's build failure drops it from the tree** with a
   warning, as a platform mismatch already does. §6. This restores the case
   `optionalDependencies` was invented for, which
   `docs/specs/2026-09-16-optional-dependencies-design.md` §1 parks on the
   grounds that "jerky runs no lifecycle scripts and builds nothing". This is
   the spec that reopens it.

## 3. Where a build happens

**A package whose publish declares an install script is copied into the
virtual store rather than hard-linked. Everything else links exactly as it does
now.**

`Plan` already distinguishes what it materialises; this adds one bit to the
decision and changes nothing else about the layout. The package still lands at
`node_modules/.jerky/<id>/node_modules/<name>/`, its dependencies are still
symlinked in beside it, and `converge` and `prune_virtual_store` still own it
the same way. Only the inodes differ.

### Why this rather than a side-effects cache

The measured population is the argument. Across the 2646 packuments in the
benchmark recording — both fixtures, 2910 and 1291 packages — exactly **70
carry `hasInstallScript`**, which is 2.6%. Copy-on-write therefore costs one
copy of roughly ten packages per project, against a tree where the other 2900
still share their bytes with every other project on the machine. That is a cost
worth paying to keep the store's central invariant — that an entry is immutable
and shared — true without qualification.

A side-effects cache would save those ten copies and would cost a cache key
nobody can write down correctly. What a build depends on is not knowable from
outside it: the node ABI, the platform, the libc, the compiler, the package's
own resolved dependencies, and whatever the script reads out of the
environment. A key that misses one of those shares a wrong build across every
project on the machine, which is a worse failure than the one it optimises.
§9 keeps it on the table behind a measurement.

### What the store keeps

The store entry stays exactly as extracted: unbuilt, keyed by tarball
integrity, immutable. A built tree exists only inside a project. Two projects
using one native package build it twice, and a project deleted with `rm -rf`
takes its builds with it. This is the pnpm behaviour before side-effects
caching and the npm behaviour always.

## 4. Which scripts run, and whose

### The three, plus one synthesized

`preinstall`, `install`, `postinstall`, in that order, read from the
`package.json` inside the extracted tree.

Plus npm's synthesized default: **a package with a `binding.gyp` at its root
and no `preinstall`, `install` or `postinstall` gets `node-gyp rebuild` as its
`install` script.** This looks like a compatibility curiosity and is not — it
is how a large share of native addons are built, because they never had to
write the script down. Omitting it would mean supporting lifecycle scripts and
still failing to build exactly the packages the feature is for. The `binding.gyp`
is detected on disk in the extracted tree, not from the packument, because the
packument does not carry a file list.

`prepare` runs for **workspace members only**. It is the convention for
build-on-install in a repository, and npm runs it for the root project on
install. It has no meaning for a registry dependency here, because jerky has no
git dependencies — the other place npm runs it — and a published tarball is
already prepared.

Nothing else. No `prepublish`, `prepublishOnly`, `prepack` or `postpack`: those
belong to publishing, which jerky does not do. No general `pre<x>`/`post<x>`
chaining; that belongs to the task runner (#13, #16) and inventing it here
would put two definitions of it in the codebase.

### Members always; dependencies only when allowlisted

A workspace member's scripts always run. Its files are in the user's
repository, under version control, written or reviewed by whoever is running
the install — the same argument the invariants use for never chmodding a
member's own files. Requiring a user to allowlist their own `postinstall` would
be asking permission to run code they just wrote.

A dependency's scripts run only if the package is named in the root manifest:

```json
{
  "jerky": {
    "allowedScripts": ["sharp", "@parcel/watcher"]
  }
}
```

Exact names, not ranges and not patterns. A range invites the belief that the
allowlist is a security boundary against a *version*, which it is not — the
lockfile is what pins versions, and this is a statement about a package's
publisher. A pattern (`@myorg/*`) invites allowlisting a namespace anyone can
publish into.

**Why deny-by-default.** pnpm 10 made this switch and it was well received.
jerky has no installed base to break, so it can have the safe default without
the migration pnpm paid for, and choosing the unsafe one now to avoid choosing
it later would be the worst of both. The cost is real and is borne on purpose:
a tree that builds under npm will not build under jerky until the user names
what may build. That is the intended shape of the failure — visible, at install
time, naming the packages, rather than invisible and at 3am.

**Every install reports what it skipped**, not only the first. The precedent is
#105, which warns about unsatisfied peers on every install and argues the case:
a warning printed once is a warning nobody sees, because the install that
matters is the one on a machine that has never run it before. The report names
each package and says what to add.

The allowlist lives in `package.json` under a `jerky` key and **not** in a
config file of its own, because there is no config file — #12 is unbuilt. When
#12 lands it can absorb this, and the spec for it should say what happens to a
manifest that still carries the key.

## 5. Reading the declaration

Two different facts, from two different places, and conflating them is the
mistake this section exists to prevent.

### Whether: `hasInstallScript`, from the packument

The abbreviated packument — `application/vnd.npm.install-v1+json`, which
`registry.rs` already requests — carries a per-version boolean
`hasInstallScript`, **present only when true**. Verified against the 2646
packuments in the benchmark recording: 70 files carry it, `sharp` on 171 of its
191 versions, `tailwindcss` on exactly 1 of 2657.

`VersionMetadata` gains it, exactly as `os`, `cpu` and `bin` were added:

```rust
#[serde(default, rename = "hasInstallScript")]
pub has_install_script: bool,
```

The `rename` is spelled out per field because that is what this struct already
does — `optionalDependencies` carries one and there is no `rename_all` — and
`default` is what makes an absent flag read as false, which is how the registry
spells "no install script".

`ResolvedPackage` carries it and **the lockfile records it**, for the reason
`bins`, `supports` and `declared_peers` are recorded: it is a fact about the
publish, and an install that resolves nothing still has to know whether to copy
a package or link it. A lockfile that omitted it would make a cache-hit install
link a package the first install copied, silently reverting the protection in
§3.

### What: the `scripts` block, from the extracted tarball

**The abbreviated packument never carries `scripts`** — zero of 2646 in the
recording. The script bodies are read from the `package.json` inside the store
entry, which is on disk by the time anything needs them. This is what npm does
and it is not a workaround: a script body is a property of the tarball, and
reading it from the bytes that were integrity-checked is stronger than reading
it from metadata that was not.

It also means `hasInstallScript` is a **hint, not a contract**. A package may
carry the flag and have nothing to run once the tarball is opened, or carry
`prepare` and not the flag. The flag decides copy-versus-link — where being
wrong costs a copy — and the extracted `package.json` decides what executes.
Nothing may depend on the two agreeing.

## 6. Running them

### Order

Topological over the resolved graph, leaves first: a package's scripts run only
after every package it depends on is linked **and built**. A build that invokes
a tool from its own dependencies — which is what `node-gyp` in `.bin` is —
needs that tool present and working.

Workspace members run last, after every registry package. The root member runs
last of all, so a root `prepare` sees a finished tree.

Scripts run **after linking**, which is what makes the failure cases in §6.3
what they are: by the time anything can fail, the tree is already on disk.
There is no ordering that avoids this, because a build needs its dependencies
linked in order to run at all.

### Environment

- **cwd** is the package's own directory in the virtual store.
- **`PATH`** is prepended with that package's `node_modules/.bin`.
  `docs/specs/2026-09-16-bin-linking-design.md` §2 anticipated this in as many
  words: it is the reason `.bin` is built per virtual-store entry and not only
  per importer.
- **Shell** is `sh -c`. Every platform jerky supports is unix; there is no
  second spelling to keep in agreement.
- **Variables**: `npm_lifecycle_event` (the script name), `npm_package_name`,
  `npm_package_version`. Deliberately a short list, and the spec says so out
  loud rather than leaving a reader to discover it: npm exports the entire
  flattened manifest and every `npm_config_*`, much of it meaningless outside
  npm, and a package reading `npm_config_registry` from jerky would be getting
  an answer to a question it should not be asking. A package that genuinely
  needs more is a bug report and a decision, not a gap to fill pre-emptively.
- **Output** is captured rather than streamed, and printed only on failure.
  A cold install builds a handful of packages and `node-gyp` is verbose; a
  successful build has nothing to say that is worth burying the install's own
  output under.

### Failure

Two shapes, split by how the package was reached.

**A required dependency, or a workspace member.** The install fails. The error
names the package and shows the tail of the script's captured output. **The
tree is left on disk and the lockfile is not written.**

Leaving the tree is the deliberate half. Deleting a user's `node_modules`
because a build failed is a worse outcome than leaving a half-built one — and
an unwind that fails partway leaves a state worse than the one it was cleaning.
Not writing the lockfile is what keeps the invariant that nothing is recorded
which is not already true on disk: the next install re-resolves and retries
rather than believing a file that describes a tree that never finished.

**An optional dependency.** It is dropped from the tree with a warning, and the
install succeeds. The package is unlinked from whoever declared it, exactly as
`ResolvedGraph::for_platform` already drops a package this machine cannot run,
and anything reachable only through it goes with it.

This is the case `optionalDependencies` was invented for.
`docs/specs/2026-09-16-optional-dependencies-design.md` §1 sets it aside
because "jerky runs no lifecycle scripts and builds nothing, so that failure
cannot occur here at all". It can now. That spec should get a line pointing
here.

**A package reached by both a required and an optional edge takes the required
shape**, and its build failing fails the install. This is not a new rule; it is
`for_platform`'s, which keeps a node reached by one path and skipped on another
because "the first path is a reason to install it and the second is only
permission not to". A build failure is the same question one cause later, and
answering it differently here would mean two rules for when an optional edge
excuses a package.

The two shapes share their mechanism with something that already exists: a
dropped optional dependency is the same graph operation `for_platform`
performs, one cause later. The pass that performs it runs after builds rather
than before them, and that is the whole of the difference.

## 7. Scope

### In scope

- `preinstall`, `install`, `postinstall` for registry dependencies, behind the
  allowlist.
- The synthesized `node-gyp rebuild` for a package with a `binding.gyp`.
- `preinstall`, `install`, `postinstall`, `prepare` for workspace members.
- Copy-instead-of-link for packages carrying `hasInstallScript`.
- `hasInstallScript` through `VersionMetadata`, `ResolvedPackage` and the
  lockfile.
- The allowlist in `package.json`, and the report of what was skipped.
- Both failure shapes.

### Explicitly out of scope

- **A side-effects cache.** §3, and §9.
- **Sandboxing.** An allowlisted script runs with the user's full privileges.
  Making that untrue means a sandbox per platform and is a different project;
  the allowlist is the boundary this spec offers, and it is a boundary on
  *whether* rather than on *what*.
- **`npm_config_*` and the flattened manifest.** §6.
- **Git dependencies**, which are the other place npm runs `prepare`. jerky
  does not resolve them.
- **A general `pre<x>`/`post<x>` chain.** That is the task runner's, #13.
- **Rebuilding on demand** — an `npm rebuild` equivalent. Worth having and not
  needed to make an install correct.

## 8. Testing

The existing `FixtureRegistry` needs to serve a tarball whose `package.json`
carries a `scripts` block and whose packument version carries
`hasInstallScript`; `testing.rs` already builds tarballs with arbitrary
contents, so this is a builder and not a new mechanism.

Claims that must be pinned:

- A dependency with an install script **does not run it** when it is not
  allowlisted, and the install reports it. This is the default and therefore
  the test that matters most.
- An allowlisted script runs, and its effect is visible in the tree.
- A package with an install script is **copied, not linked**: the test asserts
  `st_nlink == 1` on a file in its virtual-store directory, and `> 1` on a file
  belonging to a package without one. This is the §1 hazard, and it is the one
  claim here that a functional test cannot reach.
- A build that writes in place **does not alter the store**. The strongest form
  of the above: a fixture whose `postinstall` truncates one of its own files,
  asserted against the store entry's bytes afterwards.
- Scripts run in dependency order: a fixture where a child's `postinstall`
  writes a file and the parent's asserts it exists.
- A required dependency's failure fails the install, leaves the tree, and
  leaves **no lockfile**.
- An optional dependency's failure drops it, and the install succeeds with the
  rest of the tree intact.
- A member's own scripts run without being allowlisted.
- `node-gyp rebuild` is synthesized for a `binding.gyp` and is **not**
  synthesized when the package declares an `install` of its own.
- At least one test exercises more than one importer, per the invariants — a
  member with a `postinstall` and a root without one.

## 9. Deliberately not done

**The side-effects cache.** §3 argues the key is the problem. What would
reopen it is a measurement: the cost of copying 2.6% of a tree, on a real
fixture, against the cost of building those packages once per project. If
copying turns out to dominate a cold install the trade changes, and the
follow-up issue should carry that measurement as its first task rather than
its conclusion. #126 is the precedent for measuring before optimising here.

**A rebuild command.** `jerky rebuild <pkg>` after a node upgrade. An install
today copies and builds; nothing invalidates a build when the node ABI moves
underneath it, so a user's answer is to delete `node_modules`. That is the same
answer npm gives without `npm rebuild`, and it is bad. It needs to know what a
build depended on, which is the cache-key problem again from the other side.

**Deciding what happens to `"jerky": { "allowedScripts": ... }` when #12
lands.** Deliberately left to #12, which is where the config surface gets
designed. What this spec owes that one is only that the key is namespaced under
`jerky` so it cannot collide.
