# Package Manager Spec 2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Resolve npm ranges and transitive dependency trees, install a whole graph into the virtual store, and record the result in a deterministic `jerky-lock.json`.

**Architecture:** Three new modules on top of spec 1. `range` wraps `js-semver` and is the only module that names it, so the 0.4.0 dependency is swappable in one file. `resolver` is pure given a `RegistryClient` — it fetches metadata and returns a `ResolvedGraph`, writing nothing to disk, which is what makes it testable with no filesystem at all. `lockfile` is a straight serialization of that graph, which is why the two were designed together. `commands/install` grows from installing one package to walking a graph.

**Tech Stack:** Rust 2024, plus `js-semver`. Everything else is already in the tree.

**Spec:** `docs/superpowers/specs/2026-09-08-resolver-and-lockfile-design.md`
**Research:** `docs/superpowers/research/2026-09-08-npm-semver-crate-selection.md`

## Global Constraints

Spec 1's constraints all still hold. These are the ones this spec adds or sharpens.

- **Nothing outside `src/range.rs` may name `js_semver`.** This is the containment boundary for a 0.4.0 dependency and the reason a future swap is one file. A `use js_semver::` anywhere else is a bug, not a shortcut.
- **`BTreeMap`, never `HashMap`, anywhere that reaches the lockfile.** Iteration order is serialization order, and two machines resolving the same tree must produce byte-identical files. Sorted-by-construction is how that is guaranteed rather than remembered.
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

### Task 4: The lockfile

**Files:**
- Create: `src/lockfile.rs`, `tests/lockfile.rs`
- Modify: `src/lib.rs`, `src/error.rs`

**Interfaces:**
- Consumes: `ResolvedGraph`, `PackageId`, `Integrity`
- Produces: `lockfile::LOCKFILE_NAME`, `lockfile::LOCKFILE_VERSION`, `lockfile::save(&ResolvedGraph, project_dir) -> Result<(), LockfileError>`, `lockfile::load(project_dir) -> Result<Option<ResolvedGraph>, LockfileError>`, `lockfile::LockfileError`

`load` returns `Ok(None)` for a missing file — absence is normal, not an error.

- [ ] **Step 1: Write the failing tests**

Create `tests/lockfile.rs`:

