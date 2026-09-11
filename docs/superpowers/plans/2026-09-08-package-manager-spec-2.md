# Package Manager Spec 2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Resolve npm ranges and transitive dependency trees, install every project in a workspace from its root, and record the result in a deterministic `jerky-lock.json`.

**Architecture:** Four new modules on top of spec 1. `range` wraps `js-semver` and is the only module that names it, so the 0.4.0 dependency is swappable in one file. `workspace` discovers members and answers "which importer am I in". `resolver` is pure given a `RegistryClient` — it fetches metadata and returns a `ResolvedGraph`, writing nothing to disk, which is what makes it testable with no filesystem at all. `lockfile` is a straight serialization of that graph, which is why the two were designed together. `commands/install` grows from installing one package to installing a workspace.

**Tech Stack:** Rust 2024, plus `js-semver` and a glob matcher. Everything else is already in the tree.

**Spec:** `docs/superpowers/specs/2026-09-08-resolver-and-lockfile-design.md`
**Workspaces:** `docs/superpowers/specs/2026-09-10-workspace-design.md`
**Research:** `docs/superpowers/research/2026-09-08-npm-semver-crate-selection.md`

> **Revised 2026-09-11.** Tasks 1 through 3 are done and unchanged — `range`,
> packument fetching, and the resolver are all workspace-agnostic. Tasks 4
> onwards are revised against the workspace design, and the numbering shifted:
> a new Task 5 covers workspace discovery, so the old Tasks 5, 6 and 7 are now
> 6, 7 and 8. Task 4 is already implemented in its single-project form and is
> now a revision rather than new work.

## Global Constraints

Spec 1's constraints all still hold. These are the ones this spec adds or sharpens.

- **Nothing outside `src/range.rs` may name `js_semver`.** This is the containment boundary for a 0.4.0 dependency and the reason a future swap is one file. A `use js_semver::` anywhere else is a bug, not a shortcut.
- **`BTreeMap`, never `HashMap`, anywhere that reaches the lockfile.** Iteration order is serialization order, and two machines resolving the same tree must produce byte-identical files. Sorted-by-construction is how that is guaranteed rather than remembered.
- **A workspace of one goes through the same code path as a workspace of many.** A `package.json` with no `workspaces` field is a workspace with a single importer keyed `.`. There is no single-project branch to keep working, because the common case is the degenerate case of the general one. A test that only ever exercises one importer is not exercising this.
- **Only `main.rs` locates the workspace root.** It is the one place allowed to read the current directory, so walking up for the root manifest happens there and every path below is a parameter. This is the spec 1 constraint restated for a new kind of path, and it is what keeps the workspace tests free of a real filesystem layout.
- **Symlink targets are computed from the importer's depth, never hardcoded.** A link in the root's `node_modules` needs no `../`; one in `packages/ui/node_modules` needs three to reach the root's virtual store. Any literal `../` outside the function that computes depth is a bug waiting for a nested importer.
- **Never follow a dependency's `devDependencies`.** Only the root project's, and that is spec 3. This is a correctness requirement, not an optimisation: following them pulls in most of the registry.
- **Prereleases are excluded unless the range mentions one.** This is npm's rule and the exact place Cargo's semver crate diverges silently.
- **The resolver performs no I/O beyond the `RegistryClient`.** No filesystem, no `$HOME`, no cwd.
- **Nothing is recorded that is not already true on disk.** The manifest and the lockfile are both written after every package is linked.
- **Every task ends with `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` passing.**

---

## File Structure

| File | Responsibility |
|---|---|
| `src/range.rs` | jerky's `Range`/`Version`, wrapping `js-semver`. The only file that names it. |
| `src/workspace.rs` | Member discovery from `workspaces`, and importer lookup |
| `src/resolver.rs` | The transitive walk. Produces a `ResolvedGraph`. |
| `src/lockfile.rs` | `ResolvedGraph` ⟷ `jerky-lock.json` |
| `src/registry.rs` | *(modify)* `Packument`, `RegistryClient::packument` |
| `src/linker.rs` | *(modify)* intra-virtual-store symlinks, which need `../` |
| `src/commands/install.rs` | *(modify)* install a graph rather than one package |
| `src/testing.rs` | *(modify)* fixture packuments, tree builder |
| `tests/semver_conformance.rs` | The guard on the `js-semver` dependency |
| `tests/fixtures/semver-oracle.json` | Committed ground truth from npm's semver 7.8.5 |
| `tests/resolve.rs` | Tree-shape tests: diamond, conflict, cycle, chain |
| `tests/lockfile.rs` | Determinism, diff noise, version refusal, round trip |
| `tests/workspace.rs` | Discovery, globs, importer lookup, malformed members |

---

### Task 1: The `range` module and its conformance suite

**Files:**
- Create: `src/range.rs`, `tests/semver_conformance.rs`, `tests/fixtures/semver-oracle.json`
- Modify: `Cargo.toml`, `src/lib.rs`, `src/error.rs`
- Test: inline `#[cfg(test)]` in `src/range.rs`, plus the conformance integration test

**Interfaces:**
- Consumes: nothing
- Produces: `range::Version`, `range::Range`, `Range::parse(&str)`, `Range::matches(&Version)`, `Range::max_satisfying<'v>(&self, &'v [Version]) -> Option<&'v Version>`, `Version::parse(&str)`, `Version::as_str(&self)`, `range::RangeError`

- [ ] **Step 1: Add the dependency**

```bash
cargo add js-semver
```

- [ ] **Step 2: Generate and commit the oracle**

The conformance suite's ground truth is committed, not generated at test time — CI must not depend on npm being reachable. Regenerate it with the script in the research note, run in a directory where `jerky install semver` has been run, then:

```bash
mkdir -p tests/fixtures
cp oracle.json tests/fixtures/semver-oracle.json
```

The file records, for each of 531 ranges: the range string, whether npm considers it valid, and the exact list of satisfying versions from a fixed 31-version set.

- [ ] **Step 3: Write the failing unit tests**

