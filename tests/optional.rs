//! `optionalDependencies`, and the one thing jerky skips.
//!
//! Each test is a claim from
//! `docs/specs/2026-09-16-optional-dependencies-design.md` rather than coverage
//! for its own sake: that a declared platform mismatch is skipped, that nothing
//! else is, that the skip prunes a subtree without touching the lockfile, and
//! that it survives the install which resolves nothing at all.
//!
//! The platform is pinned without asking which machine is running the suite.
//! jerky targets WSL, Linux and macOS and has a standing invariant against ever
//! supporting Windows, so `os: ["win32"]` is skipped everywhere these tests can
//! run and `os: ["linux", "darwin"]` is kept everywhere.

use std::collections::BTreeMap;
use std::path::Path;

use jerky::commands::install::{Mode, sync};
use jerky::lockfile;
use jerky::platform::Platform;
use jerky::resolver::{Declared, ImporterPath, Kind, ResolvedGraph, resolve, resolve_peers};
use jerky::store::Store;
use jerky::testing::FixtureRegistry;
use jerky::workspace::Workspace;

use tempfile::TempDir;

/// A workspace of one, keyed `.`, which is the degenerate case of the general
/// input rather than a special one.
fn roots(list: &[(&str, &str)]) -> BTreeMap<ImporterPath, BTreeMap<String, Declared>> {
    BTreeMap::from([(
        ImporterPath::root(),
        list.iter()
            .map(|(name, specifier)| {
                (
                    name.to_string(),
                    Declared {
                        specifier: specifier.to_string(),
                        kind: Kind::Prod,
                    },
                )
            })
            .collect(),
    )])
}

fn no_members() -> BTreeMap<String, ImporterPath> {
    BTreeMap::new()
}

/// Every package name the graph holds, sorted, which is what almost every
/// claim below is about.
fn names(graph: &ResolvedGraph) -> Vec<&str> {
    graph.packages.keys().map(|id| id.name.as_str()).collect()
}

/// The graph this machine would actually materialise, and the names it left
/// out.
fn here(graph: &ResolvedGraph) -> (ResolvedGraph, Vec<String>) {
    let (installable, skipped) = graph.for_platform(&Platform::current());
    let skipped = skipped
        .into_iter()
        .map(|id| format!("{}@{}", id.name, id.version))
        .collect();
    (installable, skipped)
}

fn project(dir: &Path, manifest: &str) -> Workspace {
    std::fs::write(dir.join("package.json"), manifest).unwrap();
    Workspace::discover(dir).unwrap()
}

#[test]
fn an_optional_dependency_this_machine_supports_is_an_ordinary_dependency() {
    let registry = FixtureRegistry::new()
        .with_tree(&[("host", "1.0.0", &[]), ("binary", "1.0.0", &[])])
        .with_optional_dependencies("host", "1.0.0", &[("binary", "^1.0.0")])
        .with_platform("binary", "1.0.0", &["linux", "darwin"], &[]);

    let graph = resolve(&registry, &roots(&[("host", "^1.0.0")]), &no_members()).unwrap();

    let host = graph
        .packages
        .values()
        .find(|p| p.id.name == "host")
        .unwrap();
    assert_eq!(host.dependencies["binary"].version, "1.0.0");
    assert!(
        host.optional.contains("binary"),
        "the section it was declared in is recorded"
    );

    let (installable, skipped) = here(&graph);
    assert!(skipped.is_empty());
    assert_eq!(names(&installable), ["binary", "host"]);
}

#[test]
fn an_optional_dependency_this_machine_does_not_support_is_skipped() {
    let registry = FixtureRegistry::new()
        .with_tree(&[("host", "1.0.0", &[]), ("windows-binary", "1.0.0", &[])])
        .with_optional_dependencies("host", "1.0.0", &[("windows-binary", "^1.0.0")])
        .with_platform("windows-binary", "1.0.0", &["win32"], &[]);

    let graph = resolve(&registry, &roots(&[("host", "^1.0.0")]), &no_members()).unwrap();

    // Resolved in full. The lockfile is written from this graph, and it must
    // say the same thing on every machine.
    assert_eq!(names(&graph), ["host", "windows-binary"]);

    let (installable, skipped) = here(&graph);
    assert_eq!(skipped, ["windows-binary@1.0.0"]);
    assert_eq!(names(&installable), ["host"]);

    // And the edge goes with it, or the plan writes a link into a virtual
    // store entry nothing created.
    let host = installable
        .packages
        .values()
        .find(|p| p.id.name == "host")
        .unwrap();
    assert!(host.dependencies.is_empty(), "the dangling edge is dropped");
}