```rust
use std::collections::BTreeMap;

use jerky::lockfile::{self, LockfileError};
use jerky::resolver::resolve;
use jerky::testing::FixtureRegistry;
use tempfile::TempDir;

fn roots(list: &[(&str, &str)]) -> BTreeMap<String, String> {
    list.iter().map(|(n, r)| (n.to_string(), r.to_string())).collect()
}

fn small_tree() -> FixtureRegistry {
    FixtureRegistry::new().with_tree(&[
        ("a", "1.0.0", &[("b", "^1.0.0")]),
        ("b", "1.0.0", &[]),
    ])
}

#[test]
fn writing_the_same_graph_twice_is_byte_identical() {
    // #11's first requirement. Two machines resolving the same tree must
    // produce identical files or the lockfile churns in every diff.
    let registry = small_tree();
    let dir = TempDir::new().unwrap();

    let g1 = resolve(&registry, &roots(&[("a", "^1.0.0")])).unwrap();
    lockfile::save(&g1, dir.path()).unwrap();
    let first = std::fs::read_to_string(dir.path().join("jerky-lock.json")).unwrap();

    let g2 = resolve(&registry, &roots(&[("a", "^1.0.0")])).unwrap();
    lockfile::save(&g2, dir.path()).unwrap();
    let second = std::fs::read_to_string(dir.path().join("jerky-lock.json")).unwrap();

    assert_eq!(first, second);
}

#[test]
fn round_trips_through_disk() {
    let registry = small_tree();
    let dir = TempDir::new().unwrap();

    let graph = resolve(&registry, &roots(&[("a", "^1.0.0")])).unwrap();
    lockfile::save(&graph, dir.path()).unwrap();
    let back = lockfile::load(dir.path()).unwrap().expect("a lockfile was written");

    assert_eq!(back.root, graph.root);
    assert_eq!(back.packages.len(), graph.packages.len());
    for (id, pkg) in &graph.packages {
        let other = back.packages.get(id).expect("every package survives");
        assert_eq!(other.resolved, pkg.resolved);
        assert_eq!(other.integrity.to_ssri(), pkg.integrity.to_ssri());
        assert_eq!(other.dependencies, pkg.dependencies);
    }
}

#[test]
fn adding_one_dependency_touches_only_its_own_block() {
    // #11's "minimal diff noise". A flat, name@version-keyed map is what
    // makes this true; a nested tree would reindent everything below a change.
    let registry = FixtureRegistry::new().with_tree(&[
        ("a", "1.0.0", &[("b", "^1.0.0")]),
        ("b", "1.0.0", &[]),
        ("newcomer", "1.0.0", &[]),
    ]);
    let dir = TempDir::new().unwrap();

    let before_graph = resolve(&registry, &roots(&[("a", "^1.0.0")])).unwrap();
    lockfile::save(&before_graph, dir.path()).unwrap();
    let before = std::fs::read_to_string(dir.path().join("jerky-lock.json")).unwrap();

    let after_graph =
        resolve(&registry, &roots(&[("a", "^1.0.0"), ("newcomer", "^1.0.0")])).unwrap();
    lockfile::save(&after_graph, dir.path()).unwrap();
    let after = std::fs::read_to_string(dir.path().join("jerky-lock.json")).unwrap();

    let removed = before.lines().filter(|l| !after.contains(*l)).count();
    assert!(
        removed <= 1,
        "adding a dependency rewrote {removed} existing lines; only the root block should change"
    );
}

#[test]
fn a_missing_lockfile_is_not_an_error() {
    let dir = TempDir::new().unwrap();
    assert!(lockfile::load(dir.path()).unwrap().is_none());
}

#[test]
fn an_unknown_lockfile_version_is_refused() {
    // Real projects commit lockfiles. A best-effort parse of a format we do
    // not understand is how a tool silently installs the wrong tree.
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("jerky-lock.json"),
        r#"{"lockfileVersion": 99, "root": {}, "packages": {}}"#,
    )
    .unwrap();

    assert!(matches!(
        lockfile::load(dir.path()),
        Err(LockfileError::UnsupportedVersion { found: 99, .. })
    ));
}

#[test]
fn a_malformed_lockfile_is_refused_not_ignored() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("jerky-lock.json"), "{ not json").unwrap();
    assert!(matches!(lockfile::load(dir.path()), Err(LockfileError::Malformed { .. })));
}

#[test]
fn the_file_ends_with_a_newline() {
    // So it is a well-formed text file and diffs do not show "\ No newline".
    let registry = small_tree();
    let dir = TempDir::new().unwrap();
    let graph = resolve(&registry, &roots(&[("a", "^1.0.0")])).unwrap();
    lockfile::save(&graph, dir.path()).unwrap();

    let raw = std::fs::read_to_string(dir.path().join("jerky-lock.json")).unwrap();
    assert!(raw.ends_with('\n'));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test lockfile`
Expected: FAIL — `lockfile` not found.

- [ ] **Step 3: Write the implementation**

Create `src/lockfile.rs`. The on-disk shape is a separate type from
`ResolvedGraph`, deliberately: the graph is what the resolver finds convenient,
the lockfile is what is stable to commit, and conflating them means every
internal refactor is a format change.

