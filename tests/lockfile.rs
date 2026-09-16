//! Lockfile tests.
//!
//! Each maps to one of the four properties #11 asked for: deterministic key
//! ordering, minimal diff noise, a format version field from day one, and an
//! integrity hash per package.
//!
//! Version 2 adds a fifth, from #103: a node is identified by what it records
//! rather than by the key it is recorded under. The key carries a peer suffix
//! now, and a suffix that can be a hash is one nothing can parse back — so the
//! tests that matter most here are the ones asking whether two keys can land
//! on one node, and whether a file read back writes itself out unchanged.

use std::collections::BTreeMap;

use jerky::lockfile::{self, LOCKFILE_NAME, LockfileError};
use jerky::resolver::{Declared, ImporterPath, Kind, resolve};
use jerky::testing::FixtureRegistry;
use tempfile::TempDir;

/// A workspace of one, keyed `.`. These tests predate multiple importers and
/// read unchanged: one importer is the degenerate case of the general input.
fn roots(list: &[(&str, &str)]) -> BTreeMap<ImporterPath, BTreeMap<String, Declared>> {
    BTreeMap::from([(ImporterPath::root(), section(list, Kind::Prod))])
}

/// One manifest section as the resolver takes it.
fn section(list: &[(&str, &str)], kind: Kind) -> BTreeMap<String, Declared> {
    list.iter()
        .map(|(name, specifier)| {
            (
                name.to_string(),
                Declared {
                    specifier: specifier.to_string(),
                    kind,
                },
            )
        })
        .collect()
}

/// No `workspace:` dependencies, which is every test here.
fn no_members() -> BTreeMap<String, ImporterPath> {
    BTreeMap::new()
}

fn small_tree() -> FixtureRegistry {
    FixtureRegistry::new().with_tree(&[("a", "1.0.0", &[("b", "^1.0.0")]), ("b", "1.0.0", &[])])
}

fn read(dir: &TempDir) -> String {
    std::fs::read_to_string(dir.path().join(LOCKFILE_NAME)).unwrap()
}

#[test]
fn writing_the_same_graph_twice_is_byte_identical() {
    // #11's first requirement. Two machines resolving the same tree must
    // produce identical files or the lockfile churns in every diff.
    let registry = small_tree();
    let dir = TempDir::new().unwrap();

    let first_graph = resolve(&registry, &roots(&[("a", "^1.0.0")]), &no_members()).unwrap();
    lockfile::save(&first_graph, dir.path()).unwrap();
    let first = read(&dir);

    let second_graph = resolve(&registry, &roots(&[("a", "^1.0.0")]), &no_members()).unwrap();
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

    let graph = resolve(&registry, &roots(&[("alpha", "^1.0.0")]), &no_members()).unwrap();
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
    let graph = resolve(&registry, &roots(&[("a", "^1.0.0")]), &no_members()).unwrap();
    lockfile::save(&graph, dir.path()).unwrap();

    let parsed: serde_json::Value = serde_json::from_str(&read(&dir)).unwrap();
    assert!(parsed["packages"]["b@1.0.0"]["dependencies"].is_null());
}