#[test]
fn the_subtree_only_a_skipped_package_reached_is_skipped_with_it() {
    let registry = FixtureRegistry::new()
        .with_tree(&[
            ("host", "1.0.0", &[]),
            ("windows-binary", "1.0.0", &[("helper", "^1.0.0")]),
            ("helper", "1.0.0", &[("deep", "^1.0.0")]),
            ("deep", "1.0.0", &[]),
        ])
        .with_optional_dependencies("host", "1.0.0", &[("windows-binary", "^1.0.0")])
        .with_platform("windows-binary", "1.0.0", &["win32"], &[]);

    let graph = resolve(&registry, &roots(&[("host", "^1.0.0")]), &no_members()).unwrap();
    assert_eq!(names(&graph), ["deep", "helper", "host", "windows-binary"]);

    let (installable, skipped) = here(&graph);
    assert_eq!(names(&installable), ["host"]);

    // `helper` and `deep` are unreachable rather than skipped, and the
    // difference is reported: only a platform mismatch is a skip, and the
    // count the install prints is about platforms.
    assert_eq!(skipped, ["windows-binary@1.0.0"]);
}

#[test]
fn a_package_the_skipped_one_merely_shared_is_kept() {
    // `shared` is reached through the skipped binary and through a required
    // edge. One reason to install it beats any amount of permission not to,
    // which is npm's "a node is optional only when every path to it is".
    let registry = FixtureRegistry::new()
        .with_tree(&[
            ("host", "1.0.0", &[("plain", "^1.0.0")]),
            ("plain", "1.0.0", &[("shared", "^1.0.0")]),
            ("windows-binary", "1.0.0", &[("shared", "^1.0.0")]),
            ("shared", "1.0.0", &[]),
        ])
        .with_optional_dependencies("host", "1.0.0", &[("windows-binary", "^1.0.0")])
        .with_platform("windows-binary", "1.0.0", &["win32"], &[]);

    let graph = resolve(&registry, &roots(&[("host", "^1.0.0")]), &no_members()).unwrap();

    let (installable, skipped) = here(&graph);
    assert_eq!(skipped, ["windows-binary@1.0.0"]);
    assert_eq!(names(&installable), ["host", "plain", "shared"]);
}

#[test]
fn a_required_edge_installs_an_unsupported_package_without_comment() {
    // The constraint is consulted only where a skip is available. Refusing a
    // tree that works is not a stricter kind of correct, and `os` is advisory
    // metadata that publishers get wrong.
    let registry = FixtureRegistry::new()
        .with_tree(&[
            ("host", "1.0.0", &[("windows-binary", "^1.0.0")]),
            ("windows-binary", "1.0.0", &[]),
        ])
        .with_platform("windows-binary", "1.0.0", &["win32"], &[]);

    let graph = resolve(&registry, &roots(&[("host", "^1.0.0")]), &no_members()).unwrap();

    let (installable, skipped) = here(&graph);
    assert!(skipped.is_empty(), "nothing is skipped and nothing is said");
    assert_eq!(names(&installable), ["host", "windows-binary"]);
}

#[test]
fn an_importers_own_dependency_is_never_skipped() {
    // An importer declares no optional section, so every edge leaving one is
    // required — including one naming a package that rules this machine out.
    let registry = FixtureRegistry::new()
        .with_tree(&[("windows-binary", "1.0.0", &[])])
        .with_platform("windows-binary", "1.0.0", &["win32"], &[]);

    let graph = resolve(
        &registry,
        &roots(&[("windows-binary", "^1.0.0")]),
        &no_members(),
    )
    .unwrap();

    let (installable, skipped) = here(&graph);
    assert!(skipped.is_empty());
    assert_eq!(names(&installable), ["windows-binary"]);
}

#[test]
fn a_name_in_both_blocks_is_optional_and_takes_the_optional_range() {
    // npm's documented rule: "entries in optionalDependencies will override
    // entries of the same name in dependencies".
    let registry = FixtureRegistry::new()
        .with_tree(&[
            ("host", "1.0.0", &[("binary", "^1.0.0")]),
            ("binary", "1.0.0", &[]),
            ("binary", "2.0.0", &[]),
        ])
        .with_optional_dependencies("host", "1.0.0", &[("binary", "^2.0.0")])
        .with_platform("binary", "2.0.0", &["win32"], &[]);

    let graph = resolve(&registry, &roots(&[("host", "^1.0.0")]), &no_members()).unwrap();

    let host = graph
        .packages
        .values()
        .find(|p| p.id.name == "host")
        .unwrap();
    assert_eq!(
        host.dependencies["binary"].version, "2.0.0",
        "the optional range won"
    );
    assert!(host.optional.contains("binary"));

    let (_, skipped) = here(&graph);
    assert_eq!(
        skipped,
        ["binary@2.0.0"],
        "and being optional is what made it skippable"
    );
}