```rust
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::integrity::Integrity;
use crate::resolver::{PackageId, ResolvedGraph, ResolvedPackage};

pub const LOCKFILE_NAME: &str = "jerky-lock.json";

/// Bumped when the on-disk shape changes. Present from the first release
/// because a committed format without one has no migration path.
pub const LOCKFILE_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum LockfileError {
    #[error("{path} is not valid JSON")]
    Malformed {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("{path} uses lockfile version {found}, but this jerky understands {supported}. Upgrade jerky.")]
    UnsupportedVersion { path: PathBuf, found: u32, supported: u32 },
    #[error("{path} records `{entry}`, which is not a valid name@version key")]
    BadKey { path: PathBuf, entry: String },
    #[error("{path} records an unusable integrity hash for `{entry}`")]
    BadIntegrity { path: PathBuf, entry: String },
    #[error("failed to access {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Serialize, Deserialize)]
struct OnDisk {
    #[serde(rename = "lockfileVersion")]
    lockfile_version: u32,
    /// The ranges the manifest declared, so staleness is detectable.
    root: BTreeMap<String, String>,
    /// Keyed `name@version`. Flat rather than nested so that adding a
    /// dependency appends a block instead of reindenting a subtree.
    packages: BTreeMap<String, Entry>,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    version: String,
    resolved: String,
    integrity: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    dependencies: BTreeMap<String, String>,
}

pub fn save(graph: &ResolvedGraph, project_dir: &Path) -> Result<(), LockfileError> {
    let packages = graph
        .packages
        .values()
        .map(|p| {
            (
                p.id.to_string(),
                Entry {
                    version: p.id.version.clone(),
                    resolved: p.resolved.clone(),
                    integrity: p.integrity.to_ssri(),
                    // Edge values are the concrete version, not the range:
                    // the lockfile records what was chosen, not what was asked.
                    dependencies: p
                        .dependencies
                        .iter()
                        .map(|(n, id)| (n.clone(), id.version.clone()))
                        .collect(),
                },
            )
        })
        .collect();

    let on_disk = OnDisk {
        lockfile_version: LOCKFILE_VERSION,
        root: graph.root.clone(),
        packages,
    };

    let path = project_dir.join(LOCKFILE_NAME);
    let mut text = serde_json::to_string_pretty(&on_disk)
        .expect("a lockfile is always serializable");
    text.push('\n');

    std::fs::write(&path, text).map_err(|source| LockfileError::Io { path, source })
}

pub fn load(project_dir: &Path) -> Result<Option<ResolvedGraph>, LockfileError> {
    let path = project_dir.join(LOCKFILE_NAME);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        // Absence is normal: the first install has no lockfile to read.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(LockfileError::Io { path, source }),
    };

    let on_disk: OnDisk = serde_json::from_str(&raw)
        .map_err(|source| LockfileError::Malformed { path: path.clone(), source })?;

    if on_disk.lockfile_version != LOCKFILE_VERSION {
        return Err(LockfileError::UnsupportedVersion {
            path,
            found: on_disk.lockfile_version,
            supported: LOCKFILE_VERSION,
        });
    }

    let mut packages = BTreeMap::new();
    for (key, entry) in on_disk.packages {
        let name = key
            .rsplit_once('@')
            .filter(|(name, _)| !name.is_empty())
            .map(|(name, _)| name.to_string())
            .ok_or_else(|| LockfileError::BadKey {
                path: path.clone(),
                entry: key.clone(),
            })?;

        let id = PackageId { name, version: entry.version.clone() };
        let integrity = Integrity::parse(&entry.integrity).map_err(|_| {
            LockfileError::BadIntegrity { path: path.clone(), entry: key.clone() }
        })?;

        let dependencies = entry
            .dependencies
            .iter()
            .map(|(n, v)| (n.clone(), PackageId { name: n.clone(), version: v.clone() }))
            .collect();

        packages.insert(
            id.clone(),
            ResolvedPackage { id, resolved: entry.resolved, integrity, dependencies },
        );
    }

    Ok(Some(ResolvedGraph { root: on_disk.root, packages }))
}
```

**Note on `rsplit_once('@')`**: splitting from the right is what makes
`@scope/name@1.0.0` parse correctly when spec 3 adds scoped packages. Doing it
now costs nothing and avoids a quiet bug later — the same reasoning spec 1
applied to `parse_package_spec`.

- [ ] **Step 4: Register the module**

`src/lib.rs`: `pub mod lockfile;`. `src/error.rs`: the `Lockfile` variant.

- [ ] **Step 5: Run the tests**

Run: `cargo test --test lockfile`
Expected: PASS, 7 tests.

- [ ] **Step 6: Add `jerky-lock.json` awareness to `.gitignore`?**