#[test]
fn round_trips_through_disk() {
    let registry = small_tree();
    let dir = TempDir::new().unwrap();

    let graph = resolve(&registry, &roots(&[("a", "^1.0.0")]), &no_members()).unwrap();
    lockfile::save(&graph, dir.path()).unwrap();
    let back = lockfile::load(dir.path())
        .unwrap()
        .expect("a lockfile was written");

    assert_eq!(back.importers.len(), graph.importers.len());
    for (path, importer) in &graph.importers {
        let other = back.importers.get(path).expect("every importer survives");
        assert_eq!(other.dependencies.len(), importer.dependencies.len());
        for (name, dependency) in &importer.dependencies {
            let round_tripped = &other.dependencies[name];
            assert_eq!(round_tripped.specifier, dependency.specifier);
            assert_eq!(round_tripped.resolution, dependency.resolution);
        }
    }
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

    let before_graph = resolve(&registry, &roots(&[("a", "^1.0.0")]), &no_members()).unwrap();
    lockfile::save(&before_graph, dir.path()).unwrap();
    let before = read(&dir);

    let after_graph = resolve(
        &registry,
        &roots(&[("a", "^1.0.0"), ("newcomer", "^1.0.0")]),
        &no_members(),
    )
    .unwrap();
    lockfile::save(&after_graph, dir.path()).unwrap();
    let after = read(&dir);

    let removed: Vec<&str> = before.lines().filter(|l| !after.contains(*l)).collect();
    assert!(
        removed.len() <= 1,
        "adding a dependency rewrote {} existing lines; only the importer block should change: {removed:?}",
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
        dir.path().join(LOCKFILE_NAME),
        r#"{"lockfileVersion": 99, "importers": {}, "packages": {}}"#,
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
    std::fs::write(dir.path().join(LOCKFILE_NAME), "{ not json").unwrap();
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
        dir.path().join(LOCKFILE_NAME),
        r#"{"lockfileVersion": 2, "importers": {},
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
        dir.path().join(LOCKFILE_NAME),
        r#"{"lockfileVersion": 2, "importers": {},
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
        dir.path().join(LOCKFILE_NAME),
        r#"{"lockfileVersion": 2, "importers": {},
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
    let graph = resolve(&registry, &roots(&[("a", "^1.0.0")]), &no_members()).unwrap();
    lockfile::save(&graph, dir.path()).unwrap();

    assert!(read(&dir).ends_with('\n'));
}

#[test]
fn the_lockfile_records_edges_and_integrity() {
    // #11's fourth requirement: resolved version, integrity hash, tarball URL,
    // and the dependency edges.
    let registry = small_tree();
    let dir = TempDir::new().unwrap();
    let graph = resolve(&registry, &roots(&[("a", "^1.0.0")]), &no_members()).unwrap();
    lockfile::save(&graph, dir.path()).unwrap();

    let parsed: serde_json::Value = serde_json::from_str(&read(&dir)).unwrap();
    let a = &parsed["packages"]["a@1.0.0"];
    assert_eq!(a["version"], "1.0.0");
    assert!(a["resolved"].as_str().unwrap().starts_with("https://"));
    assert!(a["integrity"].as_str().unwrap().starts_with("sha512-"));
    // The edge records the concrete version chosen, not the range asked for.
    assert_eq!(a["dependencies"]["b"], "1.0.0");
    // And the root importer records the range it asked for alongside the
    // version that satisfied it.
    let declared = &parsed["importers"]["."]["dependencies"]["a"];
    assert_eq!(declared["specifier"], "^1.0.0");
    assert_eq!(declared["version"], "1.0.0");
}

/// A valid sha512 SSRI string. Content is irrelevant; it only has to parse.
const HASH: &str = "sha512-z4PhNX7vuL3xVChQ1m2AB9Yg5AULVxXcg/SpIdNs6c5H0NE8XYXysP+DGNKHfuwvY7kxvUdBeoGlODJ6+SfaPg==";

fn write_raw(dir: &TempDir, body: &str) {
    std::fs::write(dir.path().join(LOCKFILE_NAME), body).unwrap();
}

#[test]
fn a_key_disagreeing_with_its_entry_is_refused() {
    // Two distinct keys whose entries both claim 1.0.0. Taking the name from
    // the key and the version from the entry would collapse them into one
    // package, silently dropping a dependency and keeping the wrong tarball
    // URL. Exactly what hand-editing a lockfile produces.
    let dir = TempDir::new().unwrap();
    write_raw(
        &dir,
        &format!(
            r#"{{"lockfileVersion":2,"importers":{{}},"packages":{{
                "b@1.0.0": {{"version":"1.0.0","resolved":"https://r.test/u1.tgz","integrity":"{HASH}"}},
                "b@2.0.0": {{"version":"1.0.0","resolved":"https://r.test/u2.tgz","integrity":"{HASH}"}}
            }}}}"#
        ),
    );

    assert!(matches!(
        lockfile::load(dir.path()),
        Err(LockfileError::KeyVersionMismatch { .. })
    ));
}

#[test]
fn an_edge_pointing_at_nothing_is_refused() {
    // A graph with a dangling edge cannot be installed. Saying so beats
    // discovering it halfway through linking.
    let dir = TempDir::new().unwrap();
    write_raw(
        &dir,
        &format!(
            r#"{{"lockfileVersion":2,"importers":{{}},"packages":{{
                "a@1.0.0": {{"version":"1.0.0","resolved":"https://r.test/a.tgz","integrity":"{HASH}",
                             "dependencies":{{"ghost":"9.9.9"}}}}
            }}}}"#
        ),
    );

    match lockfile::load(dir.path()) {
        Err(LockfileError::DanglingEdge {
            entry, dependency, ..
        }) => {
            assert_eq!(entry, "a@1.0.0");
            assert_eq!(dependency, "ghost@9.9.9");
        }
        other => panic!("expected a dangling edge error, got {other:?}"),
    }
}

#[test]
fn an_edge_resolved_later_in_the_file_is_accepted() {
    // The dangling check must run after every node is known: `a` depends on
    // `b`, which sorts after it. A per-entry check would reject this.
    let dir = TempDir::new().unwrap();
    write_raw(
        &dir,
        &format!(
            r#"{{"lockfileVersion":2,"importers":{{}},"packages":{{
                "a@1.0.0": {{"version":"1.0.0","resolved":"https://r.test/a.tgz","integrity":"{HASH}",
                             "dependencies":{{"b":"1.0.0"}}}},
                "b@1.0.0": {{"version":"1.0.0","resolved":"https://r.test/b.tgz","integrity":"{HASH}"}}
            }}}}"#
        ),
    );

    let graph = lockfile::load(dir.path()).unwrap().unwrap();
    assert_eq!(graph.packages.len(), 2);
}

#[test]
fn a_future_format_says_upgrade_rather_than_malformed() {
    // A plausible v3 with entirely different field names. It is valid JSON, so
    // reporting a syntax error would send the user hunting for one.
    let dir = TempDir::new().unwrap();
    write_raw(
        &dir,
        r#"{"lockfileVersion":3,"importers":{},"snapshots":{}}"#,
    );

    match lockfile::load(dir.path()) {
        Err(LockfileError::UnsupportedVersion {
            found, supported, ..
        }) => {
            assert_eq!((found, supported), (3, lockfile::LOCKFILE_VERSION));
        }
        other => panic!("expected an upgrade message, got {other:?}"),
    }
}

