use std::path::Path;

use jerky::cli::{PackageSpec, VersionSpec};
use jerky::commands::install::{InstallError, install};
use jerky::resolver::{ImporterPath, ResolveError};
use jerky::store::Store;
use jerky::testing::{FixtureRegistry, TarEntry, build_tarball};
use jerky::workspace::{Workspace, WorkspaceError};

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

/// A workspace of one — what every single-project test is, and the same code
/// path a workspace of many takes.
fn solo(dir: &Path) -> Workspace {
    Workspace::discover(dir).unwrap()
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
        &solo(project_dir),
        &ImporterPath::root(),
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
        &solo(project_dir),
        &ImporterPath::root(),
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
        &solo(project_dir),
        &ImporterPath::root(),
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
        &solo(project_dir),
        &ImporterPath::root(),
        &store,
        &registry,
        &spec("lodash", VersionSpec::Latest),
    )
    .unwrap();
    assert_eq!(registry.tarball_calls(), 1);

    install(
        &solo(project_dir),
        &ImporterPath::root(),
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
        &solo(project_dir),
        &ImporterPath::root(),
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
            &solo(project_dir),
            &ImporterPath::root(),
            &store,
            &registry,
            &spec("nope", VersionSpec::Latest)
        ),
        // Reaches the caller through the resolver now: version selection is
        // what asks the registry, so that is where a missing package is found.
        Err(InstallError::Resolve(ResolveError::Registry(_)))
    ));
}

#[test]
fn requires_a_manifest() {
    let work = TempDir::new().unwrap();

    // No package.json was written. The requirement now sits in discovery
    // rather than in install: there is no workspace to install into, which is
    // the same refusal one layer earlier.
    assert!(matches!(
        Workspace::discover(work.path()),
        Err(WorkspaceError::Manifest(_))
    ));
}

fn write_manifest(dir: &Path, json: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("package.json"), json).unwrap();
}

/// The version recorded in the `package.json` a link resolves to. Reading it
/// through the link is the point: it proves the link resolves *and* lands on
/// the version the importer asked for.
fn linked_version(importer_dir: &Path, pkg: &str) -> String {
    let raw = std::fs::read_to_string(
        importer_dir
            .join("node_modules")
            .join(pkg)
            .join("package.json"),
    )
    .unwrap_or_else(|err| panic!("{pkg} is not linked into {}: {err}", importer_dir.display()));
    let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    parsed["version"].as_str().unwrap().to_string()
}

fn two_versions_of_lodash() -> FixtureRegistry {
    FixtureRegistry::new().with_packument("lodash", &[("4.17.21", &[]), ("3.10.1", &[])])
}

#[test]
fn installing_from_one_importer_leaves_the_others_linked() {
    // Two importers wanting different versions. Both end up correct, and
    // installing into one does not disturb the other's node_modules.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    write_manifest(
        root,
        r#"{"name":"ws","workspaces":["packages/*","apps/*"]}"#,
    );
    write_manifest(
        &root.join("packages/ui"),
        r#"{"name":"ui","dependencies":{"lodash":"3.10.1"}}"#,
    );
    write_manifest(&root.join("apps/web"), r#"{"name":"web"}"#);

    let store = Store::new(home.path().join("store"));
    let workspace = Workspace::discover(root).unwrap();

    install(
        &workspace,
        &ImporterPath::new("apps/web").unwrap(),
        &store,
        &two_versions_of_lodash(),
        &spec("lodash", VersionSpec::Exact("4.17.21".into())),
    )
    .unwrap();

    assert_eq!(linked_version(&root.join("apps/web"), "lodash"), "4.17.21");
    assert_eq!(
        linked_version(&root.join("packages/ui"), "lodash"),
        "3.10.1",
        "installing into apps/web disturbed packages/ui"
    );
}