#[test]
fn an_optional_dependency_that_does_not_resolve_is_still_fatal() {
    // The line is drawn between resolution and materialisation. A platform
    // mismatch is a recordable fact about the package; a name the registry
    // does not answer for has nothing the lockfile could record.
    let registry = FixtureRegistry::new()
        .with_tree(&[("host", "1.0.0", &[])])
        .with_optional_dependencies("host", "1.0.0", &[("nowhere", "^1.0.0")]);

    let error = resolve(&registry, &roots(&[("host", "^1.0.0")]), &no_members()).unwrap_err();

    assert!(
        error.to_string().contains("nowhere"),
        "reported as the failure it is: {error}"
    );
}

#[test]
fn a_corrupt_optional_tarball_is_still_fatal() {
    // The integrity argument in §2: `optional: true` is the publisher's
    // statement about whether their functionality is required, not the
    // consumer's waiver on whether the bytes are who they say they are.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let workspace = project(
        work.path(),
        r#"{"name":"demo","dependencies":{"host":"^1.0.0"}}"#,
    );
    let store = Store::new(home.path().join("store"));

    let registry = FixtureRegistry::new()
        .with_tree(&[("host", "1.0.0", &[])])
        .with_corrupt_package("binary", "1.0.0", b"these are not the bytes".to_vec())
        .with_optional_dependencies("host", "1.0.0", &[("binary", "^1.0.0")]);

    let error = sync(&workspace, &store, &registry, None, Mode::Develop).unwrap_err();

    assert!(
        error.to_string().contains("integrity"),
        "an optional dependency does not waive the hash: {error}"
    );
}

#[test]
fn a_skipped_package_is_recorded_but_neither_fetched_nor_linked() {
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let workspace = project(
        work.path(),
        r#"{"name":"demo","dependencies":{"host":"^1.0.0"}}"#,
    );
    let store = Store::new(home.path().join("store"));

    let registry = FixtureRegistry::new()
        .with_tree(&[("host", "1.0.0", &[]), ("windows-binary", "1.0.0", &[])])
        .with_optional_dependencies("host", "1.0.0", &[("windows-binary", "^1.0.0")])
        .with_platform("windows-binary", "1.0.0", &["win32"], &["x64"]);

    let outcome = sync(&workspace, &store, &registry, None, Mode::Develop).unwrap();

    assert_eq!(outcome.skipped.len(), 1);
    assert_eq!(outcome.skipped[0].name, "windows-binary");
    assert_eq!(
        outcome.linked.len(),
        1,
        "the count is what was linked, not what was resolved"
    );

    assert!(work.path().join("node_modules/host").is_symlink());
    assert!(
        !work
            .path()
            .join("node_modules/.jerky/windows-binary@1.0.0")
            .exists(),
        "no virtual store entry for a package this machine skipped"
    );
    assert!(
        !work
            .path()
            .join("node_modules/.jerky/host@1.0.0/node_modules/windows-binary")
            .exists(),
        "and no link into one"
    );

    // Recorded in full, so the same file plans correctly on a machine that
    // does support it.
    let raw = std::fs::read_to_string(work.path().join("jerky-lock.json")).unwrap();
    let lock: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        lock["packages"]["host@1.0.0"]["optionalDependencies"]["windows-binary"],
        "1.0.0"
    );
    assert!(
        lock["packages"]["host@1.0.0"]["dependencies"].is_null(),
        "an optional edge is not written into the required block"
    );
    assert_eq!(lock["packages"]["windows-binary@1.0.0"]["os"][0], "win32");
    assert_eq!(lock["packages"]["windows-binary@1.0.0"]["cpu"][0], "x64");
    assert!(
        lock["packages"]["windows-binary@1.0.0"]["skipped"].is_null(),
        "the entry records what the package declared, never what this machine decided"
    );
}

#[test]
fn a_package_with_no_platform_constraint_writes_the_bytes_it_always_did() {
    // The change has to be invisible where the feature is absent, or every
    // lockfile in the wild is rewritten by an install that changed nothing.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let workspace = project(
        work.path(),
        r#"{"name":"demo","dependencies":{"plain":"^1.0.0"}}"#,
    );
    let store = Store::new(home.path().join("store"));
    let registry = FixtureRegistry::new().with_tree(&[("plain", "1.0.0", &[])]);

    sync(&workspace, &store, &registry, None, Mode::Develop).unwrap();

    let raw = std::fs::read_to_string(work.path().join("jerky-lock.json")).unwrap();
    for absent in ["optionalDependencies", "\"os\"", "\"cpu\""] {
        assert!(
            !raw.contains(absent),
            "{absent} should be skipped when empty:\n{raw}"
        );
    }
}