**No.** Lockfiles are committed; that is their purpose. This step exists only
to record that the question was asked and answered.

- [ ] **Step 7: Commit**

```bash
git add src/ tests/
git commit -m "feat: deterministic jerky-lock.json"
```

---

### Task 5: Intra-store symlinks

**Files:**
- Modify: `src/linker.rs`
- Test: inline `#[cfg(test)]` in `src/linker.rs`

**Interfaces:**
- Produces: `linker::symlink_into_store(store_dir: &Path, pkg_name: &str, dir_name: &str) -> Result<(), LinkError>`

**The correction spec 1 predicted.** `symlink_dependency` writes a target with
no leading `../`, correct for links in `node_modules/` itself. A link from one
package's private `node_modules` to another package's store directory sits one
level deeper and does need it. Spec 1's Task 7 note called this out as pnpm's
actual use of `../`; this is where it lands.

Concretely, for `b@1.0.0` depending on `d@1.5.0`:

```
node_modules/.jerky/b@1.0.0/node_modules/d
  -> ../../d@1.5.0/node_modules/d
```

Two `../` because the link sits at `.jerky/b@1.0.0/node_modules/d`, and the
target `d@1.5.0` is a sibling of `b@1.0.0` under `.jerky/`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn intra_store_links_climb_back_to_the_virtual_store_root() {
    let root = TempDir::new().unwrap();
    let src = store_entry(root.path());
    let node_modules = root.path().join("node_modules");

    populate_virtual_store(&src, &node_modules, "b@1.0.0", "b").unwrap();
    populate_virtual_store(&src, &node_modules, "d@1.5.0", "d").unwrap();

    let b_modules = node_modules.join(".jerky/b@1.0.0/node_modules");
    symlink_into_store(&b_modules, "d", "d@1.5.0").unwrap();

    let target = std::fs::read_link(b_modules.join("d")).unwrap();
    assert_eq!(target, Path::new("../../d@1.5.0/node_modules/d"));

    // And it resolves — the assertion that catches an off-by-one in the ../
    assert!(b_modules.join("d").join("package.json").is_file());
}

#[test]
fn a_package_sees_only_its_own_dependencies() {
    // The reason for the doubled node_modules. `b` gets `d` as a sibling and
    // nothing else, so a phantom dependency cannot resolve.
    let root = TempDir::new().unwrap();
    let src = store_entry(root.path());
    let node_modules = root.path().join("node_modules");
    populate_virtual_store(&src, &node_modules, "b@1.0.0", "b").unwrap();
    populate_virtual_store(&src, &node_modules, "d@1.5.0", "d").unwrap();
    populate_virtual_store(&src, &node_modules, "unrelated@1.0.0", "unrelated").unwrap();

    let b_modules = node_modules.join(".jerky/b@1.0.0/node_modules");
    symlink_into_store(&b_modules, "d", "d@1.5.0").unwrap();

    let siblings: Vec<_> = std::fs::read_dir(&b_modules)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert!(siblings.contains(&"d".to_string()));
    assert!(!siblings.contains(&"unrelated".to_string()));
}
```

- [ ] **Step 2: Write the implementation**

`symlink_into_store` is `symlink_dependency` with a different target prefix.
Factor the shared replace-an-existing-link logic rather than copying it — the
store and linker already drifted once in spec 1 by keeping two copies of
staging cleanup, and that cost a real bug.

```rust
/// Link one package's dependency inside the virtual store.
///
/// `link_dir` is the dependent's own `node_modules`, at
/// `.jerky/<dependent>/node_modules`. The target climbs two levels to reach
/// `.jerky/` and descends into the dependency's directory — unlike
/// `symlink_dependency`, whose links sit in `node_modules/` itself and need
/// no `../` at all.
pub fn symlink_into_store(
    link_dir: &Path,
    pkg_name: &str,
    dir_name: &str,
) -> Result<(), LinkError> {
    let target = Path::new("..")
        .join("..")
        .join(dir_name)
        .join("node_modules")
        .join(pkg_name);
    place_symlink(link_dir, pkg_name, &target)
}
```

- [ ] **Step 3: Run and commit**

Run: `cargo test --lib linker`
Expected: PASS.

```bash
git add src/
git commit -m "feat: intra-virtual-store symlinks"
```

---

### Task 6: Install a graph

**Files:**
- Modify: `src/commands/install.rs`, `tests/install.rs`
- Test: `tests/install.rs`

**Interfaces:**
- Consumes: `resolve`, `ResolvedGraph`, `Store`, `linker`, `archive`, `Manifest`
- Produces: `commands::install::install(project_dir, store, registry, spec) -> Result<Installed, InstallError>` with `Installed { name, version, resolved_count }`

Spec 1's `install` fetched, verified, stored and linked one package. The shape
is unchanged; it now runs over every node in a graph.

- [ ] **Step 1: Write the failing tests**

Add to `tests/install.rs`:

```rust
#[test]
fn installs_a_whole_dependency_tree() {
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let project_dir = project(work.path());
    let store = Store::new(home.path().join("store"));
    let registry = FixtureRegistry::new().with_tree(&[
        ("a", "1.0.0", &[("b", "^1.0.0")]),
        ("b", "1.0.0", &[("c", "^1.0.0")]),
        ("c", "1.0.0", &[]),
    ]);

    let installed = install(project_dir, &store, &registry,
        &spec("a", VersionSpec::Latest)).unwrap();

    assert_eq!(installed.resolved_count, 3);

    // Only the direct dependency is visible at the top level.
    assert!(project_dir.join("node_modules/a").exists());
    assert!(!project_dir.join("node_modules/b").exists(),
        "transitive deps must not be hoisted into the project's node_modules");

    // But `a` can see `b`, and `b` can see `c`.
    assert!(project_dir.join("node_modules/a/../../.jerky/a@1.0.0/node_modules/b").exists()
         || project_dir.join("node_modules/.jerky/a@1.0.0/node_modules/b").exists());
    assert!(project_dir.join("node_modules/.jerky/b@1.0.0/node_modules/c").exists());
}

