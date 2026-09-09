use std::path::Path;

use jerky::cli::{PackageSpec, VersionSpec};
use jerky::commands::install::{InstallError, install};
use jerky::store::Store;
use jerky::testing::{FixtureRegistry, TarEntry, build_tarball};

use std::os::unix::fs::MetadataExt as _;
use tempfile::TempDir;

fn lodash_tarball() -> Vec<u8> {
    build_tarball(&[
        TarEntry::file(
            "package/package.json",
            r#"{"name":"lodash","version":"4.17.21"}"#,
        ),
        TarEntry::file("package/lodash.js", "module.exports = {};"),
    ])
}

fn project(dir: &Path) -> &Path {
    std::fs::write(dir.join("package.json"), "{\n  \"name\": \"demo\"\n}\n").unwrap();
    dir
}

fn spec(name: &str, version: VersionSpec) -> PackageSpec {
    PackageSpec {
        name: name.to_string(),
        version,
    }
}

#[test]
fn installs_a_package_end_to_end() {
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let project_dir = project(work.path());
    let store = Store::new(home.path().join("store"));
    let registry = FixtureRegistry::new().with_package("lodash", "4.17.21", lodash_tarball());

    let installed = install(
        project_dir,
        &store,
        &registry,
        &spec("lodash", VersionSpec::Latest),
    )
    .unwrap();

    assert_eq!(installed.version, "4.17.21");

    // The symlink resolves through the virtual store to real contents.
    let link = project_dir.join("node_modules/lodash");
    assert!(link.join("package.json").is_file());
    assert_eq!(
        std::fs::read_to_string(link.join("lodash.js")).unwrap(),
        "module.exports = {};"
    );

    // The manifest records the concrete version, with no caret.
    let raw = std::fs::read_to_string(project_dir.join("package.json")).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(parsed["dependencies"]["lodash"], "4.17.21");
}

#[test]
fn the_recorded_version_comes_from_the_registry_not_the_request() {
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let project_dir = project(work.path());
    let store = Store::new(home.path().join("store"));
    let registry = FixtureRegistry::new().with_package("react", "18.2.0", lodash_tarball());

    // Requesting a dist-tag must pin whatever concrete version came back.
    install(
        project_dir,
        &store,
        &registry,
        &spec("react", VersionSpec::Exact("latest".into())),
    )
    .unwrap();

    let raw = std::fs::read_to_string(project_dir.join("package.json")).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(parsed["dependencies"]["react"], "18.2.0");
}

#[test]
fn files_are_hard_linked_from_the_store_not_copied() {
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let project_dir = project(work.path());
    // Store and project share one temp root's filesystem, so the legitimate
    // EXDEV copy fallback cannot fire and make these inodes differ correctly.
    let store = Store::new(home.path().join("store"));
    let registry = FixtureRegistry::new().with_package("lodash", "4.17.21", lodash_tarball());

    install(
        project_dir,
        &store,
        &registry,
        &spec("lodash", VersionSpec::Latest),
    )
    .unwrap();

    let linked =
        project_dir.join("node_modules/.jerky/lodash@4.17.21/node_modules/lodash/lodash.js");
    let stored = std::fs::read_dir(store.entry_path_root())
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path().join("lodash.js"))
        .find(|p| p.is_file())
        .expect("a store entry exists");

    let a = std::fs::metadata(&linked).unwrap();
    let b = std::fs::metadata(&stored).unwrap();
    assert_eq!(
        (a.dev(), a.ino()),
        (b.dev(), b.ino()),
        "package was copied, not hard-linked — the store's whole purpose is lost"
    );
}

#[test]
fn a_second_install_reuses_the_store_without_downloading() {
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let project_dir = project(work.path());
    let store = Store::new(home.path().join("store"));
    let registry = FixtureRegistry::new().with_package("lodash", "4.17.21", lodash_tarball());

    install(
        project_dir,
        &store,
        &registry,
        &spec("lodash", VersionSpec::Latest),
    )
    .unwrap();
    assert_eq!(registry.tarball_calls(), 1);

    install(
        project_dir,
        &store,
        &registry,
        &spec("lodash", VersionSpec::Latest),
    )
    .unwrap();

    // Idempotence has to be *counted*, not just observed: identical output
    // would also result from downloading twice.
    assert_eq!(registry.tarball_calls(), 1, "the store hit was skipped");
    assert!(
        project_dir
            .join("node_modules/lodash/package.json")
            .is_file()
    );
}

#[test]
fn an_integrity_mismatch_leaves_the_store_empty() {
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let project_dir = project(work.path());
    let store = Store::new(home.path().join("store"));
    let registry = FixtureRegistry::new().with_corrupt_package("evil", "1.0.0", lodash_tarball());

    let result = install(
        project_dir,
        &store,
        &registry,
        &spec("evil", VersionSpec::Latest),
    );

    assert!(matches!(result, Err(InstallError::Integrity { .. })));

    // Not merely "an error was returned": nothing may reach the store, and
    // the manifest must not claim a dependency that is not on disk.
    let entries = std::fs::read_dir(store.entry_path_root())
        .map(|d| d.filter_map(Result::ok).count())
        .unwrap_or(0);
    assert_eq!(entries, 0, "unverified bytes reached the store");
    assert!(!project_dir.join("node_modules/evil").exists());

    let raw = std::fs::read_to_string(project_dir.join("package.json")).unwrap();
    assert!(!raw.contains("evil"));
}

#[test]
fn reports_an_unknown_package() {
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let project_dir = project(work.path());
    let store = Store::new(home.path().join("store"));
    let registry = FixtureRegistry::new();

    assert!(matches!(
        install(
            project_dir,
            &store,
            &registry,
            &spec("nope", VersionSpec::Latest)
        ),
        Err(InstallError::Registry(_))
    ));
}

#[test]
fn requires_a_manifest() {
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let store = Store::new(home.path().join("store"));
    let registry = FixtureRegistry::new().with_package("lodash", "4.17.21", lodash_tarball());

    // No package.json was written.
    assert!(matches!(
        install(
            work.path(),
            &store,
            &registry,
            &spec("lodash", VersionSpec::Latest)
        ),
        Err(InstallError::Manifest(_))
    ));
}
