# How pnpm makes a package's `bin` executable

**Date:** 2026-09-14
**Status:** Answered
**Question:** #20 — a package's `bin` target may ship non-executable, and a
hard-linked store entry shares its mode with every project. What does pnpm do?
**Blocks:** #20 (link package binaries into `node_modules/.bin`)

## The question

#29 settled which mode bits survive extraction: owner-execute, and nothing
else. That leaves #20 with the harder half.

npm packages routinely ship `bin` targets **without** the executable bit —
`0o644` — and rely on the package manager to fix it at install time. jerky
cannot obviously do that. `populate_virtual_store` hard-links every file out of
the store entry into the project's virtual store, so the file in
`node_modules/.jerky/...` *shares an inode* with the store entry. A `chmod +x`
on it is a `chmod +x` on the store, and therefore on every other project on the
machine holding that package.

The four options considered on #20 were: chmod the store entry anyway; break
the hard link for that one file and chmod the copy; put the bit on the shim
rather than the target; or normalise `bin` targets at extraction time using the
manifest. The initial read on the issue was that chmodding the store entry
should be **rejected outright**. That was wrong, and this note is why.

## Method

Read pnpm's current sources directly rather than its documentation, since the
behaviour in question is not documented and the reasoning is in the code.

## What pnpm does

### Its store key encodes executability

pnpm's store is content-addressed **per file**, and the key carries the mode
class. From `store/cafs/src/getFilePathInCafs.ts`:

```ts
export const modeIsExecutable = (mode: number): boolean => (mode & 0o111) !== 0

const fileType = modeIsExecutable(mode) ? 'exec' : 'nonexec'
// files/ab/cdef…        nonexec
// files/ab/cdef…-exec   exec
```

On write, `store/cafs/src/index.ts` pins executables and leaves the rest to the
umask:

```ts
writeBufferToCafs(buffer, fileDest, isExecutable ? 0o755 : undefined, …)
```

So identical content with different modes becomes two store entries. This is a
fifth option that was not on #20's list — but it is **not** how pnpm solves the
bin problem, because the exec/nonexec split is decided from the *tarball's*
recorded mode. A bin shipped at `0o644` still lands in the `nonexec` slot.

### For bins, it chmods the target through the link

From `crates/cmd-shim/src/link_bins/executable.rs` — note the comment on the
symlinked-executable path, "The target file — not the link — gets its
executable bits raised":

```rust
/// Make the underlying script executable: apply a minimum mode of
/// 0o755 without rewriting CRLF shebangs.
pub(super) fn ensure_target_executable<Sys>(target_path: &Path) -> …
```

and the implementation in `crates/cmd-shim/src/capabilities.rs`:

```rust
let metadata = std::fs::metadata(path)?;
let mode = metadata.permissions().mode() | 0o111;
std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
```

A plain chmod through the hard link, mutating shared store state. pnpm also
tolerates `NotFound` from it, because nothing serializes bin linking across
concurrent installs: another process may remove the package between extraction
and linking, and whoever unlinked it writes an equivalent file and chmods it in
turn.

## Why the rejection was wrong

Two reasons. The second is the one that decides it.

**The mutation is monotonic.** `| 0o111` only ever *adds* execute bits. The
worst outcome is a file carrying `+x` it did not need, which is harmless. The
opposite failure — a bin without `+x` — breaks the tool outright. The asymmetry
runs in favour of chmodding, and it is not a security question: adding execute
to a file that is already world-readable grants nothing that reading it did
not.

**jerky's blast radius is much smaller than pnpm's.** This is the decisive
difference and it runs opposite to how it first looks.

| | pnpm | jerky |
| --- | --- | --- |
| Store key | one file's content hash | the package tarball's integrity |
| Entry contents | a single file | the whole package tree |
| Who shares an entry | *any* package with byte-identical content | only the same package at the same version |

In pnpm, an unrelated package that happens to ship a byte-identical file shares
the entry, so chmodding a bin can put `+x` on content that is not a bin at all.
In jerky, everyone sharing an entry has the same package, the same version and
the same manifest — so they all declare that same `bin` and all want it
executable. The mutation is not shared state being violated; it is a property
of the package that every consumer already agrees on.

So chmodding the target is **better justified in jerky than in the tool that
ships it**. Breaking the hard link to chmod a copy buys nothing and costs the
store's whole purpose for that file.

## Consequences for #20

- Chmod the target. Do not copy-on-link, and do not put the bit on the shim.
- Tolerate `NotFound`, for the reason pnpm gives: nothing serializes bin
  linking between concurrent installs.
- Raise bits rather than setting a mode — `| 0o111`, not `= 0o755` — so the
  operation stays monotonic and idempotent, and two racing installs cannot
  disagree about the result.
- `files_are_hard_linked_from_the_store_not_copied` stays true, unqualified.

## A divergence this surfaced in #29

pnpm treats **any** execute bit as executable, `mode & 0o111`. jerky's
`normalise_mode` checks owner-execute only, `mode & 0o100`. A tarball recording
group- or other-execute without owner-execute therefore reads as executable to
pnpm and non-executable to jerky.

This is rare, and jerky's choice is the conservative one — but it was picked by
default rather than decided. It is worth settling deliberately when #20 lands,
since #20 is what makes the executable bit load-bearing.

## Sources

Read at `pnpm/pnpm@main`, 2026-09-14:

- `pnpm11/store/cafs/src/getFilePathInCafs.ts` — the exec/nonexec key split
- `pnpm11/store/cafs/src/index.ts` — `isExecutable ? 0o755 : undefined` on write
- `pnpm/crates/cmd-shim/src/link_bins/executable.rs` — bin materialization
- `pnpm/crates/cmd-shim/src/capabilities.rs` — `ensure_executable_bits`