#[test]
fn only_the_requested_package_is_recorded_in_the_manifest() {
    // Transitive dependencies live in the lockfile, never in package.json.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let project_dir = project(work.path());
    let store = Store::new(home.path().join("store"));
    let registry = FixtureRegistry::new().with_tree(&[
        ("a", "1.0.0", &[("b", "^1.0.0")]),
        ("b", "1.0.0", &[]),
    ]);

    install(project_dir, &store, &registry, &spec("a", VersionSpec::Latest)).unwrap();

    let raw = std::fs::read_to_string(project_dir.join("package.json")).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert!(parsed["dependencies"]["a"].is_string());
    assert!(parsed["dependencies"]["b"].is_null(), "b is transitive");
}

#[test]
fn both_versions_of_a_conflicting_dependency_land_on_disk() {
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let project_dir = project(work.path());
    let store = Store::new(home.path().join("store"));
    let registry = FixtureRegistry::new().with_tree(&[
        ("a", "1.0.0", &[("b", "^1.0.0"), ("c", "^1.0.0")]),
        ("b", "1.0.0", &[("d", "^1.0.0")]),
        ("c", "1.0.0", &[("d", "^2.0.0")]),
        ("d", "1.5.0", &[]),
        ("d", "2.1.0", &[]),
    ]);

    install(project_dir, &store, &registry, &spec("a", VersionSpec::Latest)).unwrap();

    let jerky = project_dir.join("node_modules/.jerky");
    assert!(jerky.join("d@1.5.0").is_dir());
    assert!(jerky.join("d@2.1.0").is_dir());
    // And each dependent's private view points at its own.
    assert!(jerky.join("b@1.0.0/node_modules/d/package.json").is_file());
    assert!(jerky.join("c@1.0.0/node_modules/d/package.json").is_file());
}