#[test]
fn a_file_without_a_version_field_is_malformed() {
    // Distinct from an unknown version: there is nothing to act on.
    let dir = TempDir::new().unwrap();
    write_raw(&dir, r#"{"importers":{},"packages":{}}"#);

    assert!(matches!(
        lockfile::load(dir.path()),
        Err(LockfileError::Malformed { .. })
    ));
}

/// Build a two-importer workspace by hand.
///
/// The resolver still seeds from a single root — widening its input is Task 7 —
/// but the *format* has to carry multiple importers before then, and a format
/// only ever exercised with one importer is not exercising this at all.
fn two_importer_graph() -> jerky::resolver::ResolvedGraph {
    use jerky::resolver::{Dependency, Importer, ImporterPath, Resolution};

    let registry = small_tree();
    let mut graph = resolve(&registry, &roots(&[("a", "^1.0.0")]), &no_members()).unwrap();

    let a_id = graph
        .packages
        .keys()
        .find(|id| id.name == "a")
        .cloned()
        .unwrap();

    let mut web = Importer::default();
    web.dependencies.insert(
        "a".to_string(),
        Dependency {
            specifier: "^1.0.0".to_string(),
            kind: Kind::Prod,
            resolution: Resolution::Registry(a_id),
        },
    );
    // A workspace member: linked in place, relative to the importer.
    web.dependencies.insert(
        "ui".to_string(),
        Dependency {
            specifier: "workspace:*".to_string(),
            kind: Kind::Prod,
            resolution: Resolution::Local(std::path::PathBuf::from("../../packages/ui")),
        },
    );

    graph
        .importers
        .insert(ImporterPath::new("apps/web").unwrap(), web);
    graph
}

#[test]
fn importers_are_keyed_by_directory_and_sorted() {
    let dir = TempDir::new().unwrap();
    lockfile::save(&two_importer_graph(), dir.path()).unwrap();

    let parsed: serde_json::Value = serde_json::from_str(&read(&dir)).unwrap();
    let keys: Vec<&str> = parsed["importers"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, [".", "apps/web"]);
}

#[test]
fn a_dependency_records_both_what_was_asked_and_what_was_chosen() {
    let dir = TempDir::new().unwrap();
    lockfile::save(&two_importer_graph(), dir.path()).unwrap();

    let parsed: serde_json::Value = serde_json::from_str(&read(&dir)).unwrap();
    let entry = &parsed["importers"]["."]["dependencies"]["a"];
    assert_eq!(entry["specifier"], "^1.0.0");
    assert_eq!(entry["version"], "1.0.0");
}

#[test]
fn the_lockfile_records_dev_dependencies_in_their_own_block() {
    // Asserted on the parsed JSON rather than on a struct: the on-disk shape
    // is the decision being pinned, and it is pnpm's. Two importers, one of
    // each kind, because the empty-block half of the claim needs an importer
    // that has no devDependencies at all.
    let registry = FixtureRegistry::new().with_tree(&[("a", "1.0.0", &[]), ("b", "1.0.0", &[])]);
    let dir = TempDir::new().unwrap();

    let graph = resolve(
        &registry,
        &BTreeMap::from([
            (
                ImporterPath::root(),
                section(&[("a", "^1.0.0")], Kind::Prod),
            ),
            (
                ImporterPath::new("packages/ui").unwrap(),
                section(&[("b", "^1.0.0")], Kind::Dev),
            ),
        ]),
        &no_members(),
    )
    .unwrap();
    lockfile::save(&graph, dir.path()).unwrap();

    let parsed: serde_json::Value = serde_json::from_str(&read(&dir)).unwrap();
    let ui = &parsed["importers"]["packages/ui"];
    assert_eq!(ui["devDependencies"]["b"]["specifier"], "^1.0.0");
    assert_eq!(ui["devDependencies"]["b"]["version"], "1.0.0");
    assert!(
        ui["dependencies"].is_null(),
        "a devDependency was written into the production block as well"
    );
    assert!(
        parsed["importers"]["."]["devDependencies"].is_null(),
        "an importer with no devDependencies grew an empty block"
    );

    // And the kind survives the round trip, which is what makes staleness
    // detectable after a restart rather than only within one process.
    let back = lockfile::load(dir.path()).unwrap().unwrap();
    let ui = &back.importers[&ImporterPath::new("packages/ui").unwrap()];
    assert_eq!(ui.dependencies["b"].kind, Kind::Dev);
    assert_eq!(
        back.importers[&ImporterPath::root()].dependencies["a"].kind,
        Kind::Prod
    );
}

#[test]
fn a_local_dependency_records_a_link_rather_than_a_version() {
    let dir = TempDir::new().unwrap();
    lockfile::save(&two_importer_graph(), dir.path()).unwrap();

    let parsed: serde_json::Value = serde_json::from_str(&read(&dir)).unwrap();
    let entry = &parsed["importers"]["apps/web"]["dependencies"]["ui"];
    assert_eq!(entry["specifier"], "workspace:*");
    assert_eq!(entry["version"], "link:../../packages/ui");
}

#[test]
fn a_local_dependency_has_no_packages_entry() {
    // No tarball and no integrity hash, because there is nothing to verify —
    // the bytes are in the repo.
    let dir = TempDir::new().unwrap();
    lockfile::save(&two_importer_graph(), dir.path()).unwrap();

    let parsed: serde_json::Value = serde_json::from_str(&read(&dir)).unwrap();
    assert!(
        parsed["packages"]
            .as_object()
            .unwrap()
            .keys()
            .all(|key| !key.starts_with("ui@")),
        "a linked workspace member was given a packages entry"
    );
}

#[test]
fn a_multi_importer_graph_round_trips() {
    use jerky::resolver::{ImporterPath, Resolution};

    let dir = TempDir::new().unwrap();
    let graph = two_importer_graph();
    lockfile::save(&graph, dir.path()).unwrap();

    let back = lockfile::load(dir.path()).unwrap().unwrap();

    assert_eq!(back.importers.len(), 2);
    let web = &back.importers[&ImporterPath::new("apps/web").unwrap()];
    assert_eq!(web.dependencies["a"].specifier, "^1.0.0");
    assert!(matches!(
        &web.dependencies["ui"].resolution,
        Resolution::Local(path) if path == std::path::Path::new("../../packages/ui")
    ));
}

#[test]
fn an_importer_path_escaping_the_workspace_is_refused() {
    // A hand-edited key naming a directory outside the workspace. Same class
    // as the tar-slip guard in `archive`: an untrusted path that resolves
    // outside the tree it claims to describe.
    let dir = TempDir::new().unwrap();
    write_raw(
        &dir,
        r#"{"lockfileVersion":2,"importers":{"../escape":{}},"packages":{}}"#,
    );

    assert!(matches!(
        lockfile::load(dir.path()),
        Err(LockfileError::BadImporter { .. })
    ));
}

#[test]
fn an_absolute_importer_path_is_refused() {
    let dir = TempDir::new().unwrap();
    write_raw(
        &dir,
        r#"{"lockfileVersion":2,"importers":{"/etc":{}},"packages":{}}"#,
    );

    assert!(matches!(
        lockfile::load(dir.path()),
        Err(LockfileError::BadImporter { .. })
    ));
}

#[test]
fn an_importer_dependency_naming_no_package_is_refused() {
    // The importer half needs the same guarantee the package edges already
    // have: a dependency pointing at nothing must not load as a real node.
    let dir = TempDir::new().unwrap();
    write_raw(
        &dir,
        &format!(
            r#"{{"lockfileVersion":2,
                "importers":{{".":{{"dependencies":{{"ghost":{{"specifier":"^1.0.0","version":"9.9.9"}}}}}}}},
                "packages":{{"a@1.0.0":{{"version":"1.0.0","resolved":"https://r.test/a.tgz","integrity":"{HASH}"}}}}}}"#
        ),
    );

    assert!(matches!(
        lockfile::load(dir.path()),
        Err(LockfileError::UnknownImporterDependency { .. })
    ));
}