Create `src/range.rs` with only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn versions(list: &[&str]) -> Vec<Version> {
        list.iter().map(|v| Version::parse(v).unwrap()).collect()
    }

    #[test]
    fn caret_allows_minor_and_patch_on_one_x() {
        let r = Range::parse("^1.2.3").unwrap();
        assert!(r.matches(&Version::parse("1.9.9").unwrap()));
        assert!(!r.matches(&Version::parse("2.0.0").unwrap()));
    }

    #[test]
    fn caret_on_zero_x_is_narrower() {
        // npm's rule: ^0.2.3 allows 0.2.x but not 0.3.0. This is one of the
        // places Cargo's crate disagrees, so it is worth pinning here rather
        // than only in the conformance suite.
        let r = Range::parse("^0.2.3").unwrap();
        assert!(r.matches(&Version::parse("0.2.4").unwrap()));
        assert!(!r.matches(&Version::parse("0.3.0").unwrap()));
    }

    #[test]
    fn max_satisfying_picks_the_highest_not_the_first() {
        let r = Range::parse("^1.0.0").unwrap();
        let vs = versions(&["1.0.0", "1.9.9", "1.2.3", "2.0.0"]);
        assert_eq!(r.max_satisfying(&vs).unwrap().as_str(), "1.9.9");
    }

    #[test]
    fn max_satisfying_is_none_when_nothing_matches() {
        let r = Range::parse("^3.0.0").unwrap();
        assert!(r.max_satisfying(&versions(&["1.0.0", "2.0.0"])).is_none());
    }

    #[test]
    fn prereleases_are_excluded_unless_the_range_asks() {
        let plain = Range::parse("^1.0.0").unwrap();
        assert!(!plain.matches(&Version::parse("1.2.3-beta.1").unwrap()));

        let asks = Range::parse("^1.2.3-beta.1").unwrap();
        assert!(asks.matches(&Version::parse("1.2.3-beta.2").unwrap()));
    }

    #[test]
    fn parses_the_syntax_cargo_refuses() {
        // Every one of these is rejected outright by Cargo's semver crate.
        for r in ["1.2.3 - 2.3.4", ">=1.2.3 <2.0.0", "1.2.3 || >=3.0.0", "1.2.x", "*"] {
            assert!(Range::parse(r).is_ok(), "{r} should parse");
        }
    }

    #[test]
    fn rejects_nonsense() {
        assert!(matches!(Range::parse("not a range"), Err(RangeError::Unparseable(_))));
    }

    #[test]
    fn versions_order_by_precedence_with_prereleases_lower() {
        let mut vs = versions(&["1.0.0", "1.0.0-alpha", "0.9.9"]);
        vs.sort();
        let ordered: Vec<_> = vs.iter().map(|v| v.as_str()).collect();
        assert_eq!(ordered, ["0.9.9", "1.0.0-alpha", "1.0.0"]);
    }
}
```

- [ ] **Step 4: Run the test to verify it fails**

Run: `cargo test --lib range`
Expected: FAIL — compile error, `Range` not found.

- [ ] **Step 5: Write the implementation**

Prepend to `src/range.rs`. Note that `Version` needs `Ord` so callers can sort and take a maximum without knowing what backs it.

```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RangeError {
    #[error("could not parse version `{0}`")]
    BadVersion(String),
    #[error("could not parse range `{0}`")]
    Unparseable(String),
}

/// A concrete semantic version.
///
/// Wraps `js-semver` so nothing else in jerky names it. See
/// `docs/superpowers/research/2026-09-08-npm-semver-crate-selection.md` for
/// why that crate and what replacing it would take.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(js_semver::Version);

impl Version {
    pub fn parse(input: &str) -> Result<Self, RangeError> {
        js_semver::Version::parse(input)
            .map(Version)
            .map_err(|_| RangeError::BadVersion(input.to_string()))
    }

    pub fn as_str(&self) -> &str {
        // Held as the original string so the registry's exact spelling
        // survives into `package.json` and the lockfile unchanged.
        self.0.as_str()
    }
}

/// An npm version range.
#[derive(Debug, Clone)]
pub struct Range(js_semver::Range);

impl Range {
    pub fn parse(input: &str) -> Result<Self, RangeError> {
        js_semver::Range::parse(input)
            .map(Range)
            .map_err(|_| RangeError::Unparseable(input.to_string()))
    }

    pub fn matches(&self, version: &Version) -> bool {
        self.0.satisfies(&version.0)
    }

    /// The highest published version satisfying this range, matching npm's
    /// selection rule. `None` means the range is unsatisfiable against this
    /// list, which is the caller's cue to report the available versions.
    pub fn max_satisfying<'v>(&self, versions: &'v [Version]) -> Option<&'v Version> {
        versions.iter().filter(|v| self.matches(v)).max()
    }
}
```

If `js_semver::Version` does not expose `as_str` or implement `Ord`, keep the
original string alongside it in the struct and implement `Ord` by delegating to
the crate's comparison. Do not reach for string comparison — `1.10.0` sorts
below `1.9.0` lexically.

- [ ] **Step 6: Write the conformance suite**

Create `tests/semver_conformance.rs`. This is the guard on the dependency: it is
what makes replacing `js-semver` a single test run rather than a repeat of the
original investigation.

```rust
//! Differential conformance against npm's own semver 7.8.5.
//!
//! The oracle is committed rather than generated, so this runs offline. See
//! docs/superpowers/research/2026-09-08-npm-semver-crate-selection.md for how
//! it was produced and why this suite exists.

use jerky::range::{Range, Version};
use serde::Deserialize;

#[derive(Deserialize)]
struct Case {
    range: String,
    valid: bool,
    satisfying: Vec<String>,
}