#[test]
fn a_failure_partway_through_records_nothing() {
    // The manifest and lockfile are written last, so an interrupted install
    // leaves an installed-but-unrecorded tree rather than a manifest that
    // lies. `c`'s tarball does not match its advertised hash.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let project_dir = project(work.path());
    let store = Store::new(home.path().join("store"));
    let registry = FixtureRegistry::new()
        .with_tree(&[("a", "1.0.0", &[("c", "^1.0.0")])])
        .with_corrupt_package("c", "1.0.0", vec![1, 2, 3]);

    let result = install(project_dir, &store, &registry, &spec("a", VersionSpec::Latest));
    assert!(result.is_err());

    let raw = std::fs::read_to_string(project_dir.join("package.json")).unwrap();
    assert!(!raw.contains("\"a\""), "manifest recorded a failed install");
    assert!(!project_dir.join("jerky-lock.json").exists(), "lockfile written on failure");
}
```

- [ ] **Step 2: Write the implementation**

Restructure `install` into three phases. The phase boundaries are what make
the ordering guarantees legible rather than emergent.

```rust
pub fn install(
    project_dir: &Path,
    store: &Store,
    registry: &dyn RegistryClient,
    spec: &PackageSpec,
) -> Result<Installed, InstallError> {
    // Phase 1: resolve. No disk writes at all.
    let mut manifest = Manifest::load(project_dir)?;

    let mut roots = manifest.dependency_ranges();
    roots.insert(spec.name.clone(), spec.version.as_request().to_string());
    let graph = resolver::resolve(registry, &roots)?;

    // The requested package's concrete version, for the manifest and the
    // success message. Taken from the graph, never from the request.
    let requested = graph
        .packages
        .values()
        .find(|p| p.id.name == spec.name)
        .ok_or_else(|| InstallError::NotResolved(spec.name.clone()))?
        .clone();

    // Phase 2: materialise every node. Store first, then links.
    let node_modules = project_dir.join("node_modules");
    for pkg in graph.packages.values() {
        let key = pkg.integrity.store_key();
        let entry = if store.contains(&key) {
            store.entry_path(&key)
        } else {
            let tarball = registry.fetch_tarball(&pkg.resolved)?;
            pkg.integrity.verify(&tarball).map_err(|source| InstallError::Integrity {
                name: pkg.id.name.clone(),
                version: pkg.id.version.clone(),
                source,
            })?;
            store.commit(&key, |staging| archive::extract(&tarball, staging))?
        };

        linker::populate_virtual_store(
            &entry, &node_modules, &pkg.id.to_string(), &pkg.id.name)?;
    }

    // Phase 3: wire the edges, now that every directory exists. Splitting this
    // from phase 2 avoids linking at a target that has not been created yet,
    // which the graph's ordering does not otherwise guarantee.
    for pkg in graph.packages.values() {
        let own_modules = node_modules
            .join(".jerky")
            .join(pkg.id.to_string())
            .join("node_modules");
        for (dep_name, dep_id) in &pkg.dependencies {
            linker::symlink_into_store(&own_modules, dep_name, &dep_id.to_string())?;
        }
    }

    // Direct dependencies — and only those — are visible at the top level.
    for name in graph.root.keys() {
        if let Some(p) = graph.packages.values().find(|p| &p.id.name == name) {
            linker::symlink_dependency(&node_modules, name, &p.id.to_string())?;
        }
    }

    // Phase 4: record. Last, so nothing claims more than is on disk.
    manifest.add_dependency(&requested.id.name, &format!("^{}", requested.id.version));
    manifest.save()?;
    lockfile::save(&graph, project_dir)?;

    Ok(Installed {
        name: requested.id.name.clone(),
        version: requested.id.version.clone(),
        resolved_count: graph.packages.len(),
    })
}
```

`Manifest` gains `dependency_ranges(&self) -> BTreeMap<String, String>`, reading
the `dependencies` object. Existing exact pins from spec 1 installs parse as
ranges that match exactly one version, so old manifests keep working with no
migration.

- [ ] **Step 3: Update `main.rs`'s output**

```rust
            let installed = jerky::commands::install::install(&project_dir, &store, &registry, &spec)?;
            println!("resolved {} packages", installed.resolved_count);
            println!("added {}@{}", installed.name, installed.version);
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --test install`
Expected: PASS — spec 1's 7 tests plus 4 new ones.

Spec 1's `installs_a_package_end_to_end` will need its manifest assertion
updated from `"4.17.21"` to `"^4.17.21"`. That is the caret change, not a
regression — confirm it is the only spec 1 test that moves.

- [ ] **Step 5: Commit**

```bash
git add src/ tests/
git commit -m "feat: install a whole dependency graph"
```

---

### Task 7: Caret ranges, lockfile reuse, and integrity authority

**Files:**
- Modify: `src/commands/install.rs`, `tests/install.rs`, `README` or `CHANGELOG` if one exists

**Interfaces:**
- Produces: no new public API; behaviour changes only

Three related behaviours, grouped because they are all about the lockfile being
consumed rather than merely written.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn the_manifest_records_a_caret_range() {
    // The change spec 1 deferred: "Switch to caret in spec 2, when ranges
    // actually resolve."
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let project_dir = project(work.path());
    let store = Store::new(home.path().join("store"));
    let registry = FixtureRegistry::new().with_tree(&[("lodash", "4.17.21", &[])]);

    install(project_dir, &store, &registry, &spec("lodash", VersionSpec::Latest)).unwrap();

    let raw = std::fs::read_to_string(project_dir.join("package.json")).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(parsed["dependencies"]["lodash"], "^4.17.21");
}

#[test]
fn an_unchanged_project_does_not_re_resolve() {
    // Counted, not observed: the resolved tree is identical either way.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let project_dir = project(work.path());
    let store = Store::new(home.path().join("store"));
    let registry = FixtureRegistry::new().with_tree(&[
        ("a", "1.0.0", &[("b", "^1.0.0")]),
        ("b", "1.0.0", &[]),
    ]);

    install(project_dir, &store, &registry, &spec("a", VersionSpec::Latest)).unwrap();
    let after_first = registry.packument_calls();

    install(project_dir, &store, &registry, &spec("a", VersionSpec::Latest)).unwrap();

    assert_eq!(registry.packument_calls(), after_first,
        "the lockfile was not reused");
}

#[test]
fn a_changed_manifest_invalidates_the_lockfile() {
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let project_dir = project(work.path());
    let store = Store::new(home.path().join("store"));
    let registry = FixtureRegistry::new()
        .with_tree(&[("a", "1.0.0", &[]), ("newcomer", "1.0.0", &[])]);

    install(project_dir, &store, &registry, &spec("a", VersionSpec::Latest)).unwrap();
    let after_first = registry.packument_calls();

    install(project_dir, &store, &registry, &spec("newcomer", VersionSpec::Latest)).unwrap();

    assert!(registry.packument_calls() > after_first,
        "a new dependency must trigger resolution");
    let graph = jerky::lockfile::load(project_dir).unwrap().unwrap();
    assert_eq!(graph.packages.len(), 2);
}

#[test]
fn a_lockfile_integrity_mismatch_stops_the_install() {
    // The trust-on-first-use anchor. Spec 1 accepted this gap explicitly:
    // "a republished or tampered tarball for an already-pinned version would
    // go unnoticed." This is the test that closes it.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let project_dir = project(work.path());
    let store = Store::new(home.path().join("store"));
    let registry = FixtureRegistry::new().with_tree(&[("a", "1.0.0", &[])]);

    install(project_dir, &store, &registry, &spec("a", VersionSpec::Latest)).unwrap();

    // Someone republished a@1.0.0 with different bytes.
    let tampered = registry.republish("a", "1.0.0", vec![9, 9, 9]);

    let result = install(project_dir, &store, &tampered, &spec("a", VersionSpec::Latest));
    assert!(matches!(result, Err(InstallError::LockfileIntegrityMismatch { .. })),
        "a changed hash for a pinned version must stop the install");
}
```