#[test]
fn an_alias_round_trips() {
    // `execa` declared as `npm:safe-execa@0.3.0` is real — it is in pnpm's own
    // lockfile. The package it resolves to is named `safe-execa`, NOT `execa`,
    // which is the whole point: a test whose package entry is keyed `execa@…`
    // is not testing an alias at all.
    use jerky::resolver::{ImporterPath, Resolution};

    let dir = TempDir::new().unwrap();
    write_raw(
        &dir,
        &format!(
            r#"{{"lockfileVersion":2,
                "importers":{{".":{{"dependencies":{{"execa":{{"specifier":"npm:safe-execa@0.3.0","version":"safe-execa@0.3.0"}}}}}}}},
                "packages":{{"safe-execa@0.3.0":{{"version":"0.3.0","resolved":"https://r.test/se.tgz","integrity":"{HASH}"}}}}}}"#
        ),
    );

    let graph = lockfile::load(dir.path()).unwrap().unwrap();
    let root = &graph.importers[&ImporterPath::root()];
    let execa = &root.dependencies["execa"];

    assert_eq!(execa.specifier, "npm:safe-execa@0.3.0");
    match &execa.resolution {
        Resolution::Registry(id) => {
            assert_eq!(
                id.name, "safe-execa",
                "the resolution must name the real package"
            );
            assert_eq!(id.version, "0.3.0");
        }
        other => panic!("expected a registry resolution, got {other:?}"),
    }
}

#[test]
fn an_alias_survives_a_save_and_load_cycle() {
    // The half the raw-JSON test cannot cover: that `save` writes a form
    // `load` can read back. Writing only the version would lose the name.
    use jerky::resolver::{Dependency, Importer, ImporterPath, PackageId, Resolution};

    let dir = TempDir::new().unwrap();
    let registry = FixtureRegistry::new().with_tree(&[("safe-execa", "0.3.0", &[])]);
    let mut graph = resolve(
        &registry,
        &roots(&[("safe-execa", "^0.3.0")]),
        &no_members(),
    )
    .unwrap();

    let real = PackageId::plain("safe-execa", "0.3.0");
    let mut importer = Importer::default();
    importer.dependencies.insert(
        "execa".to_string(),
        Dependency {
            specifier: "npm:safe-execa@0.3.0".to_string(),
            kind: Kind::Prod,
            resolution: Resolution::Registry(real.clone()),
        },
    );
    graph.importers.insert(ImporterPath::root(), importer);

    lockfile::save(&graph, dir.path()).unwrap();

    // The name is written because it differs from the key.
    let parsed: serde_json::Value = serde_json::from_str(&read(&dir)).unwrap();
    assert_eq!(
        parsed["importers"]["."]["dependencies"]["execa"]["version"],
        "safe-execa@0.3.0"
    );

    let back = lockfile::load(dir.path()).unwrap().unwrap();
    let execa = &back.importers[&ImporterPath::root()].dependencies["execa"];
    assert!(matches!(&execa.resolution, Resolution::Registry(id) if id == &real));
}

