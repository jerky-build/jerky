//! Lockfile tests.
//!
//! Each maps to one of the four properties #11 asked for: deterministic key
//! ordering, minimal diff noise, a format version field from day one, and an
//! integrity hash per package.

use std::collections::BTreeMap;

use jerky::lockfile::{self, LockfileError};
use jerky::resolver::resolve;
use jerky::testing::FixtureRegistry;
use tempfile::TempDir;

fn roots(list: &[(&str, &str)]) -> BTreeMap<String, String> {
    list.iter()
        .map(|(n, r)| (n.to_string(), r.to_string()))
        .collect()
}

fn small_tree() -> FixtureRegistry {
    FixtureRegistry::new().with_tree(&[("a", "1.0.0", &[("b", "^1.0.0")]), ("b", "1.0.0", &[])])
}

fn read(dir: &TempDir) -> String {
    std::fs::read_to_string(dir.path().join("jerky-lock.json")).unwrap()
}

#[test]
fn writing_the_same_graph_twice_is_byte_identical() {
    // #11's first requirement. Two machines resolving the same tree must
    // produce identical files or the lockfile churns in every diff.
    let registry = small_tree();
    let dir = TempDir::new().unwrap();

    let first_graph = resolve(&registry, &roots(&[("a", "^1.0.0")])).unwrap();
    lockfile::save(&first_graph, dir.path()).unwrap();
    let first = read(&dir);

    let second_graph = resolve(&registry, &roots(&[("a", "^1.0.0")])).unwrap();
    lockfile::save(&second_graph, dir.path()).unwrap();
    let second = read(&dir);

    assert_eq!(first, second);
}

#[test]
fn keys_are_written_in_sorted_order() {
    // The stronger form of the requirement above. Resolving twice in one
    // process would still agree if ordering came from the walk; asserting the
    // keys are sorted proves the ordering is structural, which is what makes
    // two *machines* agree.
    let registry = FixtureRegistry::new().with_tree(&[
        ("zeta", "1.0.0", &[]),
        ("alpha", "1.0.0", &[("mid", "^1.0.0"), ("zeta", "^1.0.0")]),
        ("mid", "1.0.0", &[("zeta", "^1.0.0")]),
    ]);
    let dir = TempDir::new().unwrap();

    let graph = resolve(&registry, &roots(&[("alpha", "^1.0.0")])).unwrap();
    lockfile::save(&graph, dir.path()).unwrap();

    let parsed: serde_json::Value = serde_json::from_str(&read(&dir)).unwrap();
    let keys: Vec<&str> = parsed["packages"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();

    // Discovery order is alpha, mid, zeta only by luck; sorted order is a
    // guarantee. Written out literally so the test fails loudly if the map
    // type ever changes.
    assert_eq!(keys, ["alpha@1.0.0", "mid@1.0.0", "zeta@1.0.0"]);

    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted, "package keys are not sorted");
}

#[test]
fn a_package_without_dependencies_omits_the_key() {
    // Diff noise again: an empty object on every leaf is three lines of
    // nothing in a file people read in review.
    let registry = small_tree();
    let dir = TempDir::new().unwrap();
    let graph = resolve(&registry, &roots(&[("a", "^1.0.0")])).unwrap();
    lockfile::save(&graph, dir.path()).unwrap();

    let parsed: serde_json::Value = serde_json::from_str(&read(&dir)).unwrap();
    assert!(parsed["packages"]["b@1.0.0"]["dependencies"].is_null());
}

#[test]
fn round_trips_through_disk() {
    let registry = small_tree();
    let dir = TempDir::new().unwrap();

    let graph = resolve(&registry, &roots(&[("a", "^1.0.0")])).unwrap();
    lockfile::save(&graph, dir.path()).unwrap();
    let back = lockfile::load(dir.path())
        .unwrap()
        .expect("a lockfile was written");

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
    // #11's "minimal diff noise". A flat, name@version-keyed map is what makes
    // this true; a nested tree would reindent everything below a change.
    let registry = FixtureRegistry::new().with_tree(&[
        ("a", "1.0.0", &[("b", "^1.0.0")]),
        ("b", "1.0.0", &[]),
        ("newcomer", "1.0.0", &[]),
    ]);
    let dir = TempDir::new().unwrap();

    let before_graph = resolve(&registry, &roots(&[("a", "^1.0.0")])).unwrap();
    lockfile::save(&before_graph, dir.path()).unwrap();
    let before = read(&dir);

    let after_graph = resolve(
        &registry,
        &roots(&[("a", "^1.0.0"), ("newcomer", "^1.0.0")]),
    )
    .unwrap();
    lockfile::save(&after_graph, dir.path()).unwrap();
    let after = read(&dir);

    let removed: Vec<&str> = before.lines().filter(|l| !after.contains(*l)).collect();
    assert!(
        removed.len() <= 1,
        "adding a dependency rewrote {} existing lines; only the root block should change: {removed:?}",
        removed.len()
    );
}