#[test]
fn matches_npm_semver_on_every_recorded_case() {
    let raw = include_str!("fixtures/semver-oracle.json");
    let cases: Vec<Case> = serde_json::from_str(raw).unwrap();
    assert!(cases.len() > 500, "oracle looks truncated");

    let versions: Vec<Version> = VERSIONS.iter().map(|v| Version::parse(v).unwrap()).collect();
    let mut failures = Vec::new();

    for case in &cases {
        match (Range::parse(&case.range), case.valid) {
            (Err(_), false) => {}
            (Err(_), true) => failures.push(format!("{:?}: refused a range npm accepts", case.range)),
            (Ok(_), false) => failures.push(format!("{:?}: accepted a range npm rejects", case.range)),
            (Ok(r), true) => {
                let got: Vec<&str> = versions
                    .iter()
                    .filter(|v| r.matches(v))
                    .map(|v| v.as_str())
                    .collect();
                let want: Vec<&str> = case.satisfying.iter().map(String::as_str).collect();
                if got != want {
                    failures.push(format!("{:?}: got {got:?}, npm says {want:?}", case.range));
                }
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {} cases diverge from npm:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}

/// The exact version set the oracle was generated against. Changing this
/// without regenerating the oracle invalidates the comparison.
const VERSIONS: &[&str] = &[
    "0.0.0", "0.0.1", "0.0.3", "0.0.4", "0.1.0", "0.1.1", "0.2.3", "0.2.4", "0.3.0",
    "1.0.0", "1.1.0", "1.2.2", "1.2.3", "1.2.4", "1.3.0", "1.9.9", "2.0.0", "2.3.3",
    "2.3.4", "2.3.5", "3.0.0", "1.2.3-alpha", "1.2.3-alpha.1", "1.2.3-alpha.2",
    "1.2.3-beta.2", "1.2.3-beta.3", "1.2.3-rc.1", "2.0.0-rc.1", "2.0.0-alpha",
    "0.2.3-beta.1", "1.2.4-alpha.1",
];
```

- [ ] **Step 7: Register the module**

`src/lib.rs`: add `pub mod range;`. `src/error.rs`: add `use crate::range::RangeError;` and an `#[error(transparent)] Range(#[from] RangeError)` variant.

- [ ] **Step 8: Run everything**

Run: `cargo test range && cargo test --test semver_conformance`
Expected: unit tests PASS; conformance PASSES all 531 cases.

If the conformance suite fails, **do not adjust the oracle.** It is ground
truth from the reference implementation. A failure means either `js-semver`
regressed or the wrapper is wrong.

- [ ] **Step 9: Commit**

```bash
git add Cargo.toml Cargo.lock src/ tests/
git commit -m "feat: npm range parsing behind jerky's own range type"
```

---

### Task 2: Packument fetching

**Files:**
- Modify: `src/registry.rs`, `src/testing.rs`
- Test: inline `#[cfg(test)]` in `src/registry.rs`, plus `tests/http_registry.rs`

**Interfaces:**
- Consumes: `range::Version`
- Produces: `registry::Packument { name, versions, dist_tags }`, `Packument::versions_sorted(&self) -> Vec<Version>`, `Packument::resolve_tag(&self, &str) -> Option<&str>`, `RegistryClient::packument(&self, name: &str)`, `FixtureRegistry::with_packument(...)`

Spec 1 fetches one version at `/{name}/{version}`. Resolving a range needs the
whole version list, which is a different endpoint and a much larger response.

- [ ] **Step 1: Write the failing tests**

Add to `src/registry.rs`'s test module:

```rust
#[test]
fn packument_deserializes_a_registry_response() {
    let raw = r#"{
        "name": "lodash",
        "dist-tags": { "latest": "4.17.21", "next": "5.0.0-beta.1" },
        "versions": {
            "4.17.20": { "name": "lodash", "version": "4.17.20",
                "dist": { "tarball": "https://r.test/l-4.17.20.tgz", "integrity": "sha512-YWJj" } },
            "4.17.21": { "name": "lodash", "version": "4.17.21",
                "dist": { "tarball": "https://r.test/l-4.17.21.tgz", "integrity": "sha512-YWJj" } }
        }
    }"#;

    let p: Packument = serde_json::from_str(raw).unwrap();
    assert_eq!(p.name, "lodash");
    assert_eq!(p.versions.len(), 2);
    assert_eq!(p.resolve_tag("latest"), Some("4.17.21"));
    assert_eq!(p.resolve_tag("nope"), None);
}

#[test]
fn packument_skips_versions_it_cannot_parse() {
    // The registry has published some genuinely malformed versions over the
    // years. One bad entry must not make a package unresolvable.
    let raw = r#"{
        "name": "old",
        "dist-tags": {},
        "versions": {
            "1.0.0":     { "name": "old", "version": "1.0.0",
                "dist": { "tarball": "https://r.test/a.tgz", "integrity": "sha512-YWJj" } },
            "not-a-ver": { "name": "old", "version": "not-a-ver",
                "dist": { "tarball": "https://r.test/b.tgz", "integrity": "sha512-YWJj" } }
        }
    }"#;

    let p: Packument = serde_json::from_str(raw).unwrap();
    let sorted = p.versions_sorted();
    assert_eq!(sorted.len(), 1, "the unparseable version is dropped, not fatal");
    assert_eq!(sorted[0].as_str(), "1.0.0");
}
```

And to `tests/http_registry.rs`:

```rust
#[test]
fn packument_requests_the_abbreviated_form() {
    // The unabbreviated document for a popular package is megabytes of every
    // version ever published, so the Accept header is not an optimisation.
    let (base, _, headers) = serve_capturing(vec![(
        "/lodash",
        Route { status: 200, body: PACKUMENT },
    )]);
    let registry = HttpRegistry::with_base_url(base);

    registry.packument("lodash").unwrap();

    let accept = headers.lock().unwrap().clone();
    assert!(
        accept.iter().any(|h| h == "application/vnd.npm.install-v1+json"),
        "abbreviated packument not requested; got {accept:?}"
    );
}

#[test]
fn packument_reports_an_unknown_package() {
    let (base, _) = serve(vec![]);
    let registry = HttpRegistry::with_base_url(base);
    assert!(matches!(
        registry.packument("nope"),
        Err(RegistryError::PackageNotFound(_))
    ));
}
```

`serve_capturing` is `serve` with an added `Arc<Mutex<Vec<String>>>` recording
each request's `Accept` header. Extend the existing helper rather than adding a
second server.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test packument`
Expected: FAIL — `Packument` not found.

- [ ] **Step 3: Write the implementation**

In `src/registry.rs`:

```rust
use std::collections::BTreeMap;

use crate::range::Version;

/// The abbreviated packument: every published version of one package.
#[derive(Debug, Clone, Deserialize)]
pub struct Packument {
    pub name: String,
    #[serde(default)]
    pub versions: BTreeMap<String, VersionMetadata>,
    #[serde(rename = "dist-tags", default)]
    pub dist_tags: BTreeMap<String, String>,
}

impl Packument {
    /// Every version that parses, in precedence order.
    ///
    /// Unparseable versions are dropped rather than fatal. The registry has
    /// accumulated some genuinely malformed entries, and one of them must not
    /// make an otherwise-fine package unresolvable.
    pub fn versions_sorted(&self) -> Vec<Version> {
        let mut out: Vec<Version> = self
            .versions
            .keys()
            .filter_map(|v| Version::parse(v).ok())
            .collect();
        out.sort();
        out
    }

    pub fn resolve_tag(&self, tag: &str) -> Option<&str> {
        self.dist_tags.get(tag).map(String::as_str)
    }
}
```

Add to the `RegistryClient` trait:

```rust
    /// Every published version of a package, for range resolution.
    fn packument(&self, name: &str) -> Result<Packument, RegistryError>;
```

`HttpRegistry`'s implementation is `version_metadata`'s shape without the
version segment, plus the header, and reusing the same retry policy:

```rust
    fn packument(&self, name: &str) -> Result<Packument, RegistryError> {
        let url = format!("{}/{}", self.base_url, Self::encode_name(name));

        let body = Self::with_retries(|| {
            match self
                .agent
                .get(&url)
                .header("Accept", "application/vnd.npm.install-v1+json")
                .call()
            {
                Ok(mut response) => response
                    .body_mut()
                    .with_config()
                    .limit(MAX_METADATA_BYTES)
                    .read_to_string()
                    .map_err(|source| (RegistryError::MalformedResponse {
                        url: url.clone(), source: Box::new(source) }, Retry::Yes)),
                Err(ureq::Error::StatusCode(404)) => Err((
                    RegistryError::PackageNotFound(name.to_string()), Retry::No)),
                Err(source) => Err((RegistryError::Network {
                    url: url.clone(), source: Box::new(source) }, Retry::Yes)),
            }
        })?;

        serde_json::from_str(&body).map_err(|source| RegistryError::MalformedResponse {
            url, source: Box::new(source),
        })
    }
```

`MAX_METADATA_BYTES` is already 64 MB, which is ample for even the largest
packument in abbreviated form.

- [ ] **Step 4: Extend `FixtureRegistry`**

Give it `with_packument(name, versions: &[(&str, &[(&str, &str)])])`, where each
version carries its own `(dep_name, range)` list. It should derive integrity
from generated tarball bytes exactly as `with_package` does, register `latest`
as the highest version, and count `packument_calls()` so a test can prove the
packument cache works.

Building a whole tree in one call keeps the resolver tests readable:

```rust
let registry = FixtureRegistry::new()
    .with_tree(&[
        ("a", "1.0.0", &[("b", "^1.0.0"), ("c", "^1.0.0")]),
        ("b", "1.0.0", &[("d", "^1.0.0")]),
        ("c", "1.0.0", &[("d", "^1.0.0")]),
        ("d", "1.0.0", &[]),
    ]);
```

- [ ] **Step 5: Run the tests**

Run: `cargo test packument && cargo test --test http_registry`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/ tests/
git commit -m "feat: abbreviated packument fetching"
```

---

### Task 3: The resolver

**Files:**
- Create: `src/resolver.rs`, `tests/resolve.rs`
- Modify: `src/lib.rs`, `src/error.rs`

**Interfaces:**
- Consumes: `RegistryClient`, `Packument`, `range::Range`, `integrity::Integrity`
- Produces: `resolver::PackageId { name, version }`, `resolver::ResolvedPackage`, `resolver::ResolvedGraph`, `resolver::resolve(registry, roots: &BTreeMap<String, String>) -> Result<ResolvedGraph, ResolveError>`, `resolver::ResolveError`

- [ ] **Step 1: Write the failing tests**

Create `tests/resolve.rs`. These tests are the specification of the graph's shape.

```rust
use std::collections::BTreeMap;

use jerky::resolver::{ResolveError, resolve};
use jerky::testing::FixtureRegistry;

fn roots(list: &[(&str, &str)]) -> BTreeMap<String, String> {
    list.iter().map(|(n, r)| (n.to_string(), r.to_string())).collect()
}

#[test]
fn resolves_a_diamond_to_one_shared_node() {
    // a -> b, a -> c, b -> d, c -> d.  d must appear exactly once.
    let registry = FixtureRegistry::new().with_tree(&[
        ("a", "1.0.0", &[("b", "^1.0.0"), ("c", "^1.0.0")]),
        ("b", "1.0.0", &[("d", "^1.0.0")]),
        ("c", "1.0.0", &[("d", "^1.0.0")]),
        ("d", "1.0.0", &[]),
    ]);

    let graph = resolve(&registry, &roots(&[("a", "^1.0.0")])).unwrap();

    assert_eq!(graph.packages.len(), 4);
    let ds: Vec<_> = graph.packages.keys().filter(|id| id.name == "d").collect();
    assert_eq!(ds.len(), 1, "d was duplicated");
}

#[test]
fn incompatible_versions_coexist_as_separate_nodes() {
    // The case that is hard under hoisting and nearly free in a virtual store.
    let registry = FixtureRegistry::new().with_tree(&[
        ("a", "1.0.0", &[("b", "^1.0.0"), ("c", "^1.0.0")]),
        ("b", "1.0.0", &[("d", "^1.0.0")]),
        ("c", "1.0.0", &[("d", "^2.0.0")]),
        ("d", "1.5.0", &[]),
        ("d", "2.1.0", &[]),
    ]);

    let graph = resolve(&registry, &roots(&[("a", "^1.0.0")])).unwrap();

    let mut ds: Vec<_> = graph.packages.keys()
        .filter(|id| id.name == "d")
        .map(|id| id.version.as_str())
        .collect();
    ds.sort();
    assert_eq!(ds, ["1.5.0", "2.1.0"]);

    // And each dependent points at the one it asked for.
    let b = graph.packages.values().find(|p| p.id.name == "b").unwrap();
    let c = graph.packages.values().find(|p| p.id.name == "c").unwrap();
    assert_eq!(b.dependencies["d"].version, "1.5.0");
    assert_eq!(c.dependencies["d"].version, "2.1.0");
}

#[test]
fn a_cycle_terminates() {
    // a -> b -> a. Real packages do this.
    let registry = FixtureRegistry::new().with_tree(&[
        ("a", "1.0.0", &[("b", "^1.0.0")]),
        ("b", "1.0.0", &[("a", "^1.0.0")]),
    ]);

    let graph = resolve(&registry, &roots(&[("a", "^1.0.0")])).unwrap();

    assert_eq!(graph.packages.len(), 2);
    let a = graph.packages.values().find(|p| p.id.name == "a").unwrap();
    assert_eq!(a.dependencies["b"].version, "1.0.0");
}

#[test]
fn a_deep_chain_does_not_blow_the_stack() {
    let mut tree: Vec<(String, String, Vec<(String, String)>)> = Vec::new();
    for i in 0..500 {
        let deps = if i < 499 {
            vec![(format!("p{}", i + 1), "^1.0.0".to_string())]
        } else {
            vec![]
        };
        tree.push((format!("p{i}"), "1.0.0".to_string(), deps));
    }
    let registry = FixtureRegistry::from_owned_tree(&tree);

    let graph = resolve(&registry, &roots(&[("p0", "^1.0.0")])).unwrap();

    assert_eq!(graph.packages.len(), 500);
}

#[test]
fn the_packument_for_a_package_is_fetched_once() {
    // Ten dependents on one package must not mean ten requests. Counted,
    // because the resolved graph is identical either way.
    let registry = FixtureRegistry::new().with_tree(&[
        ("a", "1.0.0", &[("b", "^1.0.0"), ("c", "^1.0.0")]),
        ("b", "1.0.0", &[("shared", "^1.0.0")]),
        ("c", "1.0.0", &[("shared", "^1.0.0")]),
        ("shared", "1.0.0", &[]),
    ]);

    resolve(&registry, &roots(&[("a", "^1.0.0")])).unwrap();

    assert_eq!(registry.packument_calls_for("shared"), 1);
}

#[test]
fn an_unsatisfiable_range_names_the_available_versions() {
    let registry = FixtureRegistry::new().with_tree(&[("a", "1.0.0", &[])]);

    let err = resolve(&registry, &roots(&[("a", "^9.0.0")])).unwrap_err();

    match err {
        ResolveError::Unsatisfiable { name, range, available } => {
            assert_eq!(name, "a");
            assert_eq!(range, "^9.0.0");
            assert!(available.contains(&"1.0.0".to_string()),
                "the error must say what does exist");
        }
        other => panic!("wrong error: {other:?}"),
    }
}

#[test]
fn dist_tags_resolve_through_the_packument() {
    let registry = FixtureRegistry::new().with_tree(&[("a", "1.0.0", &[])]);
    let graph = resolve(&registry, &roots(&[("a", "latest")])).unwrap();
    assert_eq!(graph.packages.len(), 1);
}

#[test]
fn dev_dependencies_of_a_dependency_are_not_followed() {
    // Following them would pull in most of the registry. This is the test
    // that keeps spec 3's devDependencies work additive.
    let registry = FixtureRegistry::new()
        .with_tree(&[("a", "1.0.0", &[]), ("only-a-dev-dep", "1.0.0", &[])])
        .with_dev_dependency("a", "1.0.0", "only-a-dev-dep", "^1.0.0");

    let graph = resolve(&registry, &roots(&[("a", "^1.0.0")])).unwrap();

    assert_eq!(graph.packages.len(), 1, "a dependency's devDependencies were followed");
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test resolve`
Expected: FAIL — `resolve` not found.

- [ ] **Step 3: Write the implementation**

Create `src/resolver.rs`. The walk is a worklist, not recursion — see the deep
chain test.

```rust
use std::collections::{BTreeMap, HashMap, VecDeque};

use thiserror::Error;

use crate::integrity::Integrity;
use crate::range::{Range, RangeError};
use crate::registry::{Packument, RegistryClient, RegistryError};

#[derive(Debug, Error)]
pub enum ResolveError {
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error(transparent)]
    Range(#[from] RangeError),
    #[error("no version of `{name}` satisfies `{range}` (available: {})", available.join(", "))]
    Unsatisfiable {
        name: String,
        range: String,
        available: Vec<String>,
    },
    #[error("`{name}@{version}` has no usable integrity hash")]
    Integrity {
        name: String,
        version: String,
        #[source]
        source: crate::integrity::IntegrityError,
    },
}

/// A node in the resolved graph: one concrete version of one package.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PackageId {
    pub name: String,
    pub version: String,
}

impl std::fmt::Display for PackageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.name, self.version)
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedPackage {
    pub id: PackageId,
    pub resolved: String,
    pub integrity: Integrity,
    /// What this package calls a dependency -> the node it resolved to.
    pub dependencies: BTreeMap<String, PackageId>,
}

#[derive(Debug, Clone)]
pub struct ResolvedGraph {
    /// The ranges the root manifest declared. Makes staleness detectable.
    pub root: BTreeMap<String, String>,
    pub packages: BTreeMap<PackageId, ResolvedPackage>,
}

/// Walk the dependency graph from the root's declared ranges.
///
/// Pure given the registry: fetches metadata, touches no filesystem. That is
/// what lets the tree-shape tests above run with no disk at all.
pub fn resolve(
    registry: &dyn RegistryClient,
    roots: &BTreeMap<String, String>,
) -> Result<ResolvedGraph, ResolveError> {
    let mut packages: BTreeMap<PackageId, ResolvedPackage> = BTreeMap::new();

    // One request per package, not per dependent.
    let mut packuments: HashMap<String, Packument> = HashMap::new();
    // Identical (name, range) pairs resolve once.
    let mut range_cache: HashMap<(String, String), PackageId> = HashMap::new();

    // Each item is a request to resolve one edge, and where to record it.
    // `None` means the edge belongs to the root rather than a package.
    let mut work: VecDeque<(Option<PackageId>, String, String)> = roots
        .iter()
        .map(|(name, range)| (None, name.clone(), range.clone()))
        .collect();

    while let Some((dependent, name, range)) = work.pop_front() {
        let key = (name.clone(), range.clone());

        let id = if let Some(id) = range_cache.get(&key) {
            id.clone()
        } else {
            let packument = match packuments.get(&name) {
                Some(p) => p,
                None => {
                    let p = registry.packument(&name)?;
                    packuments.entry(name.clone()).or_insert(p)
                }
            };

            // A dist-tag is not a range. Try the tag table first, exactly as
            // spec 1's single-version path does.
            let chosen = match packument.resolve_tag(&range) {
                Some(v) => v.to_string(),
                None => {
                    let parsed = Range::parse(&range)?;
                    let versions = packument.versions_sorted();
                    match parsed.max_satisfying(&versions) {
                        Some(v) => v.as_str().to_string(),
                        None => {
                            return Err(ResolveError::Unsatisfiable {
                                name: name.clone(),
                                range: range.clone(),
                                available: versions.iter().map(|v| v.as_str().to_string()).collect(),
                            });
                        }
                    }
                }
            };

            let id = PackageId { name: name.clone(), version: chosen };
            range_cache.insert(key, id.clone());
            id
        };

        // Record the edge on whoever asked for it.
        if let Some(parent) = dependent {
            if let Some(p) = packages.get_mut(&parent) {
                p.dependencies.insert(name.clone(), id.clone());
            }
        }

        // Recursion is gated on node novelty, not path. This is what makes a
        // cycle terminate rather than needing a visited-path stack.
        if packages.contains_key(&id) {
            continue;
        }

        let packument = packuments.get(&id.name).expect("fetched above");
        let metadata = packument
            .versions
            .get(&id.version)
            .expect("version came from this packument");

        let integrity = metadata.dist.integrity().map_err(|source| ResolveError::Integrity {
            name: id.name.clone(),
            version: id.version.clone(),
            source,
        })?;

        packages.insert(
            id.clone(),
            ResolvedPackage {
                id: id.clone(),
                resolved: metadata.dist.tarball.clone(),
                integrity,
                dependencies: BTreeMap::new(),
            },
        );

        // `dependencies` only. Never `devDependencies` — see the global
        // constraints and the test that pins it.
        for (dep_name, dep_range) in metadata.dependencies.iter() {
            work.push_back((Some(id.clone()), dep_name.clone(), dep_range.clone()));
        }
    }

    Ok(ResolvedGraph { root: roots.clone(), packages })
}
```

`VersionMetadata` gains `#[serde(default)] pub dependencies: BTreeMap<String, String>`.
Do **not** add a `dev_dependencies` field: a field that exists is a field
someone will read.

- [ ] **Step 4: Register the module**

`src/lib.rs`: `pub mod resolver;`. `src/error.rs`: add the `Resolve` variant.

- [ ] **Step 5: Run the tests**

Run: `cargo test --test resolve`
Expected: PASS, 8 tests.

- [ ] **Step 6: Commit**

```bash
git add src/ tests/
git commit -m "feat: transitive dependency resolution"
```

---

### Task 4 (revised): The lockfile, keyed by importer

**Status:** implemented in single-project form on `main`. This task reshapes it.

**Files:**
- Modify: `src/lockfile.rs`, `src/resolver.rs`, `tests/lockfile.rs`

**Interfaces:**
- Changed: `ResolvedGraph.root: BTreeMap<String, String>` becomes `ResolvedGraph.importers: BTreeMap<ImporterPath, Importer>`
- Produces: `resolver::ImporterPath(String)`, `resolver::Importer { dependencies: BTreeMap<String, Dependency> }`, `resolver::Dependency { specifier: String, resolution: Resolution }`, `resolver::Resolution` (`Registry(PackageId)` | `Local(PathBuf)`)

The single-project `root` block was right in intent — record what was asked so
staleness is detectable — but singular, and it stored only the specifier.

- [ ] **Step 1: Rewrite the failing tests first**

The existing `tests/lockfile.rs` tests are good and mostly survive: determinism,
diff noise, the version gate, key validation, dangling edges, the trailing
newline. Change what they build, not what they assert. Then add:

```rust
#[test]
fn importers_are_keyed_by_directory() {
    // A workspace of one is keyed ".", which is the case every single-project
    // repo takes. If this is the only importer a test ever sees, the test is
    // not exercising workspaces.
    let graph = /* resolve a workspace with "." and "apps/web" */;
    lockfile::save(&graph, root).unwrap();

    let parsed: serde_json::Value = serde_json::from_str(&read(root)).unwrap();
    let keys: Vec<&str> = parsed["importers"].as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, [".", "apps/web"]);
}

#[test]
fn a_dependency_records_both_what_was_asked_and_what_was_chosen() {
    // The pair is what lets top-level linking read the resolved identity
    // instead of re-matching the range against loaded versions.
    let entry = &parsed["importers"]["."]["dependencies"]["lodash"];
    assert_eq!(entry["specifier"], "^4.17.0");
    assert_eq!(entry["version"], "4.17.21");
}

#[test]
fn a_local_dependency_records_a_link_rather_than_a_version() {
    // `link:` in the version slot is how a workspace package needs no separate
    // mechanism. The path is relative to the importer that declared it.
    let entry = &parsed["importers"]["apps/web"]["dependencies"]["ui"];
    assert_eq!(entry["specifier"], "workspace:*");
    assert_eq!(entry["version"], "link:../../packages/ui");
}

#[test]
fn a_local_dependency_has_no_packages_entry() {
    // There is no tarball and no integrity hash, because there is nothing to
    // verify — the bytes are in the repo.
    assert!(parsed["packages"].as_object().unwrap().keys().all(|k| !k.starts_with("ui@")));
}

#[test]
fn one_stale_importer_does_not_invalidate_the_others() {
    // The reason staleness is per-importer rather than whole-file.
    // Editing apps/web's manifest leaves "."'s recorded specifiers current.
}

#[test]
fn an_importer_path_outside_the_workspace_is_refused() {
    // A key of "../escape" in a hand-edited lockfile must not be honoured.
    // Same class as the tar-slip guard in `archive`: a path from an untrusted
    // file that resolves outside the tree it claims to describe.
}

#[test]
fn an_alias_round_trips() {
    // `execa: { specifier: "npm:safe-execa@0.3.0", version: "0.3.0" }` — real,
    // from pnpm's own lockfile. The importer's key is the local name; the
    // resolution names the actual package.
}
```

- [ ] **Step 2: Change the types**

`ResolvedGraph.root` becomes `importers`. Introduce `Resolution` so a
dependency is explicitly one of two things rather than a version string that
sometimes starts with `link:`:

```rust
/// A workspace-relative directory. `.` is the workspace root.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ImporterPath(String);

#[derive(Debug, Clone)]
pub enum Resolution {
    /// Fetched from the registry; has a node in `packages`.
    Registry(PackageId),
    /// A workspace member, linked in place. No tarball, no integrity hash.
    Local(PathBuf),
}

#[derive(Debug, Clone)]
pub struct Dependency {
    /// What the manifest asked for, verbatim. Makes staleness detectable.
    pub specifier: String,
    pub resolution: Resolution,
}
```

Keeping `Resolution` an enum rather than a `String` that may carry a `link:`
prefix is the point: the compiler then makes every consumer say which case it
handles, and the `link:` spelling exists only at the serialization boundary.

- [ ] **Step 3: Reshape `OnDisk`**

`root` becomes `importers`. `packages` is unchanged — flat, keyed
`name@version`, shared by every importer. Everything already true of it stays
true, including the validation that refuses contradictory keys and dangling
edges.

`ImporterPath` must be validated on load, not just on save. A lockfile is an
untrusted file: a key of `../escape` or an absolute path has to be refused for
the same reason `archive` refuses a tar entry that climbs out of its
destination.

- [ ] **Step 4: Run and commit**

```bash
cargo test --test lockfile && cargo test
git commit -m "feat: lockfile keyed by importer"
```

---

### Task 5 (new): Workspace discovery

**Files:**
- Create: `src/workspace.rs`, `tests/workspace.rs`
- Modify: `src/lib.rs`, `src/error.rs`, `src/manifest.rs`, `Cargo.toml`

**Interfaces:**
- Produces: `workspace::Workspace`, `Workspace::discover(root: &Path) -> Result<Workspace, WorkspaceError>`, `Workspace::members(&self) -> &BTreeMap<ImporterPath, Member>`, `Workspace::member_for(&self, dir: &Path) -> Option<&Member>`, `Workspace::find_root(from: &Path) -> Option<PathBuf>`, `workspace::Member { path, manifest }`, `workspace::WorkspaceError`

- [ ] **Step 1: Add a glob matcher**

```bash
cargo add globset
```

`workspaces` patterns are globs. Hand-rolling `packages/*` is easy and
hand-rolling `packages/**/!(test)` is not, so take the crate.

- [ ] **Step 2: Write the failing tests**

```rust
#[test]
fn a_manifest_without_workspaces_is_a_workspace_of_one() {
    // The single-project case, which must go through the same path as any
    // other. This is the test that proves there is no special branch.
    let dir = TempDir::new().unwrap();
    write_manifest(dir.path(), r#"{"name":"solo"}"#);

    let ws = Workspace::discover(dir.path()).unwrap();

    assert_eq!(ws.members().len(), 1);
    assert!(ws.members().contains_key(&ImporterPath::root()));
}

#[test]
fn globs_expand_to_member_directories() {
    // packages/* with three members, keyed workspace-relative and sorted.
}

#[test]
fn a_match_without_a_manifest_is_skipped_not_fatal() {
    // `packages/.cache` exists and has no package.json. An install must not
    // fail because of a stray directory.
}

#[test]
fn a_pattern_matching_nothing_warns() {
    // Usually a typo. Worth surfacing, not worth failing.
}

#[test]
fn two_members_sharing_a_name_are_refused_naming_both_paths() {
    // Membership is a set of directories, so a name collision is found by
    // looking at paths — which is what the message must carry.
    assert!(matches!(
        Workspace::discover(dir.path()),
        Err(WorkspaceError::DuplicateName { name, first, second })
            if name == "ui" && first != second
    ));
}

#[test]
fn a_pattern_escaping_the_workspace_root_is_refused() {
    // npm permits `../siblings/*`. Silently including a directory outside the
    // repo has the same shape as the tar-slip class `archive` already guards
    // against, so it is refused rather than resolved.
    write_manifest(dir.path(), r#"{"workspaces":["../outside/*"]}"#);
    assert!(matches!(
        Workspace::discover(dir.path()),
        Err(WorkspaceError::EscapesRoot { .. })
    ));
}

#[test]
fn member_for_finds_the_nearest_enclosing_member() {
    // Standing in packages/ui/src/deep means the packages/ui importer.
}

#[test]
fn member_for_returns_none_outside_every_member() {
    // Ambiguous rather than obviously the root, so the caller decides.
}

#[test]
fn a_private_package_is_an_ordinary_member() {
    // Publishability is a property of a package, not of membership.
}

#[test]
fn find_root_walks_up_to_the_nearest_workspace_manifest() {
    // What main.rs uses, given a cwd.
}
```

- [ ] **Step 3: Implement**

`Manifest` gains `workspaces(&self) -> Vec<String>`, reading the field and
returning empty when absent — the degenerate case, not a separate one.

`discover` reads the root manifest, expands each pattern, keeps directories
containing a `package.json`, and always includes `.` whether or not it declares
dependencies.

**`find_root` is the only function that walks up**, and `main.rs` is its only
caller. Everything else takes the root as a parameter.

- [ ] **Step 4: Run and commit**

```bash
cargo test --test workspace && cargo test
git commit -m "feat: workspace discovery from package.json workspaces"
```

---

### Task 6 (was 5): Symlinks — intra-store and importer-relative

**Files:**
- Modify: `src/linker.rs`

**Interfaces:**
- Produces: `linker::symlink_into_store(link_dir, pkg_name, dir_name)`, `linker::symlink_dependency_from(importer_dir, workspace_root, pkg_name, dir_name)`, `linker::symlink_local(importer_dir, pkg_name, target_dir)`

Two corrections land together because they are the same class of change, and
doing them separately would mean rewriting the linker twice.

**The intra-store `../`.** Spec 1's `symlink_dependency` writes a target with no
leading `../`, correct for links in `node_modules/` itself. A link from one
package's private `node_modules` to another package's store directory sits one
level deeper and does need it:

```
node_modules/.jerky/b@1.0.0/node_modules/d -> ../../d@1.5.0/node_modules/d
```

**The importer-relative depth.** A link in a nested importer's `node_modules`
must climb back to the workspace root's virtual store, and how far depends on
where the importer sits:

```
packages/ui/node_modules/lodash -> ../../../node_modules/.jerky/lodash@4.17.21/node_modules/lodash
apps/web/node_modules/lodash    -> ../../../node_modules/.jerky/lodash@4.18.0/node_modules/lodash
node_modules/lodash             -> .jerky/lodash@4.17.21/node_modules/lodash
```

- [ ] **Step 1: Write the failing tests**

Cover, at minimum: the root importer (no `../`), a one-level importer, a
two-level importer, an intra-store link, and a local package link. Assert on
the **resolved** target as well as its text — a link whose string looks right
but does not resolve is the bug this catches.

```rust
#[test]
fn depth_is_computed_not_assumed() {
    // The same package, linked from importers at two different depths, must
    // get two different targets and both must resolve.
}

#[test]
fn a_local_package_links_straight_at_its_directory() {
    // apps/web/node_modules/ui -> ../../packages/ui
    // Not into the virtual store: there is no store entry, because there is
    // no tarball.
}
```

- [ ] **Step 2: Implement**

One function computes the prefix from the importer's depth; every target is
built from it. **No literal `../` anywhere else in the module** — that is the
constraint that keeps a nested importer from silently getting a root-shaped
link.

- [ ] **Step 3: Correct the spec 1 design**

Section 5's symlink line is still written for a single project. Note that the
target is importer-relative and point at the workspace design.

- [ ] **Step 4: Run and commit**

---

### Task 7 (was 6): Install across a workspace

**Files:**
- Modify: `src/commands/install.rs`, `src/main.rs`, `tests/install.rs`

**Interfaces:**
- Produces: `install(workspace: &Workspace, importer: &ImporterPath, store, registry, spec) -> Result<Installed, InstallError>`

The phase structure from the single-project version holds. What changes is that
resolution seeds from every importer, and linking happens per importer.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn installing_from_one_importer_leaves_the_others_linked() {
    // Two importers wanting different versions. Both end up correct, and
    // installing into one does not disturb the other's node_modules.
}

#[test]
fn two_importers_on_the_same_version_share_one_store_entry() {
    // The reason the store lives at the workspace root. Asserted on inode,
    // like spec 1's hard-link test — a copy passes every other assertion.
}

#[test]
fn a_local_dependency_is_linked_not_fetched() {
    // Counted: the registry must see zero requests for a workspace member.
}

#[test]
fn a_workspace_specifier_naming_no_member_is_an_error() {
    // `workspace:*` for a package that is not in the repo is a typo, not a
    // fallback to the registry.
}

#[test]
fn a_single_importer_workspace_installs_exactly_as_before() {
    // Spec 1 and 2's existing behaviour, now through the general path.
}
```

- [ ] **Step 2: Implement**

Resolution seeds from every importer's ranges at once, so one walk covers the
workspace and two importers wanting the same package share the work.

Local specifiers short-circuit before the registry: `workspace:*` resolves to
the member of that name, and errors if there is none.

Linking runs per importer: each gets its own `node_modules` of symlinks, built
with the depth-aware targets from Task 6.

- [ ] **Step 3: Wire `main.rs`**

`main.rs` finds the workspace root by walking up from the cwd, discovers
members, and picks the importer the user is standing in. A cwd inside no member
is an error naming the members, not a guess.

- [ ] **Step 4: Run, smoke-test, commit**

Verify against a real two-package workspace, not only fixtures.

---

### Task 8 (was 7): Caret ranges, per-importer reuse, and integrity authority

**Files:**
- Modify: `src/commands/install.rs`, `tests/install.rs`, `CHANGELOG.md`

Three behaviours, grouped because they are all about the lockfile being
consumed rather than merely written.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn the_manifest_records_a_caret_range() {
    // What spec 1 deferred to "spec 2, when ranges actually resolve".
}

#[test]
fn an_unchanged_workspace_does_not_re_resolve() {
    // Counted, because the resolved graph is identical either way.
}

#[test]
fn editing_one_importer_re_resolves_only_what_it_must() {
    // Staleness is per-importer: a changed apps/web must not invalidate
    // everything the root already resolved. This is what the importers map
    // buys over a single `root` block.
}

#[test]
fn a_lockfile_integrity_mismatch_stops_the_install() {
    // The trust-on-first-use anchor spec 1 explicitly went without.
}
```

- [ ] **Step 2: Implement**

Reuse compares each importer's recorded specifiers against its manifest. An
importer that matches is reused; one that does not is re-resolved.

The integrity check runs on the download path only. A store hit is itself proof
the bytes hash to the recorded key, so re-fetching metadata for an entry
already in the store would erode the benefit of reuse for no guarantee.
**Say so in a comment**, or a reader will think the check is partial.

- [ ] **Step 3: Record the behaviour change**

`jerky install lodash` used to write `"4.17.21"` and now writes `"^4.17.21"`.
This is the first change that alters output for an existing user, so it is the
moment to start `CHANGELOG.md`.

- [ ] **Step 4: Full verification and a real smoke test**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
```

Then a real two-package workspace installing a real dependency, confirming the
store is shared, each importer's `node_modules` resolves, and a second install
makes no request.

---

## Self-Review

Check each against the diff rather than from memory.

- **Does anything outside `src/range.rs` name `js_semver`?** `grep -rn 'js_semver' src/ tests/` should return `range.rs` only.
- **Is there a `HashMap` on any path that reaches the lockfile?** Internal caches may use one; anything serialized may not.
- **Does any module below `main.rs` read the cwd or `$HOME`?** Only `main.rs` locates the workspace root.
- **Is there a literal `../` outside the linker's depth calculation?** That is the bug that gives a nested importer a root-shaped link.
- **Is `devDependencies` read anywhere?** It should not be, and `VersionMetadata` should not have a field for it.
- **Does any test exercise more than one importer?** A suite that only ever sees `.` is not testing workspaces.
- **Do untrusted paths get validated?** Importer keys from a lockfile and `workspaces` globs from a manifest can both escape the root, and neither should.
- **Is the caret change noted for users?**
- **Do the spec 1 tests still pass**, apart from the manifest assertion the caret change moves?

## Deferred

- **Parallel downloads — #33.** Until it lands, large trees install visibly slower than npm, which is the accepted cost of this spec.
- **Peer dependencies — #34.** jerky will install trees npm would warn about.
- **`--filter` selection — #40.** The cwd covers the common case. Overlaps #13 and #18, which need the same project graph.
- **Enforcing one version across importers — #41.** Detection first; enforcement needs somewhere to configure it, which is #12's territory.
- **Lockfile pruning**, unfiled: there is no operation that would trigger it until an `uninstall` command exists.