#[test]
fn an_ordinary_dependency_does_not_repeat_its_name() {
    // The common case stays terse: the name is written only when it differs.
    let dir = TempDir::new().unwrap();
    lockfile::save(&two_importer_graph(), dir.path()).unwrap();

    let parsed: serde_json::Value = serde_json::from_str(&read(&dir)).unwrap();
    assert_eq!(
        parsed["importers"]["."]["dependencies"]["a"]["version"],
        "1.0.0"
    );
}

#[test]
fn a_link_target_leaving_the_workspace_is_refused() {
    // A link climbs out of its importer legitimately — that is how apps/web
    // reaches packages/ui — but must not climb out of the workspace.
    let dir = TempDir::new().unwrap();
    write_raw(
        &dir,
        r#"{"lockfileVersion":2,
            "importers":{".":{"dependencies":{"evil":{"specifier":"workspace:*","version":"link:../../../etc"}}}},
            "packages":{}}"#,
    );

    assert!(matches!(
        lockfile::load(dir.path()),
        Err(LockfileError::LinkEscapesWorkspace { .. })
    ));
}

#[test]
fn a_link_target_climbing_within_the_workspace_is_accepted() {
    // apps/web sits two deep, so ../../packages/ui lands back inside.
    use jerky::resolver::Resolution;

    let dir = TempDir::new().unwrap();
    write_raw(
        &dir,
        r#"{"lockfileVersion":2,
            "importers":{"apps/web":{"dependencies":{"ui":{"specifier":"workspace:*","version":"link:../../packages/ui"}}}},
            "packages":{}}"#,
    );

    let graph = lockfile::load(dir.path()).unwrap().unwrap();
    let web = &graph.importers[&jerky::resolver::ImporterPath::new("apps/web").unwrap()];
    assert!(matches!(
        &web.dependencies["ui"].resolution,
        Resolution::Local(_)
    ));
}

#[test]
fn equivalent_importer_spellings_collapse_to_one_key() {
    // `./packages/ui` and `packages/ui` are one directory. Keeping them apart
    // would give that directory two importers and, once linking exists, two
    // node_modules.
    use jerky::resolver::ImporterPath;

    assert_eq!(
        ImporterPath::new("./packages/ui").unwrap(),
        ImporterPath::new("packages/ui").unwrap()
    );
    assert_eq!(
        ImporterPath::new("packages/ui/").unwrap(),
        ImporterPath::new("packages/ui").unwrap()
    );
}

#[test]
fn one_stale_importer_does_not_invalidate_the_others() {
    // The reason staleness is per-importer rather than whole-file, and the
    // load-bearing justification for the whole reshape. `apps/web` edits its
    // manifest; the root's recorded specifiers are untouched and still match.
    use jerky::resolver::ImporterPath;

    let dir = TempDir::new().unwrap();
    lockfile::save(&two_importer_graph(), dir.path()).unwrap();
    let back = lockfile::load(dir.path()).unwrap().unwrap();

    // What a manifest now declares, per importer.
    let root_declares = [("a", "^1.0.0")];
    let web_declares = [("a", "^2.0.0"), ("ui", "workspace:*")]; // `a` was bumped

    let matches_manifest = |importer: &ImporterPath, declared: &[(&str, &str)]| {
        let recorded = &back.importers[importer].dependencies;
        recorded.len() == declared.len()
            && declared
                .iter()
                .all(|(name, spec)| recorded.get(*name).is_some_and(|d| d.specifier == *spec))
    };

    assert!(
        matches_manifest(&ImporterPath::root(), &root_declares),
        "the root importer should still be current"
    );
    assert!(
        !matches_manifest(&ImporterPath::new("apps/web").unwrap(), &web_declares),
        "apps/web should be detected as stale"
    );
}

#[test]
fn a_transitive_alias_survives_the_round_trip() {
    // The gap this test was written for: an importer's alias round-tripped
    // correctly long before one could be produced, but a *package's* alias was
    // written as a bare version and read back under the local name — turning
    // `width-cjs -> string-width@4.2.3` into `width-cjs@4.2.3`, which is not a
    // package any registry serves.
    let registry = FixtureRegistry::new().with_tree(&[
        (
            "cliui",
            "1.0.0",
            &[("width-cjs", "npm:string-width@^4.0.0")],
        ),
        ("string-width", "4.2.3", &[]),
    ]);
    let dir = TempDir::new().unwrap();

    let graph = resolve(&registry, &roots(&[("cliui", "^1.0.0")]), &no_members()).unwrap();
    lockfile::save(&graph, dir.path()).unwrap();

    // The real name is on disk, not inferred. Without it the entry is
    // unidentifiable, since the key is the name the dependent chose.
    let written = read(&dir);
    assert!(
        written.contains("string-width@4.2.3"),
        "the alias target's name was not recorded: {written}"
    );

    let back = lockfile::load(dir.path()).unwrap().unwrap();
    let cliui = back
        .packages
        .values()
        .find(|package| package.id.name == "cliui")
        .expect("cliui was recorded");
    let aliased = &cliui.dependencies["width-cjs"];
    assert_eq!(aliased.name, "string-width");
    assert_eq!(aliased.version, "4.2.3");
    assert!(
        back.packages.contains_key(aliased),
        "the edge read back points at no recorded package"
    );
}