#[test]
fn a_missing_lockfile_is_not_an_error() {
    // The first install has none. Absence is normal, not a failure.
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
    assert!(matches!(
        lockfile::load(dir.path()),
        Err(LockfileError::Malformed { .. })
    ));
}

#[test]
fn an_unusable_integrity_hash_is_refused() {
    // The hash is what makes this a security artifact rather than a cache, so
    // a lockfile carrying one jerky cannot parse is not usable.
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("jerky-lock.json"),
        r#"{"lockfileVersion": 1, "root": {},
            "packages": { "a@1.0.0": {
                "version": "1.0.0",
                "resolved": "https://r.test/a.tgz",
                "integrity": "not-a-hash"
            }}}"#,
    )
    .unwrap();

    assert!(matches!(
        lockfile::load(dir.path()),
        Err(LockfileError::BadIntegrity { .. })
    ));
}

#[test]
fn a_key_that_is_not_name_at_version_is_refused() {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("jerky-lock.json"),
        r#"{"lockfileVersion": 1, "root": {},
            "packages": { "no-at-sign": {
                "version": "1.0.0",
                "resolved": "https://r.test/a.tgz",
                "integrity": "sha512-z4PhNX7vuL3xVChQ1m2AB9Yg5AULVxXcg/SpIdNs6c5H0NE8XYXysP+DGNKHfuwvY7kxvUdBeoGlODJ6+SfaPg=="
            }}}"#,
    )
    .unwrap();

    assert!(matches!(
        lockfile::load(dir.path()),
        Err(LockfileError::BadKey { .. })
    ));
}

#[test]
fn a_scoped_package_key_splits_on_the_last_at() {
    // Scoped installs are spec 3, but the key format has to survive them or
    // the lockfile needs a migration when they land. `@types/node@20.1.0` must
    // yield the name `@types/node`, not `@types`.
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("jerky-lock.json"),
        r#"{"lockfileVersion": 1, "root": {},
            "packages": { "@types/node@20.1.0": {
                "version": "20.1.0",
                "resolved": "https://r.test/n.tgz",
                "integrity": "sha512-z4PhNX7vuL3xVChQ1m2AB9Yg5AULVxXcg/SpIdNs6c5H0NE8XYXysP+DGNKHfuwvY7kxvUdBeoGlODJ6+SfaPg=="
            }}}"#,
    )
    .unwrap();

    let graph = lockfile::load(dir.path()).unwrap().unwrap();
    let id = graph.packages.keys().next().unwrap();
    assert_eq!(id.name, "@types/node");
    assert_eq!(id.version, "20.1.0");
}

#[test]
fn the_file_ends_with_a_newline() {
    // So it is a well-formed text file and diffs do not show "\ No newline".
    let registry = small_tree();
    let dir = TempDir::new().unwrap();
    let graph = resolve(&registry, &roots(&[("a", "^1.0.0")])).unwrap();
    lockfile::save(&graph, dir.path()).unwrap();

    assert!(read(&dir).ends_with('\n'));
}

#[test]
fn the_lockfile_records_edges_and_integrity() {
    // #11's fourth requirement: resolved version, integrity hash, tarball URL,
    // and the dependency edges.
    let registry = small_tree();
    let dir = TempDir::new().unwrap();
    let graph = resolve(&registry, &roots(&[("a", "^1.0.0")])).unwrap();
    lockfile::save(&graph, dir.path()).unwrap();

    let parsed: serde_json::Value = serde_json::from_str(&read(&dir)).unwrap();
    let a = &parsed["packages"]["a@1.0.0"];
    assert_eq!(a["version"], "1.0.0");
    assert!(a["resolved"].as_str().unwrap().starts_with("https://"));
    assert!(a["integrity"].as_str().unwrap().starts_with("sha512-"));
    // The edge records the concrete version chosen, not the range asked for.
    assert_eq!(a["dependencies"]["b"], "1.0.0");
    // And the root records the range, which is what makes staleness detectable.
    assert_eq!(parsed["root"]["a"], "^1.0.0");
}