- [ ] **Step 2: Write the implementation**

Insert reuse ahead of resolution in phase 1:

```rust
    let mut roots = manifest.dependency_ranges();
    roots.insert(spec.name.clone(), spec.version.as_request().to_string());

    // Reuse the lockfile when it already describes exactly these ranges.
    // `root` is what makes this decidable: without it there is no way to tell
    // a current lockfile from one written before someone edited package.json.
    let graph = match lockfile::load(project_dir)? {
        Some(locked) if locked.root == roots => locked,
        _ => resolver::resolve(registry, &roots)?,
    };
```

And in phase 2, before fetching, when the graph came from the lockfile:

```rust
        // The lockfile's hash is authoritative. If the registry now reports a
        // different one for the same version, the tarball was republished or
        // tampered with, and continuing would defeat the point of recording it.
        if from_lockfile {
            let live = registry.version_metadata(&pkg.id.name, &pkg.id.version)?;
            let live_integrity = live.dist.integrity().map_err(/* ... */)?;
            if live_integrity.to_ssri() != pkg.integrity.to_ssri() {
                return Err(InstallError::LockfileIntegrityMismatch {
                    name: pkg.id.name.clone(),
                    version: pkg.id.version.clone(),
                    expected: pkg.integrity.to_ssri(),
                    found: live_integrity.to_ssri(),
                });
            }
        }
```