#[test]
fn an_importers_alias_survives_the_round_trip() {
    let registry = FixtureRegistry::new().with_tree(&[("string-width", "4.2.3", &[])]);
    let dir = TempDir::new().unwrap();

    let graph = resolve(
        &registry,
        &roots(&[("width-cjs", "npm:string-width@^4.0.0")]),
        &no_members(),
    )
    .unwrap();
    lockfile::save(&graph, dir.path()).unwrap();
    let back = lockfile::load(dir.path()).unwrap().unwrap();

    let dependency = &back.importers[&ImporterPath::root()].dependencies["width-cjs"];
    assert_eq!(
        dependency.specifier, "npm:string-width@^4.0.0",
        "the specifier was normalised, so an edit to it would read as no change"
    );
    match &dependency.resolution {
        jerky::resolver::Resolution::Registry(id) => {
            assert_eq!(id.to_string(), "string-width@4.2.3")
        }
        other => panic!("expected a registry resolution, got {other:?}"),
    }
}

/// What version 1 wrote for [`small_tree`], character for character.
///
/// Embedded rather than generated, because the claim is about a format that no
/// longer exists to generate from: a graph with no peers in it must write under
/// version 2 exactly what it wrote under version 1. Peers are a change to the
/// file for the packages that have them and to nothing else, and every package
/// that has none — which is nearly all of them — must diff as one line.
const VERSION_ONE_SMALL_TREE: &str = r#"{
  "lockfileVersion": 1,
  "importers": {
    ".": {
      "dependencies": {
        "a": {
          "specifier": "^1.0.0",
          "version": "1.0.0"
        }
      }
    }
  },
  "packages": {
    "a@1.0.0": {
      "version": "1.0.0",
      "resolved": "https://fixture.test/a/-/a-1.0.0.tgz",
      "integrity": "sha512-C4xmM0847OI4IsgerHv7mvHZYmzDKCEfqFyjjGgFckN7oERLIonogjsb3Mm7TOkjWEbZFKx+WYG30Wol+CpqBQ==",
      "dependencies": {
        "b": "1.0.0"
      }
    },
    "b@1.0.0": {
      "version": "1.0.0",
      "resolved": "https://fixture.test/b/-/b-1.0.0.tgz",
      "integrity": "sha512-1rWZDraHgQcWA94G7WXeMd4PhVFBjGUMgA8LD/2MS5t5DNASjbYD+gghpka6cgcvSFyYFyhcDJXH7Zi6NDP9AA=="
    }
  }
}
"#;

