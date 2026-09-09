# Package Manager Spine — Design

**Date:** 2026-08-24
**Status:** Approved
**Scope:** Spec 1 of 3 for jerky's package-manager spine
**Issues:** #3, #4 (this spec); reshapes #5, #6, #7, #11

## 1. Context

jerky is a Rust CLI that fuses two products: an npm-compatible package
manager and a monorepo build/task orchestrator. The 16 open issues describe
both. This spec covers only the first slice of the package-manager half.

At the time of writing the repository is greenfield: `src/main.rs` is
hello-world and `Cargo.toml` has no dependencies.

## 2. Settled decisions

These were decided during brainstorming and constrain everything below.

**Ambition: a real, usable tool.** jerky should install real-world dependency
trees correctly. Consequences: use mature crates rather than hand-rolling,
full transitive resolution (spec 2), integrity verification from day one, and
lifecycle scripts eventually.

**Layout: the pnpm paradigm from day one.** A global content-addressed store,
hard-linked into a per-project virtual store, with direct dependencies
symlinked into `node_modules/`.

This resolves a contradiction between issues #3 ("a local node_modules
directory") and #7 ("pnpm paradigm"). The installer's write path is its core,
so building the npm-hoisted layout first would mean rewriting it later. The
pnpm layout is also *easier* to implement correctly: npm-style hoisting needs
a placement algorithm that hoists each package as high as possible and nests
on conflict, whereas the virtual store layout is a direct mechanical function
of the resolved graph.

This choice also eliminates the hardest part of #6. The difficult half of
semver support is cross-tree conflict resolution — deciding who wins when two
packages need incompatible versions of the same dependency. In a virtual
store both versions coexist as separate directories and each dependent links
to the one it asked for. The conflict never arises.

**Slicing: walking skeleton first.** Spec 1 proves the plumbing — registry
client, integrity, extraction, store, linking, CLI, errors, test harness —
against a case verifiable by eye. Spec 2 adds the resolver on top of working
infrastructure.

**Platforms: Linux and macOS.** Both are Unix, so the linking code is shared.
Two real differences matter: hard links fail with `EXDEV` across filesystems
and need a copy fallback, and macOS filesystems are case-insensitive by
default, which constrains store key encoding. Windows is out of scope; the
`linker` module is the only place that would need to change.

**I/O: synchronous, behind a trait.** Spec 1 makes exactly two HTTP calls, so
`tokio` would be ceremony without benefit. `ureq` behind a `RegistryClient`
trait. The trait matters more than the client — it is what makes offline,
deterministic tests possible.

Spec 2's parallel downloads are I/O-bound network fan-out across tens of
packages; a bounded thread pool with synchronous `ureq` handles that well.
Async pays off at thousands of concurrent connections, which is not this
workload. The honest cost of this choice is that migrating to async later is
viral through the call stack.

**Store granularity: package-level, not file-level.** The store key is the
`sha512` from the registry's `dist.integrity`, which we already fetch and
verify. Each package version is one directory, hard-linked wholesale.

Real pnpm does file-level content addressing, which dedupes identical files
across versions and is better for disk usage, but requires per-file hashing
and a per-package index. Unlike the `node_modules` layout, store granularity
is entirely behind the `linker` module and invisible outside it, so upgrading
later means repopulating the store rather than redesigning anything. The
`v1/` path segment exists to make that upgrade contained.

## 3. Scope

### In scope

- `jerky init` — write a default `package.json`
- `jerky install <name>` — install the `latest` version, pinned exactly
- `jerky install <name>@<version>` — install an exact version
- Registry metadata fetch, integrity verification, tarball extraction
- Content-addressed store with atomic commit
- Hard-link and symlink wiring of the virtual store
- Recording the resolved dependency in `package.json`

The target case is a package with no dependencies of its own, e.g.
`jerky install lodash`.

### Explicitly out of scope

Each of these is deferred to a named later spec, not forgotten.