**A judgement call to make explicit while implementing:** this check costs one
metadata request per locked package, which erodes the speed benefit of reuse.
The alternative is to check only when the package is missing from the store,
since a store hit already proves the bytes hash to the recorded key. Prefer
that: a store hit is itself the integrity proof, so the check is only needed on
the download path. If you take the cheaper route, say so in a comment, because
a reader will otherwise wonder why the guarantee looks partial.

- [ ] **Step 3: Record the behaviour change**

`jerky install lodash` used to write `"4.17.21"` and now writes `"^4.17.21"`.
Add a note wherever user-facing changes are recorded. If nothing exists yet,
this is a good moment to start a `CHANGELOG.md` — it is the first change that
alters output for an existing user.

- [ ] **Step 4: Full verification**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: all PASS.

- [ ] **Step 5: Verify against the real registry**

```bash
mkdir -p /tmp/jerky-spec2 && cd /tmp/jerky-spec2
cargo run --manifest-path "$OLDPWD/Cargo.toml" -- init
cargo run --manifest-path "$OLDPWD/Cargo.toml" -- install express
node -e "console.log(typeof require('express'))"
```

Expected: `resolved N packages` with N in the fifties, then `function`. Confirm
that `node_modules/` holds only `express` and `.jerky`, that
`jerky-lock.json` has an entry per resolved package, and that a second
`install express` makes no packument request.

`express` is the right smoke test because it is the design's own worked
example and has a real transitive tree without being enormous.

- [ ] **Step 6: Commit**

```bash
git add src/ tests/ CHANGELOG.md
git commit -m "feat: caret ranges, lockfile reuse, and integrity authority"
```

---

## Self-Review

Before opening the PR, check each of these against the diff rather than from memory.

- **Does anything outside `src/range.rs` name `js_semver`?** `grep -rn 'js_semver' src/ tests/` should return `range.rs` only.
- **Is there a `HashMap` on any path that reaches the lockfile?** The resolver's caches may use one — they are not serialized. Anything in `ResolvedGraph` or `OnDisk` must be `BTreeMap`.
- **Does the resolver touch the filesystem?** `grep -n 'std::fs\|env::' src/resolver.rs` should return nothing.
- **Is `devDependencies` read anywhere?** It should not be, and `VersionMetadata` should not have a field for it.
- **Does the conformance suite actually run?** A suite that silently passes on zero cases is worse than none — the length assertion guards this.
- **Is the caret change noted for users?** It changes output for anyone who already installed with spec 1.
- **Do the spec 1 tests still pass unmodified**, apart from the single manifest assertion the caret change moves? Anything else moving is a regression wearing a costume.
- **Does an interrupted install leave a lockfile?** It must not; that is the phase 4 ordering.

## Deferred

- **Parallel downloads — #33.** Until it lands, large trees install visibly
  slower than npm, which is the accepted cost of this spec.
- **Peer dependencies — #34.** jerky will install trees npm would warn about.
- **Lockfile pruning**, unfiled: there is no operation that would trigger it
  until an `uninstall` command exists.