#[test]
fn a_tree_with_no_peers_writes_what_version_one_wrote() {
    let registry = small_tree();
    let dir = TempDir::new().unwrap();
    let graph = resolve(&registry, &roots(&[("a", "^1.0.0")]), &no_members()).unwrap();
    lockfile::save(&graph, dir.path()).unwrap();

    let expected =
        VERSION_ONE_SMALL_TREE.replace(r#""lockfileVersion": 1"#, r#""lockfileVersion": 2"#);
    assert_eq!(read(&dir), expected);
}

/// The peer keys this fixture produces, spelled out so a test asserting on one
/// reads as a claim about the format rather than as string-building.
const PLUGIN: &str = "plugin@1.0.0(react@18.2.0)";
const WRAPPER: &str = "wrapper@1.0.0(plugin@1.0.0(react@18.2.0))";

/// A resolved tree with one peer in it.
///
/// `plugin` peers `react`, which the importer supplies, and also peers a
/// `react-dom` that nothing provides — optional, so silent, and recorded all
/// the same. `wrapper` declares no peers of its own and sits above `plugin`,
/// which is what gives the file a *dependency* edge whose target carries a
/// suffix.
fn peer_graph() -> jerky::resolver::ResolvedGraph {
    let registry = FixtureRegistry::new()
        .with_tree(&[
            ("wrapper", "1.0.0", &[("plugin", "^1.0.0")]),
            ("plugin", "1.0.0", &[]),
            ("react", "18.2.0", &[]),
        ])
        .with_declared_peers(
            "plugin",
            "1.0.0",
            &[("react", ">=17", false), ("react-dom", "^18.0.0", true)],
        );

    let graph = resolve(
        &registry,
        &roots(&[("wrapper", "^1.0.0"), ("react", "18.2.0")]),
        &no_members(),
    )
    .unwrap();
    let (graph, unsatisfied) = jerky::resolver::resolve_peers(graph);
    assert!(unsatisfied.is_empty(), "{unsatisfied:?}");
    graph
}

#[test]
fn a_resolved_peer_lands_in_its_own_block() {
    // Merging peers into `dependencies` would leave the format and the linker
    // untouched, which is what makes it tempting. It would also make the
    // recorded `dependencies` stop corresponding to what the package
    // published, so anyone diffing this file against a real `package.json`
    // reads edges the package never declared.
    let dir = TempDir::new().unwrap();
    lockfile::save(&peer_graph(), dir.path()).unwrap();

    let parsed: serde_json::Value = serde_json::from_str(&read(&dir)).unwrap();
    let plugin = &parsed["packages"][PLUGIN];
    assert_eq!(plugin["peers"]["react"], "react@18.2.0");
    assert!(
        plugin["dependencies"].is_null(),
        "a resolved peer was written as a dependency the package never declared"
    );
    assert!(
        parsed["packages"]["react@18.2.0"]["peers"].is_null(),
        "a package with no peers grew an empty block"
    );
}

#[test]
fn declared_peers_record_the_range_and_the_optional_flag() {
    // The range is what makes the warning survive a cache hit: an install that
    // reuses every importer resolves nothing, so without this the second
    // install of an unsatisfied peer is silent.
    let dir = TempDir::new().unwrap();
    lockfile::save(&peer_graph(), dir.path()).unwrap();

    let parsed: serde_json::Value = serde_json::from_str(&read(&dir)).unwrap();
    let declared = &parsed["packages"][PLUGIN]["declaredPeers"];
    assert_eq!(declared["react"]["range"], ">=17");
    assert!(
        declared["react"]["optional"].is_null(),
        "`optional: false` is what saying nothing already means"
    );
    assert_eq!(declared["react-dom"]["range"], "^18.0.0");
    assert_eq!(declared["react-dom"]["optional"], true);
    // Declared and unsatisfied: what a package asked for is a fact about the
    // package, and recording it is what lets the diagnostic be recomputed.
    // What it resolved to is a different question with no answer here.
    assert!(parsed["packages"][PLUGIN]["peers"]["react-dom"].is_null());
    assert!(
        parsed["packages"]["react@18.2.0"]["declaredPeers"].is_null(),
        "a package declaring no peers grew an empty block"
    );
}

#[test]
fn peers_survive_the_round_trip_and_the_file_written_from_them_is_the_same_file() {
    let dir = TempDir::new().unwrap();
    let graph = peer_graph();
    lockfile::save(&graph, dir.path()).unwrap();
    let back = lockfile::load(dir.path()).unwrap().unwrap();

    assert_eq!(back.packages.len(), graph.packages.len());
    for (id, package) in &graph.packages {
        let other = back
            .packages
            .get(id)
            .unwrap_or_else(|| panic!("{id} came back under a different identity"));
        assert_eq!(other.peers, package.peers);
        assert_eq!(other.declared_peers, package.declared_peers);
        assert_eq!(other.dependencies, package.dependencies);
    }

    // The half a field-by-field comparison cannot cover. Identity is rebuilt
    // from the recorded fields rather than read off the key, so the rebuilt
    // identity has to render back to the key it was read from — otherwise
    // every install rewrites keys nothing asked it to change, and renames the
    // virtual store directories under them.
    let again = TempDir::new().unwrap();
    lockfile::save(&back, again.path()).unwrap();
    assert_eq!(read(&again), read(&dir));
}

#[test]
fn a_dependency_on_a_duplicated_node_keeps_its_peer_suffix() {
    // The edge has to name a node, and once `plugin` is duplicated there is no
    // `plugin@1.0.0` to name. Recording the bare version — which was enough
    // while every key was `name@version` — leaves `wrapper` pointing at a key
    // the file does not record, which `load` refuses as dangling: jerky
    // writing a lockfile jerky cannot read.
    let dir = TempDir::new().unwrap();
    lockfile::save(&peer_graph(), dir.path()).unwrap();

    let parsed: serde_json::Value = serde_json::from_str(&read(&dir)).unwrap();
    assert_eq!(
        parsed["packages"][WRAPPER]["dependencies"]["plugin"],
        "1.0.0(react@18.2.0)"
    );

    let back = lockfile::load(dir.path()).unwrap().unwrap();
    let wrapper = back
        .packages
        .values()
        .find(|package| package.id.name == "wrapper")
        .expect("wrapper was recorded");
    let plugin = &wrapper.dependencies["plugin"];
    assert_eq!(plugin.to_string(), PLUGIN);
    assert!(
        back.packages.contains_key(plugin),
        "the edge read back points at no recorded package"
    );
}

#[test]
fn one_version_under_two_peer_contexts_is_two_packages() {
    // The hazard version 2 exists to close, and the reason identity is rebuilt
    // from the recorded peers rather than parsed out of the key. Both keys
    // here split to `plugin` and `1.0.0`; only what they record tells them
    // apart, and a peer-free identity taken from the key would land them both
    // on one node — keeping one, dropping the other's subtree, saying nothing.
    let dir = TempDir::new().unwrap();
    write_raw(
        &dir,
        &format!(
            r#"{{"lockfileVersion":2,"importers":{{}},"packages":{{
                "react@17.0.2": {{"version":"17.0.2","resolved":"https://r.test/r17.tgz","integrity":"{HASH}"}},
                "react@18.2.0": {{"version":"18.2.0","resolved":"https://r.test/r18.tgz","integrity":"{HASH}"}},
                "plugin@1.0.0(react@17.0.2)": {{"version":"1.0.0","resolved":"https://r.test/p.tgz","integrity":"{HASH}",
                    "peers":{{"react":"react@17.0.2"}}}},
                "plugin@1.0.0(react@18.2.0)": {{"version":"1.0.0","resolved":"https://r.test/p.tgz","integrity":"{HASH}",
                    "peers":{{"react":"react@18.2.0"}}}}
            }}}}"#
        ),
    );

    let graph = lockfile::load(dir.path()).unwrap().unwrap();
    assert_eq!(graph.packages.len(), 4);

    let plugins: Vec<String> = graph
        .packages
        .keys()
        .filter(|id| id.name == "plugin")
        .map(|id| id.to_string())
        .collect();
    assert_eq!(
        plugins,
        ["plugin@1.0.0(react@17.0.2)", "plugin@1.0.0(react@18.2.0)"],
        "two entries, two nodes, each keyed by what it resolved against"
    );

    // And each node's peer names the react it recorded, not merely some react.
    for package in graph.packages.values().filter(|p| p.id.name == "plugin") {
        assert_eq!(
            package.peers["react"].version,
            package.id.context["react"].version
        );
    }
}