| Deferred | Goes to |
|---|---|
| Transitive dependency resolution | Spec 2 |
| Semver range parsing and selection | Spec 2 |
| `jerky.lock` | Spec 2 |
| Bare `jerky install` from the manifest | Spec 3 |
| `devDependencies` / `--save-dev` | Spec 3 |
| Scoped packages (`@scope/name`) | Spec 3 |
| `node_modules/.bin` linking | Spec 3 (blocks #8) |
| Lifecycle scripts (`preinstall`/`install`/`postinstall`) | Later |
| `.npmrc`, auth, alternate registries | Later |
| Peer and optional dependencies | Later |
| Parallel downloads | Spec 2 |
| Windows support | Later |
| Interactive `jerky init` prompts | Not planned |

**`jerky.lock` is deliberately absent from spec 1.** Its purpose is to make a
later install reproduce an earlier resolution, but spec 1 pins exact versions
into `package.json` and has no ranges and no graph, so reproducibility is
already fully determined by the manifest. Nothing in spec 1 could read a
lockfile back — bare `jerky install` is spec 3 — so it would be written and
never consumed, untestable beyond asserting it exists.

More importantly, a lockfile's format is the serialization of the resolver's
output type. Designing it now, against a case with one entry and zero edges,
means designing it against the input that exercises none of its structure. A
lockfile format that changes after real projects have committed one needs a
version field and a migration path. It belongs in spec 2 alongside the
resolver, as one design rather than two.

The gap this accepts: without a lockfile there is no trust-on-first-use
anchor, so a republished or tampered tarball for an already-pinned version
would go unnoticed. Spec 2 closes it. Spec 1 requires no structural
accommodation to keep the door open — the integrity and tarball URL already
flow through `VersionMetadata` at the exact point a lockfile writer would
hook in.

## 4. Commands

### `jerky init`

Non-interactive. Writes `package.json` with `name` (derived from the
directory name), `version` (`"1.0.0"`), `main` (`"index.js"`), `license`
(`"ISC"`), and an empty `scripts` object. Errors if `package.json` already
exists rather than prompting or clobbering.

`npm init`'s interactive questionnaire is a separate concern and issue #4
does not ask for it.

### `jerky install <spec>`

```
jerky install lodash            -> resolve "latest", pin the concrete version
jerky install lodash@4.17.21    -> exact version
```

Note that issue #5 writes this as `<pkg>@v<x.x.x>`. That is not npm
convention; there is no `v` prefix. The rewritten issue should say
`pkg@1.2.3`.

## 5. Architecture

### Module layout

A lib + bin split. `src/main.rs` stays thin; everything real lives in the lib.
This is not ceremony: Rust integration tests in `tests/` can only link
against a lib target, so a bin-only crate would force every integration test
to shell out to the compiled binary.

```
src/
  main.rs         thin: parse args, call run(), map errors to exit codes
  lib.rs
  cli.rs          clap derive definitions and spec parsing
  commands/
    init.rs
    install.rs
  manifest.rs     package.json read/write
  registry.rs     RegistryClient trait + HttpRegistry (ureq)
  integrity.rs    SSRI parsing and verification
  archive.rs      gzip + tar extraction
  store.rs        content-addressed store
  linker.rs       hard links and symlinks
  error.rs        error types
```

Dependency direction is strictly one-way. `commands` orchestrates and is the
only module aware of more than one of the others. `integrity`, `archive`,
`manifest` and `cli` are leaves that touch no network and no global state.

### Crates

`clap` (derive), `serde` + `serde_json` (with `preserve_order`), `ureq`,
`flate2`, `tar`, `sha2`, `base64`, `dirs`, `thiserror`, `tempfile`.

### Path scheme

```
~/.jerky/store/v1/sha512-<hex>/
    global content-addressed store

<project>/node_modules/.jerky/lodash@4.17.21/node_modules/lodash/
    virtual store; hard links to the store entry

<project>/node_modules/lodash -> .jerky/lodash@4.17.21/node_modules/lodash
    relative symlink for each direct dependency
```

The doubled `node_modules` in the virtual store path is the mechanism, not an
accident. Node resolves a package's own dependencies by walking *up* from its
directory looking for a `node_modules`. Placing each package one level inside
its own `node_modules` means its siblings in that directory are exactly its
declared dependencies and nothing else — which makes phantom dependencies
structurally impossible rather than merely discouraged, and makes spec 2's
transitive wiring nearly free.

**Store keys are lowercase hex, not base64.** The registry reports integrity
as base64 (`sha512-Ab3x...`), but macOS filesystems are case-insensitive by
default, so base64 keys can collide. Hex-encoding the raw digest avoids it.

**Symlinks are relative**, so moving or copying a project does not break every
link. The target carries no leading `../`: it is resolved relative to
`node_modules/`, the directory holding the link. pnpm uses `../` only for
links *inside* the virtual store, which sit one level deeper.

## 6. Data flow and types

```rust
// cli.rs
enum VersionSpec { Latest, Exact(String) }
struct PackageSpec { name: String, version: VersionSpec }

// registry.rs
trait RegistryClient {
    fn version_metadata(&self, name: &str, version: &str) -> Result<VersionMetadata>;
    fn fetch_tarball(&self, url: &str) -> Result<Vec<u8>>;
}
struct VersionMetadata { name: String, version: String, dist: Dist }
struct Dist { tarball: String, integrity: Option<String>, shasum: Option<String> }

// integrity.rs
enum Algo { Sha512, Sha1 }
struct Integrity { algo: Algo, digest: Vec<u8> }   // .store_key() -> lowercase hex
```

Flow:

```
PackageSpec
  -> registry.version_metadata()            GET /{name}/{version-or-tag}   (1)
       -> Dist { tarball, integrity }
            -> Integrity::parse()                                          (2)
                 -> store.contains(key)? --yes--> skip to link
                      --no--> fetch_tarball() -> Vec<u8>
                              -> verify(bytes, integrity)                  (3)
                                   -> archive::extract()                   (4)
                                        -> store.commit()  (stage + rename)
                                             -> linker::hard_link_tree()
                                                  -> linker::symlink()
                                                       -> manifest.add_dependency()  (5)
```

**(1) One version, not the whole packument.** `GET /{name}/{version}` returns
a single version's manifest; the full packument for a popular package is
megabytes of every version ever published. The same endpoint resolves
dist-tags, so `VersionSpec::Latest` sends the literal string `"latest"` down
the identical path — no second code path.

A consequence worth a test: `jerky install react@next` works through the same
mechanism. Treat it as a happy accident rather than an advertised feature,
but test it so we notice if it breaks.

Spec 2 will need the full packument for range resolution and should send
`Accept: application/vnd.npm.install-v1+json` for the abbreviated form.

**(2) Integrity may be missing.** Packages published before roughly 2017 carry
only a legacy `shasum` (sha1). Prefer `integrity`; fall back to `shasum` as
sha1; error if neither is present. This is why `Algo` is an enum.

**(3) Buffer, then verify, then extract — in that order.** Streaming the
download through gunzip into the tar extractor while hashing would use
constant memory and is more elegant, but by the time a hash mismatch is
detected, attacker-controlled files are already on disk. Verifying a complete
buffer before a single byte is extracted is what makes the integrity check
mean something. npm tarballs are small enough that the memory cost is fine.

**(4) Extraction is hostile-input handling.** The tar is untrusted. `archive`
must strip the leading `package/` component that npm tarballs carry, and
reject any entry whose resolved path escapes the destination — `../`
components, absolute paths, or symlinks pointing outside the tree. This is
the tar-slip class of bug and the one place in spec 1 where a mistake is a
security hole rather than a crash.

**(5) The recorded version comes from the response, never the request.**
`VersionMetadata.version` is concrete, so `jerky install lodash` resolves
`latest` to `4.17.21` and writes `"lodash": "4.17.21"`. Resolution happens
exactly once; we never re-query to discover what we installed.

**Record the exact version, not a caret range.** npm would write `^4.17.21`
here. jerky should not yet: spec 1 has no range resolution, so a caret would
put a constraint in `package.json` that the tool cannot honour on the next
install — the file would claim more than jerky can do. Switch to caret in
spec 2, when ranges actually resolve.

**Scoped name parsing is required now, even though scoped installs are not.**
Splitting on `@` naively breaks `@types/node`, and breaks it *quietly*,
yielding an empty name and a version of `types/node`. The rule: if the name
starts with `@`, look for the separator after index 0; otherwise take the
first `@`. Three lines in `cli.rs`, and it means scoped support in spec 3 is
a store and linker question rather than a parser bug hunt.

## 7. Store

Population is atomic: extract into a staging directory, then `rename` into
the final store path. A killed process leaves a stray staging directory
rather than a half-extracted package.

This matters because the store is content-addressed, so the existence check
is "does `~/.jerky/store/v1/sha512-<hex>/` exist?". A partial directory at that path
means "present and verified" to every subsequent install, which would then
hard-link a truncated package into a project. That is silent corruption which
persists until someone clears the store manually, with a symptom — a module
missing half its files — that looks nothing like its cause.

**The staging directory must live inside the store root:**

```
~/.jerky/store/v1/.staging/<random>/     extract here
~/.jerky/store/v1/sha512-<hex>/          rename here
```

`rename(2)` is atomic only within a single filesystem and fails with `EXDEV`
across one. `tempfile`'s default location is `TMPDIR`, which on most Linux
systems is a separate tmpfs from `$HOME`, so the default would fail on the
target platforms. Use `tempfile::TempDir::new_in(store_root)`. The location
is a correctness requirement, not a default to accept.

**Concurrency needs no locking.** Two `jerky install` processes racing on the
same package each extract to their own staging directory and both attempt the
rename. Directory rename onto a non-empty existing directory fails rather
than clobbering, so the loser sees `ENOTEMPTY`/`EEXIST`, concludes the entry
now exists, and drops its staging copy, which `TempDir` cleans up on drop.
The filesystem arbitrates.

**A package already in the store is a no-op** — no metadata fetch beyond
resolution, no download.

## 8. Linking

Two distinct operations, only the first of which is a hard link.

**Store to virtual store: hard links, one per file.** Directories cannot be
hard-linked, so `hard_link_tree` walks the store entry, creating directories
and calling `fs::hard_link` on each regular file.

**Virtual store to `node_modules`: one relative symlink** per direct
dependency.

**`linker` owns the `EXDEV` fallback.** The store and a project can legitimately
live on different filesystems, so a failed `hard_link` falls back to copying.
Every other module stays unaware.

Note the deliberate asymmetry with section 7: `staging -> store` never crosses
a filesystem by construction, so `EXDEV` there indicates a bug in path
construction and must not be silently handled. The same errno gets opposite
treatment in the two places.

**Linking uses the same stage-and-rename discipline as the store.** A
half-linked tree in `node_modules/.jerky/` is the same hazard as a
half-extracted store entry — it looks present. Build the tree at
`.jerky/.staging/<random>/` and rename into `.jerky/<name>@<version>/`.

## 9. Error handling

**Typed errors throughout the lib; `anyhow` only in `main.rs`.** Each module
owns a `thiserror` enum and a top-level `JerkyError` wraps them with
`#[from]`. `anyhow` inside the lib would erase the type information tests need
— `matches!(err, IntegrityError::Mismatch { .. })` is the difference between
proving we rejected a bad tarball and proving we merely failed.

| Module | Variants |
|---|---|
| `registry` | `PackageNotFound`, `VersionNotFound`, `Network`, `MalformedResponse` |
| `integrity` | `Missing`, `Unparseable`, `UnsupportedAlgorithm`, `Mismatch { expected, actual }` |
| `archive` | `Gzip`, `Tar`, `UnsafePath { entry }` |
| `store` | `StagingFailed`, `CommitFailed` |
| `linker` | `Io`, `ConflictingEntry` |
| `manifest` | `NotFound`, `Malformed`, `AlreadyExists` |

`PackageNotFound` and `VersionNotFound` are distinct because they are the same
HTTP 404 but different user errors — "no such package" versus "that package
exists, that version does not" — and the second should ideally list the
versions that do exist.

### State left on disk when a stage fails

- **Before store commit** — nothing. Staging is dropped, the store untouched.
  Integrity mismatch lands here by construction.
- **During linking** — the store entry is already valid and stays reusable;
  the staged link tree is dropped.
- **After linking, before the manifest write** — the package is on disk but
  absent from `package.json`. This is the one accepted inconsistency, and it
  is why **the manifest write goes last**. An installed-but-unrecorded package
  is harmless and self-heals on rerun; the reverse is the tool lying about
  state it controls.

**Principle:** never record something in the manifest that is not already true
on disk.

**Integrity mismatch is loud and non-retryable.** It is corruption or
tampering, so the command fails with expected and actual digests printed, and
never falls back to installing anyway.

**Retries are narrow.** Transport errors and 5xx responses on metadata and
tarball GETs get three attempts with backoff; they are idempotent reads and
transient failures are common enough that not retrying would feel broken. 404s
and integrity failures get zero retries — retrying a definite answer only
makes the tool slow at being wrong.

**Messages carry context.** Errors chain via `#[source]` and `main.rs` renders
the chain, so the user sees `failed to install lodash@4.17.21` above
`integrity check failed: expected sha512-..., got sha512-...` rather than a
bare `Io(NotFound)` with no indication which of a dozen file operations
produced it.

**Exit codes:** `0` success, `1` any failure, clap's own `2` for usage errors.

## 10. Testing

Implementation follows TDD: each behaviour below gets its failing test before
the code.

### Layers

**Unit tests on leaf modules.** `integrity`, `archive`, `manifest` and the
spec parser are pure functions over bytes and strings.

**Integration tests through a fake registry.** `FixtureRegistry` implements
`RegistryClient` from a map of manifests and tarball bytes, so the whole
install pipeline runs end to end offline and deterministically. This is what
the trait is for.

**The gap that creates, stated explicitly:** the fake tests everything except
the module it replaces. `HttpRegistry` — URL construction, the `Accept`
header, deserialization of real registry responses, 404-versus-5xx handling,
retry behaviour — gets no coverage from it. That module needs its own tests
against a local HTTP server, plus a few `#[ignore]`d tests hitting the real
registry, run on a schedule rather than per-PR so npm having a bad afternoon
does not redden every pull request.

### Fixtures are generated, not committed

Build tarballs in-test with `tar` and `flate2`, hash the generated bytes, and
hand that digest to the fake registry, so the happy path is self-consistent by
construction and the mismatch test corrupts one byte. This also allows
*malicious* fixtures — a tar entry named `../../evil`, an absolute path, a
symlink escaping the tree — which are the only way to test `archive`'s
hostile-input handling and which could not reasonably be committed as
binaries.

### Filesystem isolation is an API constraint

Nothing below `main.rs` may read `$HOME` or the current directory.
`Store::new(root)` and the command functions take their paths as parameters.
A module reaching for a global path makes tests collide with each other or
scribble on a developer's real `~/.jerky`. This is a rule, not advice.

### Assertions worth naming

- **The hard link is really a hard link.** Compare `(dev, ino)` pairs between
  the store file and the virtual store file — both, since inode numbers are
  only unique within a filesystem. Without this, a silent regression to
  copying passes every other test: contents match, the symlink resolves, Node
  can require the module, the install "works", and the only thing lost is the
  entire reason the store exists. The test harness must place the store root
  and project root under one temp directory, or the legitimate `EXDEV` copy
  fallback would make the inodes differ correctly.
- **Idempotence, counted.** A second install is a no-op *and* the fake
  registry's call count does not increase, proving the store-hit path was
  taken rather than a redundant download producing the same result.
- **Integrity mismatch leaves the store empty** — assert the store has no
  entry afterwards, not merely that an error was returned.
- **Tar-slip writes nothing outside the destination** — assert the error
  variant *and* that the escape target does not exist.
- **`package.json` round-trips losslessly.** Start from a manifest with
  unknown fields in a specific key order; assert everything is byte-identical
  except the added `dependencies` entry. This catches `preserve_order` being
  dropped from the `serde_json` features, which otherwise silently reshuffles
  the user's file.
- **404 discrimination** — `PackageNotFound` versus `VersionNotFound`.
- **Dist-tag resolution** — `pkg@next` resolves through the same path.
- **`jerky init` refuses to overwrite** an existing `package.json`.

### Acknowledged coverage gap

The `EXDEV` copy fallback in `linker`. Creating a second filesystem in CI is
not practical, so the copy path is tested by calling it directly and the
"triggers on the right errno" branch is covered by inspection only.

### CI

GitHub Actions on `ubuntu-latest` and `macos-latest`, running `cargo test`,
`cargo clippy -- -D warnings`, and `cargo fmt --check`. The macOS leg is what
would actually catch a case-insensitivity bug in store keys, which is
invisible on Linux.

## 11. Roadmap and issue remapping

**Spec 2 — the resolver.** `jerky install express`: npm range parsing,
transitive walk, virtual store wiring for nested dependencies, and
`jerky.lock`. Designed in
`docs/superpowers/specs/2026-09-08-resolver-and-lockfile-design.md`. Parallel
downloads were dropped from that slice: spec 2 is already a resolver plus a
lockfile format, and both are worth getting right before making fast.

**Spec 3 — everyday use.** Bare `jerky install` from the manifest,
`devDependencies`, scoped packages, and `node_modules/.bin` linking.

### Effect on existing issues

- **#3** absorbs #5 and #7 and becomes this spec's install command.
- **#4** is unchanged in intent; clarify that it is non-interactive.
- **#5** collapses into a CLI parsing case; correct the `@v<x.x.x>` syntax.
- **#6** loses its hard half to the layout choice; what remains is range
  parsing and max-satisfying selection, which belongs to spec 2.
- **#7** disappears into #3 rather than being a follow-up.
- **#11** moves to spec 2 and should specify deterministic key ordering,
  minimal diff noise, and a format version field — "text based" alone
  provides none of those.

### Issues that need filing

Bare `jerky install`; `node_modules/.bin` linking (a silent prerequisite of
#8, which calls swc); `devDependencies` support; lifecycle scripts; scoped
package support.

Two candidates were dropped on review. Integrity verification does not need
its own issue — it is part of this spec and therefore of the rewritten #3. A
global tarball cache is subsumed by the store: because store keys are the
tarball integrity hash, a package already in the store is never re-downloaded,
so a separate cache would only help after the store is cleared. If that
becomes a real need it is an offline-mode feature, not a cache.

## 12. Open questions

**~~An npm-flavoured semver crate for spec 2.~~ Answered 2026-09-08.** Use
`js-semver`. Differential testing against npm's own `semver` 7.8.5 over 531
ranges put it at 531/531, against 515 for `nodejs-semver` and 431 for Cargo's
`semver`. The hypothesis that the Cargo crate is unusable is confirmed and is
worse than expected: 45 of its 100 failures are silent wrong answers on
prerelease ranges rather than refusals to parse. Full data, the residual risk
that `js-semver` is pre-1.0, and the reproducible harness are in
`docs/superpowers/research/2026-09-08-npm-semver-crate-selection.md`.

**File-level content addressing.** Whether to upgrade the store from
package-level to file-level CAS, and when. The `v1/` path segment keeps this
open.

**`package.json` reformatting.** `Manifest::save` re-serializes with
`serde_json::to_string_pretty`, which always emits two-space indentation and
expands arrays one element per line. npm detects and preserves the file's
existing indentation. A project using tabs or four spaces will see its
`package.json` reformatted on first install. Key order and unknown fields are
already preserved, so this is purely about whitespace. Not blocking for spec 1;
tracked as #24.