#[test]
fn two_importers_on_the_same_version_share_one_store_entry() {
    // The reason the store lives at the workspace root. Asserted on inode,
    // like spec 1's hard-link test — a copy passes every other assertion.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    write_manifest(
        root,
        r#"{"name":"ws","workspaces":["packages/*","apps/*"]}"#,
    );
    write_manifest(
        &root.join("packages/ui"),
        r#"{"name":"ui","dependencies":{"lodash":"4.17.21"}}"#,
    );
    write_manifest(&root.join("apps/web"), r#"{"name":"web"}"#);

    let store = Store::new(home.path().join("store"));
    let workspace = Workspace::discover(root).unwrap();

    install(
        &workspace,
        &ImporterPath::new("apps/web").unwrap(),
        &store,
        &two_versions_of_lodash(),
        &spec("lodash", VersionSpec::Exact("4.17.21".into())),
    )
    .unwrap();

    let virtual_store = root.join("node_modules/.jerky");
    let entries: Vec<_> = std::fs::read_dir(&virtual_store)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("lodash@"))
        .collect();
    assert_eq!(entries, ["lodash@4.17.21"], "the version was stored twice");

    let a = std::fs::metadata(root.join("apps/web/node_modules/lodash/package.json")).unwrap();
    let b = std::fs::metadata(root.join("packages/ui/node_modules/lodash/package.json")).unwrap();
    assert_eq!(
        (a.dev(), a.ino()),
        (b.dev(), b.ino()),
        "each importer got its own copy of the bytes"
    );
}

#[test]
fn a_local_dependency_is_linked_not_fetched() {
    // Counted: the registry must see zero requests for a workspace member.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    write_manifest(
        root,
        r#"{"name":"ws","workspaces":["packages/*","apps/*"]}"#,
    );
    write_manifest(
        &root.join("packages/ui"),
        r#"{"name":"ui","version":"1.0.0"}"#,
    );
    write_manifest(
        &root.join("apps/web"),
        r#"{"name":"web","dependencies":{"ui":"workspace:*"}}"#,
    );

    let store = Store::new(home.path().join("store"));
    let workspace = Workspace::discover(root).unwrap();
    let registry = two_versions_of_lodash();

    install(
        &workspace,
        &ImporterPath::new("apps/web").unwrap(),
        &store,
        &registry,
        &spec("lodash", VersionSpec::Exact("4.17.21".into())),
    )
    .unwrap();

    assert_eq!(linked_version(&root.join("apps/web"), "ui"), "1.0.0");
    assert_eq!(
        registry.packument_calls_for("ui"),
        0,
        "a workspace member was fetched from the registry"
    );
    // And it points at the member itself, not into the virtual store.
    let target = std::fs::read_link(root.join("apps/web/node_modules/ui")).unwrap();
    assert_eq!(target, Path::new("../../../packages/ui"));
}

#[test]
fn a_workspace_specifier_naming_no_member_is_an_error() {
    // `workspace:*` for a package that is not in the repo is a typo, not a
    // fallback to the registry.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    write_manifest(root, r#"{"name":"ws","workspaces":["apps/*"]}"#);
    write_manifest(
        &root.join("apps/web"),
        r#"{"name":"web","dependencies":{"ghost":"workspace:*"}}"#,
    );

    let store = Store::new(home.path().join("store"));
    let workspace = Workspace::discover(root).unwrap();

    let err = install(
        &workspace,
        &ImporterPath::new("apps/web").unwrap(),
        &store,
        &two_versions_of_lodash(),
        &spec("lodash", VersionSpec::Exact("4.17.21".into())),
    )
    .unwrap_err();

    assert!(
        matches!(err, InstallError::Resolve(_)),
        "expected a resolution failure, got {err:?}"
    );
}

#[test]
fn a_single_importer_workspace_installs_exactly_as_before() {
    // Spec 1 and 2's existing behaviour, now through the general path.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = project(work.path());

    let store = Store::new(home.path().join("store"));
    let workspace = Workspace::discover(root).unwrap();

    let installed = install(
        &workspace,
        &ImporterPath::root(),
        &store,
        &FixtureRegistry::new().with_package("lodash", "4.17.21", lodash_tarball()),
        &spec("lodash", VersionSpec::Latest),
    )
    .unwrap();

    assert_eq!(installed.version, "4.17.21");
    assert_eq!(
        std::fs::read_link(root.join("node_modules/lodash")).unwrap(),
        Path::new(".jerky/lodash@4.17.21/node_modules/lodash"),
        "the root importer got a target shaped for a nested one"
    );
    assert_eq!(linked_version(root, "lodash"), "4.17.21");
}

/// A two-importer workspace with one local dependency, which is the shape
/// most of these assertions need.
fn linked_workspace(root: &Path) -> Workspace {
    write_manifest(
        root,
        r#"{"name":"ws","workspaces":["packages/*","apps/*"]}"#,
    );
    write_manifest(
        &root.join("packages/ui"),
        r#"{"name":"ui","version":"1.0.0"}"#,
    );
    write_manifest(
        &root.join("apps/web"),
        r#"{"name":"web","dependencies":{"ui":"workspace:*"}}"#,
    );
    Workspace::discover(root).unwrap()
}

fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn installing_a_member_by_the_workspace_protocol_links_it() {
    // `jerky install ui@workspace:*` names a member on purpose. It must link
    // rather than fetch, and record the protocol rather than a version: the
    // member's version is whatever the repo says today, so pinning it would
    // go stale on the next commit to that member.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let workspace = linked_workspace(root);
    let store = Store::new(home.path().join("store"));
    let registry = two_versions_of_lodash();

    let installed = install(
        &workspace,
        &ImporterPath::new("apps/web").unwrap(),
        &store,
        &registry,
        &spec("ui", VersionSpec::Exact("workspace:*".into())),
    )
    .unwrap();

    assert_eq!(installed.version, "1.0.0", "reported the member's version");
    assert_eq!(linked_version(&root.join("apps/web"), "ui"), "1.0.0");
    assert_eq!(
        read_json(&root.join("apps/web/package.json"))["dependencies"]["ui"],
        "workspace:*",
        "a member was pinned to a version instead of the protocol"
    );
    assert_eq!(registry.packument_calls_for("ui"), 0);
}

#[test]
fn install_writes_one_lockfile_at_the_workspace_root() {
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let workspace = linked_workspace(root);
    let store = Store::new(home.path().join("store"));

    install(
        &workspace,
        &ImporterPath::new("apps/web").unwrap(),
        &store,
        &two_versions_of_lodash(),
        &spec("lodash", VersionSpec::Exact("4.17.21".into())),
    )
    .unwrap();

    let lock = read_json(&root.join("jerky-lock.json"));
    assert_eq!(
        lock["importers"]["apps/web"]["dependencies"]["lodash"]["version"],
        "4.17.21"
    );
    // Every importer appears, including the ones that declare nothing: each
    // owns a node_modules, and a missing key would read as unresolved.
    assert!(lock["importers"]["packages/ui"].is_object());
    assert!(lock["importers"]["."].is_object());
    assert!(
        !root.join("apps/web/jerky-lock.json").exists(),
        "an importer got its own lockfile; there is one per workspace"
    );
}

#[test]
fn a_lockfile_target_is_one_climb_short_of_the_symlink() {
    // Two independent calculations produce these: `resolver::local_path` for
    // the lockfile, relative to the importer, and `linker::relative_path` for
    // the symlink, relative to the importer's node_modules. The link sits one
    // level deeper, so it must climb exactly once more. Nothing in the types
    // keeps the two in agreement, so it is pinned here.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let workspace = linked_workspace(root);
    let store = Store::new(home.path().join("store"));

    install(
        &workspace,
        &ImporterPath::new("apps/web").unwrap(),
        &store,
        &two_versions_of_lodash(),
        &spec("lodash", VersionSpec::Exact("4.17.21".into())),
    )
    .unwrap();

    let lock = read_json(&root.join("jerky-lock.json"));
    let recorded = lock["importers"]["apps/web"]["dependencies"]["ui"]["version"]
        .as_str()
        .unwrap()
        .strip_prefix("link:")
        .expect("a local dependency is recorded with the link: protocol")
        .to_string();
    let linked = std::fs::read_link(root.join("apps/web/node_modules/ui")).unwrap();

    assert_eq!(recorded, "../../packages/ui");
    assert_eq!(linked, Path::new("..").join(&recorded));
}

#[test]
fn the_manifest_records_an_exact_pin() {
    // Even now that ranges resolve, `jerky install <pkg>` pins. What gets
    // recorded is the version the registry chose, not the request that found
    // it, so a dist-tag lands in the manifest as the version it meant today
    // rather than as a range that will mean something else tomorrow.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let project_dir = project(work.path());
    let store = Store::new(home.path().join("store"));

    install(
        &solo(project_dir),
        &ImporterPath::root(),
        &store,
        &FixtureRegistry::new().with_package("lodash", "4.17.21", lodash_tarball()),
        &spec("lodash", VersionSpec::Latest),
    )
    .unwrap();

    assert_eq!(
        read_json(&project_dir.join("package.json"))["dependencies"]["lodash"],
        "4.17.21"
    );
}