#[test]
fn a_peer_pointing_at_nothing_is_refused() {
    // A peer is linked like a dependency, so a peer naming a node the file
    // does not record is the same broken install: a `node_modules` with a link
    // into nowhere. Same check, same report.
    let dir = TempDir::new().unwrap();
    write_raw(
        &dir,
        &format!(
            r#"{{"lockfileVersion":2,"importers":{{}},"packages":{{
                "plugin@1.0.0(react@18.2.0)": {{"version":"1.0.0","resolved":"https://r.test/p.tgz","integrity":"{HASH}",
                    "peers":{{"react":"react@18.2.0"}}}}
            }}}}"#
        ),
    );

    match lockfile::load(dir.path()) {
        Err(LockfileError::DanglingEdge {
            entry, dependency, ..
        }) => {
            assert_eq!(entry, "plugin@1.0.0(react@18.2.0)");
            assert_eq!(dependency, "react@18.2.0");
        }
        other => panic!("expected a dangling edge error, got {other:?}"),
    }
}

#[test]
fn a_key_whose_suffix_swallowed_the_version_is_refused() {
    // What the suffix *says* is never read back — a collapsed one is a hash
    // and does not decode — but where it begins still has to leave a
    // `name@version` in front of it.
    let dir = TempDir::new().unwrap();
    write_raw(
        &dir,
        &format!(
            r#"{{"lockfileVersion":2,"importers":{{}},"packages":{{
                "plugin(react@18.2.0)": {{"version":"1.0.0","resolved":"https://r.test/p.tgz","integrity":"{HASH}"}}
            }}}}"#
        ),
    );

    assert!(matches!(
        lockfile::load(dir.path()),
        Err(LockfileError::BadKey { .. })
    ));
}

#[test]
fn a_key_disagreeing_with_the_peers_it_records_is_refused() {
    // A block copied and its key edited, which is what hand-editing produces.
    // The file states the peer context twice — once in the key and once in
    // `peers` — so the two can disagree, exactly as the key and the version
    // can. Refusing is also what stops the copy landing on the original: two
    // keys recording one set of peers are one node, and a file that cannot say
    // which entry an edge means is not one to install from.
    let dir = TempDir::new().unwrap();
    write_raw(
        &dir,
        &format!(
            r#"{{"lockfileVersion":2,"importers":{{}},"packages":{{
                "react@18.2.0": {{"version":"18.2.0","resolved":"https://r.test/r18.tgz","integrity":"{HASH}"}},
                "plugin@1.0.0(react@17.0.2)": {{"version":"1.0.0","resolved":"https://r.test/p.tgz","integrity":"{HASH}",
                    "peers":{{"react":"react@18.2.0"}}}},
                "plugin@1.0.0(react@18.2.0)": {{"version":"1.0.0","resolved":"https://r.test/p.tgz","integrity":"{HASH}",
                    "peers":{{"react":"react@18.2.0"}}}}
            }}}}"#
        ),
    );

    match lockfile::load(dir.path()) {
        Err(LockfileError::KeyIdentityMismatch { entry, rebuilt, .. }) => {
            assert_eq!(
                (entry.as_str(), rebuilt.as_str()),
                ("plugin@1.0.0(react@17.0.2)", "plugin@1.0.0(react@18.2.0)"),
                "the key claims a react the entry itself does not record"
            );
        }
        other => panic!("expected a key/identity mismatch, got {other:?}"),
    }
}

#[test]
fn a_version_one_lockfile_is_refused_rather_than_migrated() {
    // #104 was closed as unnecessary rather than deferred: jerky has shipped
    // no MVP, so nobody holds a version 1 file and a migration has nobody to
    // serve. The upgrade message it already falls through to is the whole
    // answer, and this test is what stops a migration being written later
    // under the impression one is missing.
    let dir = TempDir::new().unwrap();
    write_raw(
        &dir,
        r#"{"lockfileVersion":1,"importers":{},"packages":{}}"#,
    );

    assert!(matches!(
        lockfile::load(dir.path()),
        Err(LockfileError::UnsupportedVersion { found: 1, .. })
    ));
}

#[test]
fn a_cycle_through_a_peer_keyed_node_reads_back_under_the_keys_it_was_written_with() {
    // Identity is rebuilt by walking edges, and a dependency cycle is where a
    // walk has to stop. The peer pass leaves the edge that closed the loop it
    // entered out of the name; reading the file back has to leave out the same
    // edge, which means entering the loop where the pass did — from the
    // importers, not from whichever key happens to sort first. Break a
    // different edge and `b` comes back carrying `z`'s context, so it is
    // written under a key it was never read from and the whole subtree is
    // renamed by an install that changed nothing.
    let registry = FixtureRegistry::new()
        .with_tree(&[
            ("z", "1.0.0", &[("b", "^1.0.0"), ("plugin", "^1.0.0")]),
            ("b", "1.0.0", &[("z", "^1.0.0")]),
            ("plugin", "1.0.0", &[]),
            ("react", "18.2.0", &[]),
        ])
        .with_declared_peers("plugin", "1.0.0", &[("react", ">=17", false)]);

    let dir = TempDir::new().unwrap();
    let graph = resolve(
        &registry,
        &roots(&[("z", "^1.0.0"), ("react", "18.2.0")]),
        &no_members(),
    )
    .unwrap();
    let (graph, _) = jerky::resolver::resolve_peers(graph);
    lockfile::save(&graph, dir.path()).unwrap();

    let back = lockfile::load(dir.path()).unwrap().unwrap();
    let again = TempDir::new().unwrap();
    lockfile::save(&back, again.path()).unwrap();

    assert_eq!(read(&again), read(&dir));
}