#[test]
fn a_reused_lockfile_still_skips() {
    // The case that fails if optionality is not recorded. The second install
    // matches every importer, so it resolves nothing at all and builds its
    // graph out of the lockfile — which is the only thing left that can say
    // `windows-binary` was skippable.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let workspace = project(
        work.path(),
        r#"{"name":"demo","dependencies":{"host":"^1.0.0"}}"#,
    );
    let store = Store::new(home.path().join("store"));

    let registry = FixtureRegistry::new()
        .with_tree(&[("host", "1.0.0", &[]), ("windows-binary", "1.0.0", &[])])
        .with_optional_dependencies("host", "1.0.0", &[("windows-binary", "^1.0.0")])
        .with_platform("windows-binary", "1.0.0", &["win32"], &[]);

    sync(&workspace, &store, &registry, None, Mode::Develop).unwrap();
    let before = std::fs::read_to_string(work.path().join("jerky-lock.json")).unwrap();

    let again = sync(&workspace, &store, &registry, None, Mode::Develop).unwrap();

    assert_eq!(again.skipped.len(), 1, "the skip survived the cache hit");
    assert_eq!(again.linked.len(), 1);
    assert!(
        !work
            .path()
            .join("node_modules/.jerky/windows-binary@1.0.0")
            .exists()
    );

    let after = std::fs::read_to_string(work.path().join("jerky-lock.json")).unwrap();
    assert_eq!(before, after, "and the file it wrote back is the same file");
}

#[test]
fn a_production_install_skips_from_the_lockfile_alone() {
    // `--production` resolves nothing by construction, so this is the reuse
    // case with the registry taken away entirely.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let workspace = project(
        work.path(),
        r#"{"name":"demo","dependencies":{"host":"^1.0.0"}}"#,
    );
    let store = Store::new(home.path().join("store"));

    let registry = FixtureRegistry::new()
        .with_tree(&[("host", "1.0.0", &[]), ("windows-binary", "1.0.0", &[])])
        .with_optional_dependencies("host", "1.0.0", &[("windows-binary", "^1.0.0")])
        .with_platform("windows-binary", "1.0.0", &["win32"], &[]);

    sync(&workspace, &store, &registry, None, Mode::Develop).unwrap();
    std::fs::remove_dir_all(work.path().join("node_modules")).unwrap();

    let outcome = sync(&workspace, &store, &registry, None, Mode::Production).unwrap();

    assert_eq!(outcome.skipped.len(), 1);
    assert!(work.path().join("node_modules/host").is_symlink());
    assert!(
        !work
            .path()
            .join("node_modules/.jerky/windows-binary@1.0.0")
            .exists()
    );
}

#[test]
fn an_optional_edge_counts_towards_its_dependents_identity() {
    // The hazard behind reading both blocks back into one map. `plugin` is
    // duplicated by its peers, so `wrapper`'s own identity depends on which
    // copy it points at — and that is true whether it declared the edge as
    // required or as optional. A rebuild that followed only the required block
    // would name `wrapper` without the context it was written with, and write
    // it back under a key it was not read from: a lockfile whose keys move on
    // an install that changed nothing, and virtual store directories renamed
    // along with them.
    let dir = TempDir::new().unwrap();
    let registry = FixtureRegistry::new()
        .with_tree(&[
            ("wrapper", "1.0.0", &[]),
            ("plugin", "1.0.0", &[]),
            ("react", "18.2.0", &[]),
        ])
        .with_optional_dependencies("wrapper", "1.0.0", &[("plugin", "^1.0.0")])
        .with_declared_peers("plugin", "1.0.0", &[("react", ">=17", false)]);

    let graph = resolve(
        &registry,
        &roots(&[("wrapper", "^1.0.0"), ("react", "18.2.0")]),
        &no_members(),
    )
    .unwrap();
    let (graph, unsatisfied) = resolve_peers(graph);
    assert!(unsatisfied.is_empty(), "{unsatisfied:?}");

    lockfile::save(&graph, dir.path()).unwrap();
    let raw = std::fs::read_to_string(dir.path().join("jerky-lock.json")).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let wrapper = "wrapper@1.0.0(plugin@1.0.0(react@18.2.0))";
    assert_eq!(
        parsed["packages"][wrapper]["optionalDependencies"]["plugin"], "1.0.0(react@18.2.0)",
        "the optional edge keeps the suffix that names a node the file records"
    );

    let back = lockfile::load(dir.path()).unwrap().unwrap();
    assert_eq!(
        back.packages
            .keys()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        graph
            .packages
            .keys()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        "every node came back under the identity it was written with"
    );

    let again = TempDir::new().unwrap();
    lockfile::save(&back, again.path()).unwrap();
    assert_eq!(
        std::fs::read_to_string(again.path().join("jerky-lock.json")).unwrap(),
        raw,
        "and the file written back from what was read is the same file"
    );
}