#[test]
fn an_exact_request_still_installs_that_exact_version() {
    // A request for one version must not be widened on the way through the
    // resolver: `^4.17.21` would select 4.18.0 where one exists, which is not
    // what `lodash@4.17.21` asked for — in the manifest or in node_modules.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let workspace = linked_workspace(root);
    let store = Store::new(home.path().join("store"));
    let registry =
        FixtureRegistry::new().with_packument("lodash", &[("4.17.21", &[]), ("4.18.0", &[])]);

    let installed = install(
        &workspace,
        &ImporterPath::new("apps/web").unwrap(),
        &store,
        &registry,
        &spec("lodash", VersionSpec::Exact("4.17.21".into())),
    )
    .unwrap();

    assert_eq!(installed.version, "4.17.21");
    assert_eq!(linked_version(&root.join("apps/web"), "lodash"), "4.17.21");
    assert_eq!(
        read_json(&root.join("apps/web/package.json"))["dependencies"]["lodash"],
        "4.17.21"
    );
}

#[test]
fn a_range_the_user_typed_is_recorded_as_they_typed_it() {
    // The pin is a default, not an override. Someone who writes a range has
    // stated a preference, and flattening it to the version it happens to
    // select today would be the tool overruling an instruction rather than
    // supplying a missing one. 5.0.0 exists and must not be selected, which is
    // what makes this a range rather than a synonym for `latest`.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));
    let registry = FixtureRegistry::new().with_packument(
        "lodash",
        &[("4.17.21", &[]), ("4.18.0", &[]), ("5.0.0", &[])],
    );
    write_manifest(root, r#"{"name":"demo"}"#);

    let installed = install(
        &solo(root),
        &ImporterPath::root(),
        &store,
        &registry,
        &spec("lodash", VersionSpec::Exact("^4.0.0".into())),
    )
    .unwrap();

    assert_eq!(installed.version, "4.18.0");
    assert_eq!(
        read_json(&root.join("package.json"))["dependencies"]["lodash"],
        "^4.0.0"
    );

    // The lockfile carries the same specifier the manifest declares, which is
    // what lets the next install recognise it as answered.
    let recorded =
        &read_json(&root.join("jerky-lock.json"))["importers"]["."]["dependencies"]["lodash"];
    assert_eq!(recorded["specifier"], "^4.0.0");
    assert_eq!(recorded["version"], "4.18.0");

    let after_first = registry.packument_calls();
    install(
        &solo(root),
        &ImporterPath::root(),
        &store,
        &registry,
        &spec("lodash", VersionSpec::Exact("^4.0.0".into())),
    )
    .unwrap();
    assert_eq!(
        registry.packument_calls(),
        after_first,
        "a repeated range request re-resolved what the lockfile already answered"
    );
}

#[test]
fn a_dist_tag_is_pinned_rather_than_recorded_as_a_tag() {
    // `latest` parses as no range at all, and recording it verbatim would put
    // a moving pointer in the manifest — worse than the caret this default
    // exists to avoid, because it names whatever the registry decides later
    // rather than a constraint on it.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));
    let registry =
        FixtureRegistry::new().with_packument("lodash", &[("4.17.21", &[]), ("4.18.0", &[])]);
    write_manifest(root, r#"{"name":"demo"}"#);

    install(
        &solo(root),
        &ImporterPath::root(),
        &store,
        &registry,
        &spec("lodash", VersionSpec::Exact("latest".into())),
    )
    .unwrap();

    assert_eq!(
        read_json(&root.join("package.json"))["dependencies"]["lodash"],
        "4.18.0"
    );
}

#[test]
fn a_pinned_dependency_does_not_drift_when_its_importer_is_re_resolved() {
    // The reason the pin is the default. Installing something *else* makes
    // the importer stale, so lodash is resolved a second time — and a caret
    // in the manifest is exactly the permission the resolver needs to move it
    // to 4.18.0 at that point, without the user asking for anything. An exact
    // specifier resolves to itself however often it is asked.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));
    let registry = FixtureRegistry::new()
        .with_packument("lodash", &[("4.17.21", &[]), ("4.18.0", &[])])
        .with_tree(&[("alpha", "1.0.0", &[])]);
    write_manifest(root, r#"{"name":"demo"}"#);

    install(
        &solo(root),
        &ImporterPath::root(),
        &store,
        &registry,
        &spec("lodash", VersionSpec::Exact("4.17.21".into())),
    )
    .unwrap();

    install(
        &solo(root),
        &ImporterPath::root(),
        &store,
        &registry,
        &spec("alpha", VersionSpec::Exact("1.0.0".into())),
    )
    .unwrap();

    assert_eq!(linked_version(root, "lodash"), "4.17.21");
    assert_eq!(
        read_json(&root.join("package.json"))["dependencies"]["lodash"],
        "4.17.21"
    );
    assert_eq!(
        read_json(&root.join("jerky-lock.json"))["importers"]["."]["dependencies"]["lodash"]["version"],
        "4.17.21"
    );
}

#[test]
fn an_unchanged_workspace_does_not_re_resolve() {
    // Counted, because the resolved graph is identical either way — only the
    // requests distinguish a reuse from a re-resolution.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let workspace = linked_workspace(root);
    let store = Store::new(home.path().join("store"));
    let registry = two_versions_of_lodash();
    let request = spec("lodash", VersionSpec::Exact("4.17.21".into()));
    let web = ImporterPath::new("apps/web").unwrap();

    install(&workspace, &web, &store, &registry, &request).unwrap();
    let after_first = registry.packument_calls();
    assert!(after_first > 0, "the first install resolved nothing");

    // Same workspace, same request, and now a lockfile recording both.
    let workspace = Workspace::discover(root).unwrap();
    install(&workspace, &web, &store, &registry, &request).unwrap();

    assert_eq!(
        registry.packument_calls(),
        after_first,
        "the second install re-resolved an unchanged workspace"
    );
    assert_eq!(registry.tarball_calls(), 1, "the tarball was fetched twice");
    assert_eq!(linked_version(&root.join("apps/web"), "lodash"), "4.17.21");
}

#[test]
fn editing_one_importer_re_resolves_only_what_it_must() {
    // Staleness is per-importer: a changed apps/web must not invalidate
    // everything the root already resolved. This is what the importers map
    // buys over a single `root` block.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    write_manifest(
        root,
        r#"{"name":"ws","workspaces":["packages/*","apps/*"]}"#,
    );
    write_manifest(
        &root.join("packages/ui"),
        r#"{"name":"ui","dependencies":{"lodash":"^4.0.0"}}"#,
    );
    write_manifest(&root.join("apps/web"), r#"{"name":"web"}"#);

    let store = Store::new(home.path().join("store"));
    let registry = FixtureRegistry::new().with_tree(&[
        ("lodash", "4.17.21", &[]),
        ("alpha", "1.0.0", &[]),
        ("beta", "1.0.0", &[]),
    ]);
    let web = ImporterPath::new("apps/web").unwrap();

    install(
        &Workspace::discover(root).unwrap(),
        &web,
        &store,
        &registry,
        &spec("alpha", VersionSpec::Exact("1.0.0".into())),
    )
    .unwrap();
    assert_eq!(registry.packument_calls_for("lodash"), 1);

    // Only apps/web changes.
    write_manifest(
        &root.join("apps/web"),
        r#"{"name":"web","dependencies":{"alpha":"^1.0.0","beta":"^1.0.0"}}"#,
    );

    install(
        &Workspace::discover(root).unwrap(),
        &web,
        &store,
        &registry,
        &spec("beta", VersionSpec::Exact("1.0.0".into())),
    )
    .unwrap();

    assert_eq!(
        registry.packument_calls_for("lodash"),
        1,
        "packages/ui was re-resolved even though its manifest never changed"
    );
    assert_eq!(registry.packument_calls_for("beta"), 1);

    // And the untouched importer is still installed and still recorded.
    assert_eq!(
        linked_version(&root.join("packages/ui"), "lodash"),
        "4.17.21"
    );
    let lock = read_json(&root.join("jerky-lock.json"));
    assert_eq!(
        lock["importers"]["packages/ui"]["dependencies"]["lodash"]["version"], "4.17.21",
        "a reused importer was dropped from the lockfile"
    );
    assert!(
        lock["packages"]["lodash@4.17.21"].is_object(),
        "the reused importer's package was pruned out from under it"
    );
}

#[test]
fn a_lockfile_integrity_mismatch_stops_the_install() {
    // The trust-on-first-use anchor spec 1 explicitly went without. The
    // lockfile's hash is authoritative: if the registry later reports a
    // different one for the same version, the install stops.
    let work = TempDir::new().unwrap();
    let root = work.path();
    write_manifest(root, r#"{"name":"ws","workspaces":["packages/*"]}"#);
    write_manifest(
        &root.join("packages/ui"),
        r#"{"name":"ui","dependencies":{"lodash":"^4.0.0"}}"#,
    );

    let registry = FixtureRegistry::new().with_tree(&[("lodash", "4.17.21", &[])]);
    let first_home = TempDir::new().unwrap();
    install(
        &Workspace::discover(root).unwrap(),
        &ImporterPath::root(),
        &Store::new(first_home.path().join("store")),
        &registry,
        &spec("lodash", VersionSpec::Exact("4.17.21".into())),
    )
    .unwrap();

    // Someone edits the committed lockfile, or the registry republishes the
    // version under a different tarball. Either way the recorded hash and the
    // reported one disagree.
    let lock_path = root.join("jerky-lock.json");
    let mut lock = read_json(&lock_path);
    // Well-formed and the right length — sha512 of different bytes — so the
    // refusal comes from the comparison rather than from a parse failure.
    lock["packages"]["lodash@4.17.21"]["integrity"] = serde_json::Value::String(
        "sha512-IsrOi3z1jfn6siSktZ62e0Sw0+tMBkqVpd1c21rXb70/VQu/Lp0xHuqJsgBtmbh5rCrKy4NYndU2byuYRUPePw=="
            .into(),
    );
    std::fs::write(&lock_path, serde_json::to_string_pretty(&lock).unwrap()).unwrap();

    // Force a re-resolution, so the registry's hash is fetched and compared.
    write_manifest(
        &root.join("packages/ui"),
        r#"{"name":"ui","dependencies":{"lodash":"^4.17.0"}}"#,
    );

    // A fresh store, which is the realistic case: another machine cloning the
    // repo with the lockfile committed. A store hit could not be wrong — the
    // store key is the hash — so this is the only path where it can bite.
    let second_home = TempDir::new().unwrap();
    let err = install(
        &Workspace::discover(root).unwrap(),
        &ImporterPath::root(),
        &Store::new(second_home.path().join("store")),
        &registry,
        &spec("lodash", VersionSpec::Exact("4.17.21".into())),
    )
    .unwrap_err();

    match err {
        InstallError::LockedIntegrityMismatch { name, version, .. } => {
            assert_eq!(name, "lodash");
            assert_eq!(version, "4.17.21");
        }
        other => panic!("expected a locked-integrity refusal, got {other:?}"),
    }
}

#[test]
fn a_warm_store_does_not_excuse_a_lockfile_mismatch() {
    // The bytes being present proves only that they hash to their own key. It
    // says nothing about whether that hash is the one the lockfile pinned, and
    // a republished tarball is exactly the case where the two differ while the
    // store is warm — the store is machine-global, so another project can have
    // put the new bytes there already.
    let work = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let root = work.path();
    write_manifest(root, r#"{"name":"ws","workspaces":["packages/*"]}"#);
    write_manifest(
        &root.join("packages/ui"),
        r#"{"name":"ui","dependencies":{"lodash":"^4.0.0"}}"#,
    );

    let registry = FixtureRegistry::new().with_tree(&[("lodash", "4.17.21", &[])]);
    let store = Store::new(home.path().join("store"));
    install(
        &Workspace::discover(root).unwrap(),
        &ImporterPath::root(),
        &store,
        &registry,
        &spec("lodash", VersionSpec::Exact("4.17.21".into())),
    )
    .unwrap();

    let lock_path = root.join("jerky-lock.json");
    let mut lock = read_json(&lock_path);
    lock["packages"]["lodash@4.17.21"]["integrity"] = serde_json::Value::String(
        "sha512-IsrOi3z1jfn6siSktZ62e0Sw0+tMBkqVpd1c21rXb70/VQu/Lp0xHuqJsgBtmbh5rCrKy4NYndU2byuYRUPePw=="
            .into(),
    );
    std::fs::write(&lock_path, serde_json::to_string_pretty(&lock).unwrap()).unwrap();
    write_manifest(
        &root.join("packages/ui"),
        r#"{"name":"ui","dependencies":{"lodash":"^4.17.0"}}"#,
    );

    // Same store as the first install, so the bytes are already there.
    let err = install(
        &Workspace::discover(root).unwrap(),
        &ImporterPath::root(),
        &store,
        &registry,
        &spec("lodash", VersionSpec::Exact("4.17.21".into())),
    )
    .unwrap_err();

    assert!(
        matches!(err, InstallError::LockedIntegrityMismatch { .. }),
        "a warm store let a lockfile mismatch through, got {err:?}"
    );
}
