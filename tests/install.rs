use std::path::{Path, PathBuf};

use jerky::cli::{PackageSpec, VersionSpec};
use jerky::commands::install::{InstallError, Mode, Outcome, Recorded, Request, install, sync};
use jerky::linker::{Unowned, UnownedReason};
use jerky::resolver::{ImporterPath, Kind, ResolveError};
use jerky::store::Store;
use jerky::testing::{FixtureRegistry, TarEntry, build_tarball, mode_of};
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

/// What an install added, which is what most of these tests are about.
///
/// `install` returns the whole sync outcome, because convergence has entries to
/// report that are nothing to do with the package being added.
fn added(outcome: Outcome) -> Recorded {
    outcome
        .recorded
        .expect("an install always reports what it recorded")
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

    let installed = added(
        install(
            &solo(project_dir),
            &ImporterPath::root(),
            &store,
            &registry,
            &spec("lodash", VersionSpec::Latest),
            None,
        )
        .unwrap(),
    );

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
        None,
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
        None,
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
fn a_hostile_mode_never_reaches_the_project() {
    // The propagation the mode normalisation exists to stop. A store entry is
    // hard-linked into every project that installs the package, so one set of
    // permissions is shared machine-wide: a file left group- or
    // world-writable in the store is writable in every project at once, and
    // rewriting it there changes what all of them import.
    //
    // Asserting at the extraction seam alone would not show this. The claim
    // is about what a developer's `node_modules` ends up holding.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let project_dir = project(work.path());
    let store = Store::new(home.path().join("store"));
    let tarball = build_tarball(&[
        TarEntry::file(
            "package/package.json",
            r#"{"name":"hostile","version":"1.0.0"}"#,
        ),
        TarEntry::file_with_mode("package/index.js", "module.exports = {};", 0o666),
        TarEntry::file_with_mode("package/bin/cli.js", "#!/usr/bin/env node\n", 0o4777),
    ]);
    let registry = FixtureRegistry::new().with_package("hostile", "1.0.0", tarball);

    install(
        &solo(project_dir),
        &ImporterPath::root(),
        &store,
        &registry,
        &spec("hostile", VersionSpec::Latest),
        None,
    )
    .unwrap();

    let installed = project_dir.join("node_modules/hostile");
    assert_eq!(
        mode_of(&installed.join("index.js")),
        0o644,
        "a world-writable file reached the project"
    );
    // The setuid bit is gone, and the one bit worth keeping survived: #20
    // links this into `node_modules/.bin`, where a non-executable target is a
    // runtime failure rather than an install one.
    assert_eq!(
        mode_of(&installed.join("bin/cli.js")),
        0o755,
        "a setuid binary reached the project"
    );
    // The directory the tarball never named, created on the way to the file
    // inside it. Its mode comes from the process umask rather than from any
    // rule of jerky's unless extraction sets it.
    assert_eq!(
        mode_of(&installed.join("bin")),
        0o755,
        "an implicitly created directory kept the umask's mode"
    );

    // The entry root in the store, which is the staging directory renamed
    // into place rather than anything extraction wrote. A world-writable
    // directory here would let another user add files inside a package every
    // project on the machine imports, which is the propagation this whole
    // rule exists to stop.
    //
    // Under a strict umask this assertion also holds without the fix, so it
    // bites only where the hole is real — a developer or CI runner on 0o002
    // or 0o000. That is the case worth guarding.
    let entry = std::fs::read_dir(store.entry_path_root())
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.is_dir() && p.join("package.json").is_file())
        .expect("a store entry exists");
    assert_eq!(mode_of(&entry), 0o755, "the store entry root is too open");
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
        None,
    )
    .unwrap();
    assert_eq!(registry.tarball_calls(), 1);

    install(
        &solo(project_dir),
        &ImporterPath::root(),
        &store,
        &registry,
        &spec("lodash", VersionSpec::Latest),
        None,
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
        None,
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
            &spec("nope", VersionSpec::Latest),
            None
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
        None,
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
        None,
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
        None,
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
        None,
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

    let installed = added(
        install(
            &workspace,
            &ImporterPath::root(),
            &store,
            &FixtureRegistry::new().with_package("lodash", "4.17.21", lodash_tarball()),
            &spec("lodash", VersionSpec::Latest),
            None,
        )
        .unwrap(),
    );

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

    let installed = added(
        install(
            &workspace,
            &ImporterPath::new("apps/web").unwrap(),
            &store,
            &registry,
            &spec("ui", VersionSpec::Exact("workspace:*".into())),
            None,
        )
        .unwrap(),
    );

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
        None,
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
        None,
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
        None,
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

    let installed = added(
        install(
            &workspace,
            &ImporterPath::new("apps/web").unwrap(),
            &store,
            &registry,
            &spec("lodash", VersionSpec::Exact("4.17.21".into())),
            None,
        )
        .unwrap(),
    );

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

    let installed = added(
        install(
            &solo(root),
            &ImporterPath::root(),
            &store,
            &registry,
            &spec("lodash", VersionSpec::Exact("^4.0.0".into())),
            None,
        )
        .unwrap(),
    );

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
        None,
    )
    .unwrap();
    assert_eq!(
        registry.packument_calls(),
        after_first,
        "a repeated range request re-resolved what the lockfile already answered"
    );
}

#[test]
fn every_range_form_is_recorded_as_written() {
    // The rule is "parses as a range, is not a bare version", so it is the
    // shape of the request that decides rather than a list of operators the
    // code knows about. Asserting the *selected* version alongside the
    // recorded string is what keeps this honest: each of these resolves as a
    // real range, and none of them is a string being copied through.
    let cases = [
        ("~4.17.0", "4.17.21"),
        ("~4.17.21", "4.17.21"),
        ("^4.0.0", "4.18.0"),
        ("4.x", "4.18.0"),
        ("4.17.x", "4.17.21"),
        ("4", "4.18.0"),
        (">=4 <5", "4.18.0"),
    ];

    for (requested, expected) in cases {
        let home = TempDir::new().unwrap();
        let work = TempDir::new().unwrap();
        let root = work.path();
        let store = Store::new(home.path().join("store"));
        let registry = FixtureRegistry::new().with_packument(
            "lodash",
            &[("4.17.21", &[]), ("4.18.0", &[]), ("5.0.0", &[])],
        );
        write_manifest(root, r#"{"name":"demo"}"#);

        let installed = added(
            install(
                &solo(root),
                &ImporterPath::root(),
                &store,
                &registry,
                &spec("lodash", VersionSpec::Exact(requested.into())),
                None,
            )
            .unwrap(),
        );

        assert_eq!(
            installed.version, expected,
            "`{requested}` selected the wrong version"
        );
        assert_eq!(
            read_json(&root.join("package.json"))["dependencies"]["lodash"],
            requested,
            "`{requested}` was not recorded as written"
        );
    }
}

#[test]
fn a_range_that_rules_nothing_out_is_refused() {
    // `*` is the one range there is no good answer to. Recording it would put
    // the widest possible drift permission in a manifest whose default exists
    // to avoid one; pinning instead would answer a question nobody asked. So
    // it is refused, before anything is fetched or written.
    //
    // Every spelling, because the check is on what the range admits rather
    // than on how it was typed — a blocklist of literals would catch `*` and
    // miss `x`.
    for requested in ["*", "x", "X", "*.*.*", "x.x.x", ">=0.0.0"] {
        let home = TempDir::new().unwrap();
        let work = TempDir::new().unwrap();
        let root = work.path();
        let registry =
            FixtureRegistry::new().with_packument("lodash", &[("4.17.21", &[]), ("5.0.0", &[])]);
        write_manifest(root, r#"{"name":"demo"}"#);

        let err = install(
            &solo(root),
            &ImporterPath::root(),
            &Store::new(home.path().join("store")),
            &registry,
            &spec("lodash", VersionSpec::Exact(requested.into())),
            None,
        )
        .unwrap_err();

        assert!(
            matches!(err, InstallError::UnconstrainedRange { .. }),
            "`{requested}` was accepted, got {err:?}"
        );
        // Refused before the registry was asked and before anything was
        // written, which is what makes this a rejection rather than a rollback.
        assert_eq!(registry.packument_calls(), 0, "`{requested}` was resolved");
        assert!(
            read_json(&root.join("package.json"))
                .get("dependencies")
                .is_none(),
            "`{requested}` reached the manifest"
        );
        assert!(!root.join("jerky-lock.json").exists());
    }
}

#[test]
fn a_bounded_range_is_not_mistaken_for_a_wildcard() {
    // The refusal is narrow on purpose. `^0.0.0` and `0.x` admit 0.0.0,
    // `>=1.0.0` has no upper bound, and `<9999999.0.0` has one that merely
    // looks enormous; each rules something out, so each stands.
    let registry = FixtureRegistry::new().with_packument(
        "lodash",
        &[("0.0.0", &[]), ("0.1.0", &[]), ("4.17.21", &[])],
    );

    for (requested, expected) in [
        ("^0.0.0", "0.0.0"),
        ("0.x", "0.1.0"),
        (">=1.0.0", "4.17.21"),
        // Bounded, however large the bound looks. Probing with a merely big
        // version rather than the highest expressible one refused this.
        ("<9999999.0.0", "4.17.21"),
    ] {
        let home = TempDir::new().unwrap();
        let work = TempDir::new().unwrap();
        let root = work.path();
        write_manifest(root, r#"{"name":"demo"}"#);

        let installed = added(
            install(
                &solo(root),
                &ImporterPath::root(),
                &Store::new(home.path().join("store")),
                &registry,
                &spec("lodash", VersionSpec::Exact(requested.into())),
                None,
            )
            .unwrap_or_else(|err| panic!("`{requested}` was refused: {err}")),
        );

        assert_eq!(installed.version, expected);
        assert_eq!(
            read_json(&root.join("package.json"))["dependencies"]["lodash"],
            requested
        );
    }
}

#[test]
fn a_dist_tag_other_than_latest_resolves_and_pins() {
    // `next` is not a range, so it takes the tag path — and a tag is the one
    // request that reaches a prerelease, since range resolution excludes them
    // unless the range says otherwise. What lands in the manifest is the
    // version the tag meant, never the tag: `next` names something different
    // next week, which is the opposite of what a manifest is for.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let registry = FixtureRegistry::new()
        .with_packument("lodash", &[("4.17.21", &[]), ("5.0.0-beta.1", &[])])
        .with_dist_tag("lodash", "next", "5.0.0-beta.1");
    write_manifest(root, r#"{"name":"demo"}"#);

    let installed = added(
        install(
            &solo(root),
            &ImporterPath::root(),
            &Store::new(home.path().join("store")),
            &registry,
            &spec("lodash", VersionSpec::Exact("next".into())),
            None,
        )
        .unwrap(),
    );

    assert_eq!(installed.version, "5.0.0-beta.1");
    assert_eq!(linked_version(root, "lodash"), "5.0.0-beta.1");
    assert_eq!(
        read_json(&root.join("package.json"))["dependencies"]["lodash"],
        "5.0.0-beta.1"
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
        None,
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
        None,
    )
    .unwrap();

    install(
        &solo(root),
        &ImporterPath::root(),
        &store,
        &registry,
        &spec("alpha", VersionSpec::Exact("1.0.0".into())),
        None,
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

    install(&workspace, &web, &store, &registry, &request, None).unwrap();
    let after_first = registry.packument_calls();
    assert!(after_first > 0, "the first install resolved nothing");

    // Same workspace, same request, and now a lockfile recording both.
    let workspace = Workspace::discover(root).unwrap();
    install(&workspace, &web, &store, &registry, &request, None).unwrap();

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
        None,
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
        None,
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
fn a_dependency_deleted_by_hand_loses_its_subtree() {
    // Spec 2 §12 deferred pruning until an `uninstall` command existed to
    // trigger it; §2 now settles it the other way, so this is the behaviour
    // that decision promises. `gamma` is reachable only through `alpha`, so it
    // has to go with it — pruning the named package and leaving its subtree
    // behind would be the worst of both.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));
    let registry = FixtureRegistry::new().with_tree(&[
        ("alpha", "1.0.0", &[("gamma", "^1.0.0")]),
        ("gamma", "1.0.0", &[]),
        ("beta", "1.0.0", &[]),
    ]);
    write_manifest(root, r#"{"name":"demo"}"#);

    install(
        &solo(root),
        &ImporterPath::root(),
        &store,
        &registry,
        &spec("alpha", VersionSpec::Exact("1.0.0".into())),
        None,
    )
    .unwrap();

    let lock = read_json(&root.join("jerky-lock.json"));
    assert!(lock["packages"]["alpha@1.0.0"].is_object());
    assert!(
        lock["packages"]["gamma@1.0.0"].is_object(),
        "the transitive dependency was never recorded, so its removal proves nothing"
    );

    // The deletion an `uninstall` command would eventually make: the manifest
    // no longer declares alpha. Any later install is what notices.
    write_manifest(root, r#"{"name":"demo"}"#);

    install(
        &solo(root),
        &ImporterPath::root(),
        &store,
        &registry,
        &spec("beta", VersionSpec::Exact("1.0.0".into())),
        None,
    )
    .unwrap();

    let lock = read_json(&root.join("jerky-lock.json"));
    assert!(lock["packages"]["beta@1.0.0"].is_object());
    assert!(
        lock["packages"]["alpha@1.0.0"].is_null(),
        "a dependency no importer declares survived in the lockfile"
    );
    assert!(
        lock["packages"]["gamma@1.0.0"].is_null(),
        "alpha was pruned but its subtree was left behind"
    );
    assert!(
        lock["importers"]["."]["dependencies"]["alpha"].is_null(),
        "the importer still records a dependency its manifest dropped"
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
        None,
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
        None,
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
        None,
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
        None,
    )
    .unwrap_err();

    assert!(
        matches!(err, InstallError::LockedIntegrityMismatch { .. }),
        "a warm store let a lockfile mismatch through, got {err:?}"
    );
}

// The three tests below guard decisions that #53's own ticket proposed
// reversing. The review that caught them noted the suite passed *with* the
// reversal applied, so each of these fails if someone reapplies it.

#[test]
fn a_request_naming_the_locked_version_does_not_re_resolve() {
    // #53 proposed folding the request into the importer's declared ranges so
    // that `already_satisfies` could be deleted. This is the case that makes
    // the fold wrong: the request names the version the lockfile already
    // resolved to, so it is answered — but it differs from the declared
    // specifier `^4.0.0`, so a fold would mark the importer stale and ask the
    // registry a question that already has its answer.
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
        &spec("lodash", VersionSpec::Exact("^4.0.0".into())),
        None,
    )
    .unwrap();
    assert_eq!(registry.packument_calls_for("lodash"), 1);

    install(
        &solo(root),
        &ImporterPath::root(),
        &store,
        &registry,
        &spec("lodash", VersionSpec::Exact("4.18.0".into())),
        None,
    )
    .unwrap();

    assert_eq!(
        registry.packument_calls_for("lodash"),
        1,
        "naming the version already locked is answered by the lockfile"
    );
    assert_eq!(
        read_json(&root.join("package.json"))["dependencies"]["lodash"],
        "4.18.0",
        "and the pin is still recorded, since a bare version is not a range"
    );
}

#[test]
fn a_bare_request_asks_even_when_the_manifest_declares_a_dist_tag() {
    // #53 proposed flattening `Request::seed` to the string `as_request`
    // produces. A manifest may declare `"lodash": "latest"` — it resolves, and
    // the lockfile then records the specifier `latest`. Against one that does,
    // a flattened seed would equal the recorded specifier and reuse the
    // lockfile, so `jerky install lodash` would stop asking what `latest`
    // means today. #47 requires that it always asks.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));
    let registry = FixtureRegistry::new()
        .with_packument("lodash", &[("4.17.21", &[]), ("4.18.0", &[])])
        .with_tree(&[("alpha", "1.0.0", &[])]);
    write_manifest(
        root,
        r#"{"name":"demo","dependencies":{"lodash":"latest"}}"#,
    );

    // Installing something else resolves the whole workspace, which is what
    // gets `latest` into the lockfile as a specifier.
    install(
        &solo(root),
        &ImporterPath::root(),
        &store,
        &registry,
        &spec("alpha", VersionSpec::Latest),
        None,
    )
    .unwrap();
    let before = registry.packument_calls_for("lodash");

    install(
        &solo(root),
        &ImporterPath::root(),
        &store,
        &registry,
        &spec("lodash", VersionSpec::Latest),
        None,
    )
    .unwrap();

    assert!(
        registry.packument_calls_for("lodash") > before,
        "only the registry can say what `latest` means today"
    );
}

#[test]
fn a_sync_with_no_request_installs_what_the_manifests_declare() {
    // The `None` path has no caller until #19, and an untested branch that
    // exists only for a future ticket is one that ships broken. Two importers,
    // because a suite that only ever sees `.` is not testing workspaces.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    write_manifest(root, r#"{"name":"ws","workspaces":["packages/*"]}"#);
    write_manifest(
        &root.join("packages/ui"),
        r#"{"name":"ui","dependencies":{"lodash":"4.17.21"}}"#,
    );
    write_manifest(
        &root.join("packages/api"),
        r#"{"name":"api","dependencies":{"alpha":"1.0.0"}}"#,
    );

    let store = Store::new(home.path().join("store"));
    let registry = FixtureRegistry::new()
        .with_packument("lodash", &[("4.17.21", &[])])
        .with_tree(&[("alpha", "1.0.0", &[])]);

    let outcome = sync(
        &Workspace::discover(root).unwrap(),
        &store,
        &registry,
        None::<&Request>,
        Mode::Develop,
    )
    .unwrap();

    assert!(outcome.recorded.is_none(), "nothing was requested");
    assert_eq!(
        linked_version(&root.join("packages/ui"), "lodash"),
        "4.17.21"
    );
    assert_eq!(linked_version(&root.join("packages/api"), "alpha"), "1.0.0");

    let mut linked: Vec<String> = outcome
        .linked
        .iter()
        .map(|installed| format!("{}@{}", installed.name, installed.version))
        .collect();
    linked.sort();
    assert_eq!(linked, vec!["alpha@1.0.0", "lodash@4.17.21"]);

    // Written, and describing both importers rather than only the one a
    // request would have named.
    let lock = read_json(&root.join("jerky-lock.json"));
    assert!(lock["importers"]["packages/ui"]["dependencies"]["lodash"].is_object());
    assert!(lock["importers"]["packages/api"]["dependencies"]["alpha"].is_object());
}

#[test]
fn a_members_dev_dependencies_are_installed() {
    // The whole point: a manifest declaring only devDependencies installed
    // nothing at all before this. Two importers, because the member that
    // declares nothing must still come out of the same walk unharmed.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    write_manifest(
        root,
        r#"{"name":"ws","workspaces":["packages/*"],"devDependencies":{"lodash":"4.17.21"}}"#,
    );
    write_manifest(&root.join("packages/ui"), r#"{"name":"ui"}"#);

    let store = Store::new(home.path().join("store"));
    let registry = two_versions_of_lodash();

    sync(
        &Workspace::discover(root).unwrap(),
        &store,
        &registry,
        None::<&Request>,
        Mode::Develop,
    )
    .unwrap();

    assert_eq!(linked_version(root, "lodash"), "4.17.21");
    let lock = read_json(&root.join("jerky-lock.json"));
    assert_eq!(
        lock["importers"]["."]["devDependencies"]["lodash"]["version"],
        "4.17.21"
    );
}

#[test]
fn every_importers_dev_dependencies_are_followed_not_only_the_roots() {
    // The rule is usually phrased "only the root project's", which does not
    // survive workspaces: in a monorepo every member is a first-party project,
    // not just the one keyed `.`. So the root declares nothing here and the
    // assertions are on packages/ui.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    write_manifest(root, r#"{"name":"ws","workspaces":["packages/*"]}"#);
    write_manifest(
        &root.join("packages/ui"),
        r#"{"name":"ui","devDependencies":{"lodash":"4.17.21"}}"#,
    );

    let store = Store::new(home.path().join("store"));
    let registry = two_versions_of_lodash();

    sync(
        &Workspace::discover(root).unwrap(),
        &store,
        &registry,
        None::<&Request>,
        Mode::Develop,
    )
    .unwrap();

    assert_eq!(
        linked_version(&root.join("packages/ui"), "lodash"),
        "4.17.21",
        "a member's devDependencies went unfollowed because it is not the root"
    );
    let lock = read_json(&root.join("jerky-lock.json"));
    assert_eq!(
        lock["importers"]["packages/ui"]["devDependencies"]["lodash"]["version"],
        "4.17.21"
    );
}

#[test]
fn moving_a_dependency_between_sections_makes_its_importer_stale() {
    // Same specifier, different section. This is what the separate blocks buy:
    // with one flat block the edit reads as no change at all, and the lockfile
    // goes on claiming a section the manifest no longer uses.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    write_manifest(root, r#"{"name":"ws","workspaces":["packages/*"]}"#);
    write_manifest(
        &root.join("packages/ui"),
        r#"{"name":"ui","dependencies":{"lodash":"4.17.21"}}"#,
    );
    write_manifest(
        &root.join("packages/api"),
        r#"{"name":"api","dependencies":{"alpha":"1.0.0"}}"#,
    );

    let store = Store::new(home.path().join("store"));
    let registry = two_versions_of_lodash().with_tree(&[("alpha", "1.0.0", &[])]);

    sync(
        &Workspace::discover(root).unwrap(),
        &store,
        &registry,
        None::<&Request>,
        Mode::Develop,
    )
    .unwrap();
    assert_eq!(registry.packument_calls_for("lodash"), 1);

    // Nothing about what is wanted changed — only which section wants it.
    write_manifest(
        &root.join("packages/ui"),
        r#"{"name":"ui","devDependencies":{"lodash":"4.17.21"}}"#,
    );

    sync(
        &Workspace::discover(root).unwrap(),
        &store,
        &registry,
        None::<&Request>,
        Mode::Develop,
    )
    .unwrap();

    assert_eq!(
        registry.packument_calls_for("lodash"),
        2,
        "the section change read as no change, so the importer was reused"
    );
    assert_eq!(
        registry.packument_calls_for("alpha"),
        1,
        "packages/api was re-resolved even though its manifest never changed"
    );

    let lock = read_json(&root.join("jerky-lock.json"));
    assert_eq!(
        lock["importers"]["packages/ui"]["devDependencies"]["lodash"]["version"],
        "4.17.21"
    );
    assert!(
        lock["importers"]["packages/ui"]["dependencies"].is_null(),
        "the lockfile still records the section the manifest moved away from"
    );
}

#[test]
fn a_name_in_both_sections_resolves_as_a_production_dependency() {
    // A contradiction the manifest may not be the user's to fix, so it is
    // resolved the way npm resolves it rather than refused.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    write_manifest(root, r#"{"name":"ws","workspaces":["packages/*"]}"#);
    write_manifest(
        &root.join("packages/ui"),
        r#"{"name":"ui","dependencies":{"lodash":"4.17.21"},"devDependencies":{"lodash":"3.10.1"}}"#,
    );

    let store = Store::new(home.path().join("store"));
    let registry = two_versions_of_lodash();

    sync(
        &Workspace::discover(root).unwrap(),
        &store,
        &registry,
        None::<&Request>,
        Mode::Develop,
    )
    .unwrap();

    assert_eq!(
        linked_version(&root.join("packages/ui"), "lodash"),
        "4.17.21",
        "the devDependencies entry won"
    );
    let lock = read_json(&root.join("jerky-lock.json"));
    assert_eq!(
        lock["importers"]["packages/ui"]["dependencies"]["lodash"]["specifier"],
        "4.17.21"
    );
    assert!(
        lock["importers"]["packages/ui"]["devDependencies"].is_null(),
        "one name resolved to two entries; the graph is keyed by name"
    );
}

// The two tests below cover a bug the review found in the combined #54/#55
// work: `jerky install <pkg>` against a package already declared in
// `devDependencies` wrote it into `dependencies` as well, leaving it in both
// sections permanently. The lockfile settled on the next install; the manifest
// never did, because the prod-wins rule masks the duplicate rather than
// resolving it.

#[test]
fn installing_a_dev_dependency_at_its_locked_version_does_not_duplicate_it() {
    // The reused path: the request is already satisfied, so the importer is
    // taken from the lockfile verbatim and the fold never runs. The manifest
    // write is the only thing that happens, and it must not change the
    // section.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));
    let registry = FixtureRegistry::new().with_packument("lodash", &[("4.17.21", &[])]);
    write_manifest(
        root,
        r#"{"name":"demo","devDependencies":{"lodash":"4.17.21"}}"#,
    );

    sync(
        &Workspace::discover(root).unwrap(),
        &store,
        &registry,
        None::<&Request>,
        Mode::Develop,
    )
    .unwrap();

    install(
        &solo(root),
        &ImporterPath::root(),
        &store,
        &registry,
        &spec("lodash", VersionSpec::Exact("4.17.21".into())),
        None,
    )
    .unwrap();

    let manifest = read_json(&root.join("package.json"));
    assert_eq!(manifest["devDependencies"]["lodash"], "4.17.21");
    assert!(
        manifest["dependencies"].is_null(),
        "the section a user chose is not jerky's to change: {manifest}"
    );

    // And the lockfile still agrees with it, so the next install has nothing
    // to re-resolve.
    let lock = read_json(&root.join("jerky-lock.json"));
    assert_eq!(
        lock["importers"]["."]["devDependencies"]["lodash"]["version"],
        "4.17.21"
    );
    assert!(lock["importers"]["."]["dependencies"].is_null());
}

#[test]
fn a_plain_install_does_not_settle_a_duplicate_it_was_not_asked_about() {
    // A manifest declaring lodash in *both* sections is a contradiction jerky
    // masks with the prod-wins rule rather than resolving in the file, because
    // the manifest may belong to someone else's monorepo or be generated by a
    // tool. `jerky install lodash@4.18.0` asks for a version and nothing else,
    // so the `devDependencies` line has to survive it — deleting that entry
    // would lose a declaration the user never mentioned.
    //
    // Driven through `install` rather than `Manifest` directly: what decides
    // this is the branch in `install` choosing `add_dependency` over
    // `move_dependency`, and a unit test on the manifest cannot see it.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));
    let registry =
        FixtureRegistry::new().with_packument("lodash", &[("4.17.21", &[]), ("4.18.0", &[])]);
    write_manifest(
        root,
        r#"{"name":"demo","dependencies":{"lodash":"4.17.21"},"devDependencies":{"lodash":"3.0.0"}}"#,
    );

    install(
        &solo(root),
        &ImporterPath::root(),
        &store,
        &registry,
        &spec("lodash", VersionSpec::Exact("4.18.0".into())),
        None,
    )
    .unwrap();

    let manifest = read_json(&root.join("package.json"));
    assert_eq!(manifest["dependencies"]["lodash"], "4.18.0");
    assert_eq!(
        manifest["devDependencies"]["lodash"], "3.0.0",
        "a version change must not delete the other declaration: {manifest}"
    );
}

#[test]
fn installing_a_new_version_of_a_dev_dependency_keeps_it_a_dev_dependency() {
    // The re-resolved path: a different version makes the importer stale, so
    // the request *is* folded into what it declares. Hardcoding `Prod` there
    // moved the package to `dependencies` without being asked, and because
    // `already_satisfies` never compares kinds, nothing downstream noticed.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));
    let registry =
        FixtureRegistry::new().with_packument("lodash", &[("4.17.21", &[]), ("4.18.0", &[])]);
    write_manifest(
        root,
        r#"{"name":"demo","devDependencies":{"lodash":"4.17.21"}}"#,
    );

    sync(
        &Workspace::discover(root).unwrap(),
        &store,
        &registry,
        None::<&Request>,
        Mode::Develop,
    )
    .unwrap();

    install(
        &solo(root),
        &ImporterPath::root(),
        &store,
        &registry,
        &spec("lodash", VersionSpec::Exact("4.18.0".into())),
        None,
    )
    .unwrap();

    let manifest = read_json(&root.join("package.json"));
    assert_eq!(manifest["devDependencies"]["lodash"], "4.18.0");
    assert!(
        manifest["dependencies"].is_null(),
        "upgrading a dev dependency must not promote it: {manifest}"
    );

    let lock = read_json(&root.join("jerky-lock.json"));
    assert_eq!(
        lock["importers"]["."]["devDependencies"]["lodash"]["version"],
        "4.18.0"
    );
    assert!(lock["importers"]["."]["dependencies"].is_null());
}

// The `--save-dev` tests. What the flag does is one line of manifest writing;
// what it has to not do is leave the name in the section it came from, which
// is the half that needs a workspace and a lockfile to pin down.

#[test]
fn save_dev_records_under_dev_dependencies() {
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
        Some(Kind::Dev),
    )
    .unwrap();

    let manifest = read_json(&project_dir.join("package.json"));
    assert_eq!(manifest["devDependencies"]["lodash"], "4.17.21");
    assert!(
        manifest["dependencies"].is_null(),
        "the package was declared twice: {manifest}"
    );

    // And the lockfile agrees, so the next install has nothing to re-resolve.
    let lock = read_json(&project_dir.join("jerky-lock.json"));
    assert_eq!(
        lock["importers"]["."]["devDependencies"]["lodash"]["version"],
        "4.17.21"
    );
    assert!(lock["importers"]["."]["dependencies"].is_null());
}

#[test]
fn save_dev_records_in_the_importer_you_are_standing_in() {
    // The same rule as any other install — the flag chooses a section, not an
    // importer. Two members, so there is a wrong manifest for it to land in.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let root_manifest = r#"{"name":"ws","workspaces":["packages/*"]}"#;
    write_manifest(root, root_manifest);
    write_manifest(&root.join("packages/ui"), r#"{"name":"ui"}"#);

    let store = Store::new(home.path().join("store"));
    let registry = FixtureRegistry::new().with_packument("lodash", &[("4.17.21", &[])]);

    install(
        &Workspace::discover(root).unwrap(),
        &ImporterPath::new("packages/ui").unwrap(),
        &store,
        &registry,
        &spec("lodash", VersionSpec::Exact("4.17.21".into())),
        Some(Kind::Dev),
    )
    .unwrap();

    let ui = read_json(&root.join("packages/ui/package.json"));
    assert_eq!(ui["devDependencies"]["lodash"], "4.17.21");

    // Byte-identical rather than field-by-field: a `"devDependencies": {}`
    // added to a manifest nobody asked about is exactly as wrong as an entry
    // in it, and only the bytes catch that.
    let untouched = std::fs::read_to_string(root.join("package.json")).unwrap();
    assert_eq!(
        untouched, root_manifest,
        "installing into packages/ui rewrote the root manifest"
    );

    let lock = read_json(&root.join("jerky-lock.json"));
    assert_eq!(
        lock["importers"]["packages/ui"]["devDependencies"]["lodash"]["specifier"],
        "4.17.21"
    );
}

#[test]
fn installing_over_an_existing_entry_does_not_duplicate_it_across_sections() {
    // `jerky install lodash` then `jerky install --save-dev lodash` must move
    // it. A name in both sections is the contradiction the resolver has to
    // paper over with its prod-wins rule, and the paper never comes off: the
    // lockfile settles and the manifest goes on declaring both.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let project_dir = project(work.path());
    let store = Store::new(home.path().join("store"));
    let registry = FixtureRegistry::new().with_packument("lodash", &[("4.17.21", &[])]);

    for kind in [None, Some(Kind::Dev)] {
        install(
            &solo(project_dir),
            &ImporterPath::root(),
            &store,
            &registry,
            &spec("lodash", VersionSpec::Latest),
            kind,
        )
        .unwrap();
    }

    let manifest = read_json(&project_dir.join("package.json"));
    assert_eq!(manifest["devDependencies"]["lodash"], "4.17.21");
    assert!(
        manifest["dependencies"].is_null(),
        "lodash is declared in both sections: {manifest}"
    );
}

#[test]
fn save_dev_moves_an_entry_the_lockfile_already_satisfies() {
    // The reuse path, which is where a section change is easiest to lose: the
    // manifest matches the lockfile and the requested version is the one
    // already resolved, so nothing about *this package* needs looking up. The
    // section is still a change, and one the lockfile records, so it has to
    // make the importer stale — otherwise the manifest moves, the lockfile
    // does not, and the file goes on describing a section the manifest has
    // abandoned until some later install notices.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));
    let registry = FixtureRegistry::new().with_packument("lodash", &[("4.17.21", &[])]);
    write_manifest(
        root,
        r#"{"name":"demo","dependencies":{"lodash":"4.17.21"}}"#,
    );

    sync(
        &Workspace::discover(root).unwrap(),
        &store,
        &registry,
        None::<&Request>,
        Mode::Develop,
    )
    .unwrap();

    install(
        &solo(root),
        &ImporterPath::root(),
        &store,
        &registry,
        &spec("lodash", VersionSpec::Exact("4.17.21".into())),
        Some(Kind::Dev),
    )
    .unwrap();

    let manifest = read_json(&root.join("package.json"));
    assert_eq!(manifest["devDependencies"]["lodash"], "4.17.21");
    assert!(manifest["dependencies"].is_null(), "{manifest}");

    let lock = read_json(&root.join("jerky-lock.json"));
    assert_eq!(
        lock["importers"]["."]["devDependencies"]["lodash"]["version"],
        "4.17.21"
    );
    assert!(
        lock["importers"]["."]["dependencies"].is_null(),
        "the lockfile still records the section the manifest left"
    );
}

#[test]
fn a_matching_lockfile_makes_no_registry_calls_for_dev_dependencies_either() {
    // #55 pinned this for `dependencies` and #54 never extended it, because
    // each ticket assumed the other owned it. It is the one assertion that
    // catches drift in the kind round-tripping through save and load.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    write_manifest(root, r#"{"name":"ws","workspaces":["packages/*"]}"#);
    write_manifest(
        &root.join("packages/ui"),
        r#"{"name":"ui","devDependencies":{"lodash":"4.17.21"},"dependencies":{"alpha":"1.0.0"}}"#,
    );

    let store = Store::new(home.path().join("store"));
    let registry = FixtureRegistry::new()
        .with_packument("lodash", &[("4.17.21", &[])])
        .with_tree(&[("alpha", "1.0.0", &[])]);

    sync(
        &Workspace::discover(root).unwrap(),
        &store,
        &registry,
        None::<&Request>,
        Mode::Develop,
    )
    .unwrap();
    assert!(registry.packument_calls() > 0, "the first install resolves");

    let before_packuments = registry.packument_calls();
    let before_metadata = registry.metadata_calls();

    sync(
        &Workspace::discover(root).unwrap(),
        &store,
        &registry,
        None::<&Request>,
        Mode::Develop,
    )
    .unwrap();

    assert_eq!(
        registry.packument_calls(),
        before_packuments,
        "a matching lockfile answers for devDependencies too"
    );
    assert_eq!(registry.metadata_calls(), before_metadata);
}

#[test]
fn a_bare_install_installs_everything_the_manifests_declare() {
    // The post-clone command. Three importers, each declaring a different
    // package, none of them installed. The root declares one too, because a
    // root holding nothing but `workspaces` is not the shape of a real
    // monorepo and would leave the root importer untested.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    write_manifest(
        root,
        r#"{"name":"ws","workspaces":["packages/*"],"dependencies":{"alpha":"1.0.0"}}"#,
    );
    write_manifest(
        &root.join("packages/ui"),
        r#"{"name":"ui","dependencies":{"lodash":"4.17.21"}}"#,
    );
    write_manifest(
        &root.join("packages/api"),
        r#"{"name":"api","dependencies":{"beta":"2.0.0"}}"#,
    );
    let manifests = || -> Vec<String> {
        ["", "packages/ui", "packages/api"]
            .iter()
            .map(|dir| std::fs::read_to_string(root.join(dir).join("package.json")).unwrap())
            .collect()
    };
    let before = manifests();

    let store = Store::new(home.path().join("store"));
    let registry = FixtureRegistry::new()
        .with_packument("lodash", &[("4.17.21", &[])])
        .with_tree(&[("alpha", "1.0.0", &[]), ("beta", "2.0.0", &[])]);

    let outcome = sync(
        &Workspace::discover(root).unwrap(),
        &store,
        &registry,
        None::<&Request>,
        Mode::Develop,
    )
    .unwrap();

    assert_eq!(linked_version(root, "alpha"), "1.0.0");
    assert_eq!(
        linked_version(&root.join("packages/ui"), "lodash"),
        "4.17.21"
    );
    assert_eq!(linked_version(&root.join("packages/api"), "beta"), "2.0.0");

    let mut linked: Vec<String> = outcome
        .linked
        .iter()
        .map(|installed| format!("{}@{}", installed.name, installed.version))
        .collect();
    linked.sort();
    assert_eq!(linked, vec!["alpha@1.0.0", "beta@2.0.0", "lodash@4.17.21"]);

    // Nothing was requested, so nothing is recorded — and no manifest is
    // rewritten. A bare install answers the question the manifests already
    // ask; one that edited them on the way would be changing the question.
    assert!(outcome.recorded.is_none());
    assert_eq!(before, manifests(), "a bare install rewrote a package.json");
}

#[test]
fn a_bare_install_covers_every_importer_whatever_the_cwd() {
    // Standing in `packages/ui` and linking only `packages/ui` would write a
    // lockfile describing a root `node_modules` that does not exist — against
    // *nothing is recorded that is not already true on disk*. The cwd reaches
    // an install only through `find_root`, so this asks it the question
    // `main.rs` asks from that directory and syncs what comes back.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    write_manifest(
        root,
        r#"{"name":"ws","workspaces":["packages/*"],"dependencies":{"alpha":"1.0.0"}}"#,
    );
    write_manifest(
        &root.join("packages/ui"),
        r#"{"name":"ui","dependencies":{"lodash":"4.17.21"}}"#,
    );

    let cwd = root.join("packages/ui");
    let found = Workspace::find_root(&cwd).expect("the root manifest sits above packages/ui");
    assert_eq!(
        found,
        root.canonicalize().unwrap(),
        "a member's own manifest stopped the walk"
    );

    let store = Store::new(home.path().join("store"));
    let registry = FixtureRegistry::new()
        .with_packument("lodash", &[("4.17.21", &[])])
        .with_tree(&[("alpha", "1.0.0", &[])]);

    sync(
        &Workspace::discover(&found).unwrap(),
        &store,
        &registry,
        None::<&Request>,
        Mode::Develop,
    )
    .unwrap();

    assert_eq!(linked_version(&cwd, "lodash"), "4.17.21");
    assert_eq!(
        linked_version(root, "alpha"),
        "1.0.0",
        "the importer the user was not standing in was left unlinked"
    );

    let lock = read_json(&root.join("jerky-lock.json"));
    assert!(lock["importers"]["."]["dependencies"]["alpha"].is_object());
    assert!(lock["importers"]["packages/ui"]["dependencies"]["lodash"].is_object());
}

#[test]
fn a_bare_install_with_a_matching_lockfile_makes_no_registry_calls() {
    // The resolved graph is identical either way, so only the call count can
    // tell reuse from a re-resolution that happened to agree. The second run
    // gets a fresh registry holding the same packages: had it asked, the ask
    // would have succeeded and been counted, rather than failing for an
    // unrelated reason.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    write_manifest(
        root,
        r#"{"name":"ws","workspaces":["packages/*"],"dependencies":{"alpha":"1.0.0"}}"#,
    );
    write_manifest(
        &root.join("packages/ui"),
        r#"{"name":"ui","dependencies":{"lodash":"4.17.21"}}"#,
    );

    let store = Store::new(home.path().join("store"));
    let fixtures = || {
        FixtureRegistry::new()
            .with_packument("lodash", &[("4.17.21", &[])])
            .with_tree(&[("alpha", "1.0.0", &[])])
    };

    sync(
        &Workspace::discover(root).unwrap(),
        &store,
        &fixtures(),
        None::<&Request>,
        Mode::Develop,
    )
    .unwrap();

    let registry = fixtures();
    sync(
        &Workspace::discover(root).unwrap(),
        &store,
        &registry,
        None::<&Request>,
        Mode::Develop,
    )
    .unwrap();

    assert_eq!(
        registry.packument_calls(),
        0,
        "a matching lockfile still asked the registry for a version list"
    );
    assert_eq!(
        registry.metadata_calls(),
        0,
        "a matching lockfile still asked the registry for version metadata"
    );
    assert_eq!(
        registry.tarball_calls(),
        0,
        "bytes the store already holds were downloaded again"
    );
    assert_eq!(linked_version(root, "alpha"), "1.0.0");
    assert_eq!(
        linked_version(&root.join("packages/ui"), "lodash"),
        "4.17.21"
    );
}

#[test]
fn a_bare_install_with_no_lockfile_resolves_from_scratch() {
    // The case the command exists for: `package.json` committed,
    // `jerky-lock.json` and `node_modules` both absent because neither is.
    // Ranges rather than pins, since a clone with nothing resolved is exactly
    // where a range still has to be interpreted.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    write_manifest(
        root,
        r#"{"name":"ws","workspaces":["packages/*"],"dependencies":{"alpha":"^1.0.0"}}"#,
    );
    write_manifest(
        &root.join("packages/ui"),
        r#"{"name":"ui","dependencies":{"lodash":"^4.0.0"}}"#,
    );
    assert!(!root.join("jerky-lock.json").exists());
    assert!(!root.join("node_modules").exists());

    let store = Store::new(home.path().join("store"));
    let registry = FixtureRegistry::new()
        .with_packument("lodash", &[("4.17.21", &[])])
        .with_tree(&[("alpha", "1.0.0", &[])]);

    sync(
        &Workspace::discover(root).unwrap(),
        &store,
        &registry,
        None::<&Request>,
        Mode::Develop,
    )
    .unwrap();

    assert!(
        registry.packument_calls() > 0,
        "nothing was asked, so the declared ranges were never interpreted"
    );
    assert_eq!(linked_version(root, "alpha"), "1.0.0");
    assert_eq!(
        linked_version(&root.join("packages/ui"), "lodash"),
        "4.17.21"
    );

    // The lockfile now exists, and records the ranges rather than the versions
    // they chose — which is what lets the next install recognise it as
    // answered.
    let lock = read_json(&root.join("jerky-lock.json"));
    assert_eq!(
        lock["importers"]["."]["dependencies"]["alpha"]["specifier"],
        "^1.0.0"
    );
    assert_eq!(
        lock["importers"]["packages/ui"]["dependencies"]["lodash"]["specifier"],
        "^4.0.0"
    );
}

#[test]
fn a_bare_install_from_a_directory_belonging_to_no_member_still_works() {
    // `jerky install lodash` in `tools/scripts` is ambiguous and errors —
    // `importer_for` has its own test for that. A bare install is not
    // ambiguous: the answer is every importer, so there is no guess to refuse
    // and nothing above `find_root` to ask.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    write_manifest(root, r#"{"name":"ws","workspaces":["packages/*"]}"#);
    write_manifest(
        &root.join("packages/ui"),
        r#"{"name":"ui","dependencies":{"lodash":"4.17.21"}}"#,
    );
    write_manifest(
        &root.join("packages/api"),
        r#"{"name":"api","dependencies":{"alpha":"1.0.0"}}"#,
    );
    let outsider = root.join("tools/scripts");
    std::fs::create_dir_all(&outsider).unwrap();

    let found = Workspace::find_root(&outsider)
        .expect("the root manifest sits above a directory belonging to no member");
    assert_eq!(found, root.canonicalize().unwrap());

    let store = Store::new(home.path().join("store"));
    let registry = FixtureRegistry::new()
        .with_packument("lodash", &[("4.17.21", &[])])
        .with_tree(&[("alpha", "1.0.0", &[])]);

    sync(
        &Workspace::discover(&found).unwrap(),
        &store,
        &registry,
        None::<&Request>,
        Mode::Develop,
    )
    .unwrap();

    assert_eq!(
        linked_version(&root.join("packages/ui"), "lodash"),
        "4.17.21"
    );
    assert_eq!(linked_version(&root.join("packages/api"), "alpha"), "1.0.0");
}

/// A temp directory's path as the *workspace* will report it.
///
/// `Workspace::discover` canonicalizes the root, so every path an install
/// hands back — an `Unowned`'s among them — is canonical. A raw
/// `TempDir::path()` is not: on macOS a temp directory is reached through
/// `/var`, which is a symlink to `/private/var`, so the two spell the same
/// directory differently and a path comparison between them fails there while
/// passing on Linux, where `/tmp` is a real directory. Canonicalizing here is
/// what makes such a comparison test jerky rather than the host's layout.
///
/// Only comparisons need this. Reaching a file through either spelling works,
/// which is why every other test in this file can use `TempDir::path()`.
fn workspace_root(dir: &TempDir) -> PathBuf {
    dir.path()
        .canonicalize()
        .expect("the temp directory was just created")
}

/// A two-importer workspace where `packages/ui` declares one dependency it is
/// about to lose, installed once. Two importers throughout, because the whole
/// risk convergence carries is deleting something it should not have — and a
/// suite that only ever sees `.` cannot catch a removal that reaches an
/// importer it was never asked about.
fn converging_workspace(root: &Path) -> FixtureRegistry {
    write_manifest(
        root,
        r#"{"name":"ws","workspaces":["packages/*"],"dependencies":{"alpha":"1.0.0"}}"#,
    );
    write_manifest(
        &root.join("packages/ui"),
        r#"{"name":"ui","dependencies":{"lodash":"4.17.21","beta":"2.0.0"}}"#,
    );

    FixtureRegistry::new()
        .with_packument("lodash", &[("4.17.21", &[])])
        .with_tree(&[("alpha", "1.0.0", &[]), ("beta", "2.0.0", &[])])
}

/// `packages/ui` after someone deleted `beta` from it by hand.
fn drop_beta(root: &Path) {
    write_manifest(
        &root.join("packages/ui"),
        r#"{"name":"ui","dependencies":{"lodash":"4.17.21"}}"#,
    );
}

fn bare_install(root: &Path, store: &Store, registry: &FixtureRegistry) -> Outcome {
    sync(
        &Workspace::discover(root).unwrap(),
        store,
        registry,
        None::<&Request>,
        Mode::Develop,
    )
    .unwrap()
}

/// Does anything at all still sit here? `exists` follows links, so it answers
/// `false` for a dangling one — and a link left dangling is exactly the failure
/// these tests are watching for.
fn still_there(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

#[test]
fn a_dependency_removed_from_the_manifest_loses_its_link() {
    // Install is convergent, not additive. Without this the symlink survives
    // and still resolves, so `require("beta")` goes on working for a
    // dependency the project no longer declares and the tree quietly disagrees
    // with the manifest.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));
    let registry = converging_workspace(root);

    bare_install(root, &store, &registry);
    assert_eq!(linked_version(&root.join("packages/ui"), "beta"), "2.0.0");

    drop_beta(root);
    let outcome = bare_install(root, &store, &registry);

    assert!(
        !still_there(&root.join("packages/ui/node_modules/beta")),
        "a dependency the manifest no longer declares kept its link"
    );
    assert!(
        outcome.left_alone.is_empty(),
        "a link jerky wrote was reported as someone else's rather than removed"
    );

    // The two directions convergence must not confuse: a sibling in the same
    // importer, and another importer entirely.
    assert_eq!(
        linked_version(&root.join("packages/ui"), "lodash"),
        "4.17.21",
        "convergence took a dependency the manifest still declares"
    );
    assert_eq!(
        linked_version(root, "alpha"),
        "1.0.0",
        "converging packages/ui reached into the root importer"
    );
}

#[test]
fn its_virtual_store_entry_goes_with_it() {
    // The link is only half of what an install put on disk. Leaving the
    // unpacked tree behind would mean a deleted dependency stays in the project
    // forever, because nothing else ever removes one.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));
    let registry = converging_workspace(root);

    bare_install(root, &store, &registry);
    assert!(root.join("node_modules/.jerky/beta@2.0.0").is_dir());

    drop_beta(root);
    bare_install(root, &store, &registry);

    assert!(
        !still_there(&root.join("node_modules/.jerky/beta@2.0.0")),
        "the unpacked tree outlived the dependency it belongs to"
    );
    assert!(
        root.join("node_modules/.jerky/lodash@4.17.21").is_dir(),
        "an entry the graph still contains was pruned with it"
    );
    assert!(
        root.join("node_modules/.jerky/alpha@1.0.0").is_dir(),
        "the root importer's entry was pruned by converging packages/ui"
    );
}

#[test]
fn a_real_directory_left_by_npm_survives_and_is_reported() {
    // The first `jerky install` in a repository that has seen npm must not be a
    // destructive surprise. jerky only ever writes symlinks into an importer's
    // `node_modules`, so a real directory is provably someone else's.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    // Canonical, because this test compares a reported path against one it
    // built itself. See `workspace_root`.
    let root = &workspace_root(&work);
    let store = Store::new(home.path().join("store"));
    let registry = converging_workspace(root);

    let stray = root.join("packages/ui/node_modules/left-by-npm");
    write_manifest(&stray, r#"{"name":"left-by-npm","version":"0.0.1"}"#);

    let outcome = bare_install(root, &store, &registry);

    assert_eq!(
        std::fs::read_to_string(stray.join("package.json")).unwrap(),
        r#"{"name":"left-by-npm","version":"0.0.1"}"#,
        "jerky deleted a directory it did not write"
    );

    let reported: Vec<&Unowned> = outcome
        .left_alone
        .iter()
        .filter(|entry| entry.path == stray)
        .collect();
    assert_eq!(reported.len(), 1, "left alone: {:?}", outcome.left_alone);
    assert!(matches!(reported[0].reason, UnownedReason::NotASymlink));
}

#[test]
fn a_symlink_pointing_outside_the_workspace_survives_and_is_reported() {
    // Someone's `npm link`. It is a symlink, so the shape is jerky's, but it
    // resolves neither into this workspace's virtual store nor onto a member —
    // not ours, so not ours to remove.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let elsewhere = TempDir::new().unwrap();
    // Canonical, for the same reason as the test above.
    let root = &workspace_root(&work);
    let store = Store::new(home.path().join("store"));
    let registry = converging_workspace(root);

    let checkout = elsewhere.path().join("my-lib");
    write_manifest(&checkout, r#"{"name":"my-lib","version":"9.9.9"}"#);
    let link = root.join("packages/ui/node_modules/my-lib");
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&checkout, &link).unwrap();

    let outcome = bare_install(root, &store, &registry);

    assert_eq!(
        linked_version(&root.join("packages/ui"), "my-lib"),
        "9.9.9",
        "a link jerky did not write was removed"
    );

    let reported: Vec<&Unowned> = outcome
        .left_alone
        .iter()
        .filter(|entry| entry.path == link)
        .collect();
    assert_eq!(reported.len(), 1, "left alone: {:?}", outcome.left_alone);
    assert!(matches!(reported[0].reason, UnownedReason::PointsOutside));
}

#[test]
fn the_content_store_is_never_touched() {
    // `~/.jerky/store` is machine-global and shared by every project on the
    // machine, so a project-local convergence has no basis for deciding one of
    // its entries is dead. Collecting it is a separate command (#52); pruning a
    // project's virtual store must leave it exactly as it found it.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));
    let registry = converging_workspace(root);

    // Content-addressed entries only. The store's own `.staging` scratch
    // directory sits beside them and is nobody's package.
    let stored = || -> Vec<PathBuf> {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(store.entry_path_root())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| !path.file_name().unwrap().to_string_lossy().starts_with('.'))
            .collect();
        entries.sort();
        entries
    };

    bare_install(root, &store, &registry);
    let before = stored();
    assert_eq!(before.len(), 3, "three packages were installed");

    drop_beta(root);
    bare_install(root, &store, &registry);

    assert!(
        !still_there(&root.join("node_modules/.jerky/beta@2.0.0")),
        "the last project-local reference to beta was not actually pruned, so \
         this proves nothing about the store"
    );
    assert_eq!(
        before,
        stored(),
        "convergence reached into the machine-global content store"
    );
}

// --- `jerky install --production` -------------------------------------------

/// A two-importer workspace where each importer declares one of each kind.
///
/// Two importers throughout, because `--production` acts on all of them at
/// once and a suite that only ever saw `.` could not catch a mode applied to
/// one importer and not the next. Each has a devDependency so that "installs
/// `dependencies` only" is a claim with something to be false about.
fn production_workspace(root: &Path) -> FixtureRegistry {
    write_manifest(
        root,
        r#"{"name":"ws","workspaces":["packages/*"],"dependencies":{"alpha":"1.0.0"},"devDependencies":{"dev-root":"3.0.0"}}"#,
    );
    write_manifest(
        &root.join("packages/ui"),
        r#"{"name":"ui","dependencies":{"beta":"2.0.0"},"devDependencies":{"dev-ui":"4.0.0"}}"#,
    );

    FixtureRegistry::new().with_tree(&[
        ("alpha", "1.0.0", &[]),
        ("beta", "2.0.0", &[]),
        ("dev-root", "3.0.0", &[]),
        ("dev-ui", "4.0.0", &[]),
    ])
}

fn production_install(
    root: &Path,
    store: &Store,
    registry: &FixtureRegistry,
) -> Result<Outcome, InstallError> {
    sync(
        &Workspace::discover(root).unwrap(),
        store,
        registry,
        None::<&Request>,
        Mode::Production,
    )
}

#[test]
fn production_installs_dependencies_and_not_dev_dependencies() {
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));
    let registry = production_workspace(root);

    bare_install(root, &store, &registry);
    std::fs::remove_dir_all(root.join("node_modules")).unwrap();

    production_install(root, &store, &registry).unwrap();

    assert_eq!(linked_version(root, "alpha"), "1.0.0");
    assert_eq!(linked_version(&root.join("packages/ui"), "beta"), "2.0.0");
    assert!(
        !still_there(&root.join("node_modules/dev-root")),
        "the root's devDependency was linked by a production install"
    );
    assert!(
        !still_there(&root.join("packages/ui/node_modules/dev-ui")),
        "a member's devDependency was linked by a production install"
    );
}

#[test]
fn production_removes_dev_dependency_links_that_are_already_there() {
    // Convergence is not selectively applied. A `node_modules` that has seen a
    // normal install is not left half-production, because the mode describes
    // the tree jerky guarantees rather than the work it happens to do.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));
    let registry = production_workspace(root);

    bare_install(root, &store, &registry);
    assert!(still_there(&root.join("node_modules/dev-root")));
    assert!(still_there(&root.join("packages/ui/node_modules/dev-ui")));

    production_install(root, &store, &registry).unwrap();

    assert!(
        !still_there(&root.join("node_modules/dev-root")),
        "a devDependency link survived a production install"
    );
    assert!(
        !still_there(&root.join("packages/ui/node_modules/dev-ui")),
        "a member's devDependency link survived a production install"
    );
    assert_eq!(linked_version(root, "alpha"), "1.0.0");
    assert_eq!(linked_version(&root.join("packages/ui"), "beta"), "2.0.0");
}

#[test]
fn production_leaves_the_lockfile_byte_identical() {
    // Asserted on bytes, because bytes are the actual guarantee. A run that
    // rewrote the file it exists to reproduce would be the bug, and a
    // structural comparison would not notice a reordering or a reformat.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));
    let registry = production_workspace(root);

    bare_install(root, &store, &registry);
    let before = std::fs::read(root.join("jerky-lock.json")).unwrap();

    production_install(root, &store, &registry).unwrap();

    let after = std::fs::read(root.join("jerky-lock.json")).unwrap();
    assert_eq!(
        before, after,
        "`--production` rewrote the lockfile it exists to reproduce"
    );
}

#[test]
fn production_without_a_lockfile_errors() {
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    // Canonical, because the error names a path and this test compares it
    // against one built here. See `workspace_root`.
    let root = &workspace_root(&work);
    let store = Store::new(home.path().join("store"));
    let registry = production_workspace(root);

    let err = production_install(root, &store, &registry)
        .expect_err("a production install has nothing to reproduce without a lockfile");

    let InstallError::ProductionLockfileMissing { path } = err else {
        panic!("expected ProductionLockfileMissing, got {err}");
    };
    assert_eq!(path, root.join("jerky-lock.json"));
}

#[test]
fn production_against_an_edited_manifest_errors_and_writes_nothing() {
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));
    let registry = production_workspace(root);

    bare_install(root, &store, &registry);
    let lockfile_before = std::fs::read(root.join("jerky-lock.json")).unwrap();
    std::fs::remove_dir_all(root.join("node_modules")).unwrap();

    // A dependency bumped in the manifest and never reinstalled.
    write_manifest(
        &root.join("packages/ui"),
        r#"{"name":"ui","dependencies":{"beta":"2.5.0"},"devDependencies":{"dev-ui":"4.0.0"}}"#,
    );

    let err = production_install(root, &store, &registry)
        .expect_err("the lockfile no longer describes what packages/ui declares");

    let InstallError::ProductionLockfileStale {
        importer,
        name,
        declared,
        locked,
    } = err
    else {
        panic!("expected ProductionLockfileStale, got {err}");
    };
    assert_eq!(importer, "packages/ui");
    assert_eq!(name, "beta");
    assert!(declared.contains("2.5.0"), "declared was {declared}");
    assert!(locked.contains("2.0.0"), "locked was {locked}");

    assert_eq!(
        std::fs::read(root.join("jerky-lock.json")).unwrap(),
        lockfile_before,
        "a refused production install rewrote the lockfile"
    );
    assert!(
        !still_there(&root.join("node_modules")),
        "a refused production install linked part of the tree anyway"
    );
}

#[test]
fn production_fails_on_a_stale_dev_dependency_too() {
    // Even though no devDependency would have been linked. Ignoring it would
    // let CI pass on a lockfile that is genuinely out of date, which is the one
    // thing this mode exists to refuse.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));
    let registry = production_workspace(root);

    bare_install(root, &store, &registry);

    write_manifest(
        &root.join("packages/ui"),
        r#"{"name":"ui","dependencies":{"beta":"2.0.0"},"devDependencies":{"dev-ui":"4.5.0"}}"#,
    );

    let err = production_install(root, &store, &registry)
        .expect_err("a stale devDependency is still a stale lockfile");

    let InstallError::ProductionLockfileStale { name, .. } = err else {
        panic!("expected ProductionLockfileStale, got {err}");
    };
    assert_eq!(name, "dev-ui");
}

#[test]
fn production_makes_no_metadata_requests() {
    // With every importer reusable there is nothing to resolve, so the only
    // traffic is tarballs the store does not already hold. Counted, because the
    // resolved graph is identical either way and only the count can tell the
    // paths apart.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));
    let registry = production_workspace(root);

    bare_install(root, &store, &registry);
    std::fs::remove_dir_all(root.join("node_modules")).unwrap();
    let metadata_before = registry.metadata_calls();
    let packuments_before = registry.packument_calls();

    production_install(root, &store, &registry).unwrap();

    assert_eq!(
        registry.metadata_calls(),
        metadata_before,
        "a production install asked the registry about a version"
    );
    assert_eq!(
        registry.packument_calls(),
        packuments_before,
        "a production install fetched a packument"
    );
}

#[test]
fn production_fails_on_a_dependency_deleted_from_the_manifest() {
    // Staleness from the far side: the manifest dropped a dependency the
    // lockfile still records. Nothing would have been linked for it, and a
    // check that only walked the manifest would never look at it — but the
    // lockfile no longer describes the project, which is the whole question
    // `--production` asks before it does anything.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));
    let registry = production_workspace(root);

    bare_install(root, &store, &registry);

    write_manifest(
        &root.join("packages/ui"),
        r#"{"name":"ui","devDependencies":{"dev-ui":"4.0.0"}}"#,
    );

    let err = production_install(root, &store, &registry)
        .expect_err("the lockfile records a dependency packages/ui no longer declares");

    let InstallError::ProductionLockfileStale {
        importer,
        name,
        declared,
        locked,
    } = err
    else {
        panic!("expected ProductionLockfileStale, got {err}");
    };
    assert_eq!(importer, "packages/ui");
    assert_eq!(name, "beta");
    assert_eq!(declared, "nothing");
    assert!(locked.contains("2.0.0"), "locked was {locked}");
}

#[test]
fn production_names_the_section_when_only_the_section_moved() {
    // A dependency moved between sections at an unchanged specifier is a real
    // edit. An error printing specifiers alone would read as `4.0.0`
    // disagreeing with `4.0.0`, which tells the user nothing.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));
    let registry = production_workspace(root);

    bare_install(root, &store, &registry);

    write_manifest(
        &root.join("packages/ui"),
        r#"{"name":"ui","dependencies":{"beta":"2.0.0","dev-ui":"4.0.0"}}"#,
    );

    let err = production_install(root, &store, &registry)
        .expect_err("dev-ui moved sections without its specifier changing");

    let InstallError::ProductionLockfileStale {
        name,
        declared,
        locked,
        ..
    } = err
    else {
        panic!("expected ProductionLockfileStale, got {err}");
    };
    assert_eq!(name, "dev-ui");
    assert_eq!(declared, "`4.0.0` in dependencies");
    assert_eq!(locked, "`4.0.0` in devDependencies");
}

#[test]
fn production_fails_when_the_lockfile_records_an_importer_the_workspace_lost() {
    // The mode's own failure in miniature. A member dropped from `workspaces`
    // and committed without reinstalling leaves a lockfile that `jerky install`
    // would rewrite — so `--production` reporting success on it is CI passing
    // on a stale file, which is the one thing this mode exists to refuse.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));
    let registry = production_workspace(root);

    bare_install(root, &store, &registry);

    write_manifest(
        root,
        r#"{"name":"ws","dependencies":{"alpha":"1.0.0"},"devDependencies":{"dev-root":"3.0.0"}}"#,
    );

    let err = production_install(root, &store, &registry)
        .expect_err("the lockfile still records an importer the workspace dropped");

    let InstallError::ProductionLockfileImporterGone { importer } = err else {
        panic!("expected ProductionLockfileImporterGone, got {err}");
    };
    assert_eq!(importer, "packages/ui");
}

#[test]
fn production_accepts_a_workspace_whose_importers_all_still_exist() {
    // The other side of the check above: it must refuse a *dropped* importer
    // without refusing every ordinary workspace along with it.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));
    let registry = production_workspace(root);

    bare_install(root, &store, &registry);

    production_install(root, &store, &registry)
        .expect("nothing about this workspace changed between the two installs");
}

// ---------------------------------------------------------------------------
// Parallel tarball fetching (#33, opportunity 1)
// ---------------------------------------------------------------------------

/// A registry holding `count` independent packages, `pkg00` upward.
///
/// Independent on purpose: what the fan-out tests exercise is work that is all
/// known up front, which is exactly what a resolved graph is. Kept separate
/// from the manifest so a test can stock the registry with a package the
/// project does not yet declare.
fn wide_registry(count: usize) -> FixtureRegistry {
    let mut registry = FixtureRegistry::new();
    for i in 0..count {
        let name = format!("pkg{i:02}");
        registry = registry.with_package(
            &name,
            "1.0.0",
            build_tarball(&[TarEntry::file(
                "package/package.json",
                &format!(r#"{{"name":"{name}","version":"1.0.0"}}"#),
            )]),
        );
    }
    registry
}

/// A manifest declaring the first `count` of those packages at an exact pin.
fn wide_manifest(count: usize) -> String {
    let declared: Vec<String> = (0..count)
        .map(|i| format!(r#""pkg{i:02}": "1.0.0""#))
        .collect();
    format!(
        r#"{{"name":"demo","dependencies":{{{}}}}}"#,
        declared.join(",")
    )
}

#[test]
fn tarballs_are_fetched_concurrently() {
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));

    let registry = wide_registry(4).with_tarball_rendezvous(4);
    write_manifest(root, &wide_manifest(4));

    sync(&solo(root), &store, &registry, None, Mode::Develop).unwrap();

    // The rendezvous, not the high-water mark: a serial installer can reach a
    // high-water mark of one and still pass a "> 1" assertion on a loaded
    // machine by never being observed. It cannot satisfy a rendezvous.
    assert!(
        registry.tarballs_met_rendezvous(),
        "four tarball fetches never overlapped, so the install is still serial"
    );
}

#[test]
fn concurrency_is_bounded() {
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));

    // Comfortably more packages than the cap, so an unbounded implementation —
    // one thread per package — is visible as a peak above it. Unbounded fan-out
    // at the registry is what gets an IP rate-limited.
    let count = jerky::registry::MAX_CONCURRENT_FETCHES * 2;
    let registry =
        wide_registry(count).with_tarball_rendezvous(jerky::registry::MAX_CONCURRENT_FETCHES);
    write_manifest(root, &wide_manifest(count));

    sync(&solo(root), &store, &registry, None, Mode::Develop).unwrap();

    assert!(
        registry.tarballs_met_rendezvous(),
        "the pool never reached its own cap, so the cap is not the limit in force"
    );
    // Necessary but not sufficient, and deliberately so. `peak_concurrent_
    // tarballs` is an exact peak, so a peak above the cap is a real failure —
    // but an unbounded pool is not *guaranteed* to be caught, since it would
    // have to be observed above the cap rather than merely be capable of it.
    // Proving the upper bound outright means a rendezvous one wider than the
    // cap, asserted to time out, and that costs `RENDEZVOUS_TIMEOUT` on every
    // run of a suite that finishes in well under a second.
    assert!(
        registry.peak_concurrent_tarballs() <= jerky::registry::MAX_CONCURRENT_FETCHES,
        "fetched {} at once, above the cap of {}",
        registry.peak_concurrent_tarballs(),
        jerky::registry::MAX_CONCURRENT_FETCHES
    );
    assert_eq!(
        registry.tarball_calls(),
        count,
        "every package is fetched once"
    );
}

#[test]
fn a_corrupt_package_is_named_even_among_many() {
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));

    let registry = wide_registry(6);
    write_manifest(root, &wide_manifest(6));
    // Replaces `pkg03`'s entry with one whose recorded hash does not match its
    // bytes. The name has to survive the fan-out; "one of six downloads
    // failed" is not a diagnosable error.
    let registry = registry.with_corrupt_package("pkg03", "1.0.0", lodash_tarball());

    let err = sync(&solo(root), &store, &registry, None, Mode::Develop).unwrap_err();

    match err {
        InstallError::Integrity { name, version, .. } => {
            assert_eq!(name, "pkg03");
            assert_eq!(version, "1.0.0");
        }
        other => panic!("expected an integrity failure naming the package, got {other:?}"),
    }
}

#[test]
fn the_reported_failure_does_not_depend_on_which_thread_lost() {
    // Forty packages against sixteen workers, so most indices are claimed in
    // later rounds rather than all at once in the first, and the two corrupt
    // ones sit far apart. A fresh store *every* iteration is what makes this
    // test mean something: sharing one across iterations warms it, `missing`
    // shrinks to the two corrupt packages, and every run then trivially
    // claims both in the first round — which is a test that passes without
    // exercising the ordering it claims to check.
    //
    // What must hold is that the lowest-indexed failure is always the one
    // reported. Every claim is a `fetch_add`, so the claimed indices are a
    // contiguous prefix whatever the workers do; the failure flag decides only
    // how far that prefix extends. The lowest index that fails is therefore
    // always inside it — the prefix stopped growing *because* something in it
    // failed — so a `BTreeMap` keyed by index picks the same package every
    // time. An install that blamed a different package on each run would be
    // untriageable.
    let mut reported = std::collections::BTreeSet::new();
    for _ in 0..40 {
        let home = TempDir::new().unwrap();
        let work = TempDir::new().unwrap();
        let root = work.path();
        let store = Store::new(home.path().join("store"));
        let registry = wide_registry(40)
            .with_corrupt_package("pkg03", "1.0.0", lodash_tarball())
            .with_corrupt_package("pkg30", "1.0.0", lodash_tarball());
        write_manifest(root, &wide_manifest(40));

        match sync(&solo(root), &store, &registry, None, Mode::Develop).unwrap_err() {
            InstallError::Integrity { name, .. } => {
                reported.insert(name);
            }
            other => panic!("expected an integrity failure, got {other:?}"),
        }
    }

    assert_eq!(
        reported.len(),
        1,
        "the reported package varied across runs: {reported:?}"
    );
    assert!(
        reported.contains("pkg03"),
        "expected the lowest-indexed failure, got {reported:?}"
    );
}

#[test]
fn a_locked_integrity_mismatch_is_caught_before_anything_is_downloaded() {
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));

    let registry = wide_registry(5);
    write_manifest(root, &wide_manifest(4));
    sync(&solo(root), &store, &registry, None, Mode::Develop).unwrap();

    // Re-point one entry's recorded integrity at bytes it does not describe,
    // which is what a republished tarball looks like from here. Parseable but
    // wrong: an unparseable digest is a different error raised in a different
    // place.
    let lock_path = root.join("jerky-lock.json");
    let mut lock: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&lock_path).unwrap()).unwrap();
    lock["packages"]["pkg02@1.0.0"]["integrity"] = serde_json::Value::String(
        "sha512-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=="
            .into(),
    );
    std::fs::write(&lock_path, serde_json::to_string_pretty(&lock).unwrap()).unwrap();

    // The importer has to be *re-resolved* for the gate to have two hashes to
    // compare: a reused importer carries the lockfile's own integrity into the
    // graph, where it necessarily agrees with itself. So the request names
    // `pkg04`, which the manifest does not yet declare — asking again for a
    // package already pinned at the version recorded would match the lockfile
    // and be reused, which is the behaviour #47 added and not a flaw in the
    // gate.
    //
    // A fresh store and a fresh registry, so every package would otherwise be
    // downloaded and the count below starts from zero.
    let cold_home = TempDir::new().unwrap();
    let cold_store = Store::new(cold_home.path().join("store"));
    let registry = wide_registry(5);

    let err = install(
        &solo(root),
        &ImporterPath::root(),
        &cold_store,
        &registry,
        &spec("pkg04", VersionSpec::Exact("1.0.0".into())),
        None,
    )
    .unwrap_err();

    assert!(
        matches!(err, InstallError::LockedIntegrityMismatch { ref name, .. } if name == "pkg02"),
        "expected a locked-integrity refusal naming pkg02, got {err:?}"
    );
    // The gate is worth nothing if it fires after the bytes are already pulled.
    // Serially it only held for packages ordered after the offending one; with
    // sixteen workers in flight there is no "after", which is why the check is
    // now a pass of its own ahead of the pool.
    assert_eq!(
        registry.tarball_calls(),
        0,
        "tarballs were downloaded before the locked-integrity gate ran"
    );
}

/// A registry holding one real package plus a caller that reaches it under a
/// different name — the `@isaacs/cliui` shape, reduced.
fn aliasing_registry() -> FixtureRegistry {
    // `with_packument` throughout: it builds each version's tarball from the
    // name and version it is given, and unlike `with_package` it carries the
    // dependency edges — which is the whole point here.
    FixtureRegistry::new()
        .with_packument("string-width", &[("4.2.3", &[])])
        .with_packument(
            "cliui",
            &[("1.0.0", &[("width-cjs", "npm:string-width@^4.0.0")])],
        )
}

#[test]
fn an_aliased_dependency_is_linked_under_its_local_name() {
    // The link a package sees must be the name it wrote in its own source —
    // `require('width-cjs')` — while the directory it lands on is the real
    // package. One name for the link, another for the target.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));

    write_manifest(root, r#"{"name":"demo","dependencies":{"cliui":"1.0.0"}}"#);
    sync(
        &solo(root),
        &store,
        &aliasing_registry(),
        None,
        Mode::Develop,
    )
    .unwrap();

    let owner = root.join("node_modules/.jerky/cliui@1.0.0/node_modules");
    let link = owner.join("width-cjs");
    assert!(
        std::fs::symlink_metadata(&link).is_ok(),
        "the alias was not linked under the name its dependent uses"
    );

    // Read through it: proves the link resolves and lands on the real package
    // rather than on a directory named after the alias.
    let raw = std::fs::read_to_string(link.join("package.json")).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(parsed["name"], "string-width");
    assert_eq!(parsed["version"], "4.2.3");

    // And the store holds it under its own name, not the alias.
    assert!(
        root.join("node_modules/.jerky/string-width@4.2.3").is_dir(),
        "the store entry is not keyed by the real package"
    );
    assert!(
        !root.join("node_modules/.jerky/width-cjs@4.2.3").exists(),
        "the local name became a store entry of its own"
    );
}

#[test]
fn an_importer_may_alias_a_package_itself() {
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));

    write_manifest(
        root,
        r#"{"name":"demo","dependencies":{"width-cjs":"npm:string-width@^4.0.0"}}"#,
    );
    sync(
        &solo(root),
        &store,
        &aliasing_registry(),
        None,
        Mode::Develop,
    )
    .unwrap();

    let raw = std::fs::read_to_string(root.join("node_modules/width-cjs/package.json")).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(parsed["name"], "string-width");
    assert_eq!(parsed["version"], "4.2.3");
}

#[test]
fn an_aliased_install_reuses_its_lockfile() {
    // The round trip that matters in practice: resolve once, then install
    // again from what was recorded and reach the registry not at all. A
    // lockfile that lost the alias would re-resolve, or fail outright.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));
    let registry = aliasing_registry();

    write_manifest(root, r#"{"name":"demo","dependencies":{"cliui":"1.0.0"}}"#);
    sync(&solo(root), &store, &registry, None, Mode::Develop).unwrap();

    let before = registry.packument_calls();
    std::fs::remove_dir_all(root.join("node_modules")).unwrap();
    sync(&solo(root), &store, &registry, None, Mode::Develop).unwrap();

    assert_eq!(
        registry.packument_calls(),
        before,
        "the second install re-resolved, so the lockfile did not describe the alias"
    );
    let raw = std::fs::read_to_string(
        root.join("node_modules/.jerky/cliui@1.0.0/node_modules/width-cjs/package.json"),
    )
    .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(parsed["name"], "string-width");
}

#[test]
fn installing_an_alias_records_the_scheme_not_just_the_version() {
    // #73 asks the CLI to support this or refuse it, and specifically not to
    // half-work. Recording the bare version is the half-working answer: the
    // link is right, and the *next* install asks the registry for a
    // `width-cjs@4.2.3` that does not exist.
    //
    // jerky pins by default, so the pin goes inside the scheme rather than
    // replacing it.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));

    write_manifest(root, r#"{"name":"demo"}"#);
    let outcome = install(
        &solo(root),
        &ImporterPath::root(),
        &store,
        &aliasing_registry(),
        &spec(
            "width-cjs",
            VersionSpec::Exact("npm:string-width@^4.0.0".to_string()),
        ),
        None,
    )
    .unwrap();

    assert_eq!(added(outcome).specifier, "npm:string-width@^4.0.0");

    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("package.json")).unwrap()).unwrap();
    assert_eq!(
        manifest["dependencies"]["width-cjs"], "npm:string-width@^4.0.0",
        "a range the user typed is theirs to keep, inside the scheme"
    );
}

#[test]
fn an_alias_without_a_range_is_pinned_inside_the_scheme() {
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));

    write_manifest(root, r#"{"name":"demo"}"#);
    install(
        &solo(root),
        &ImporterPath::root(),
        &store,
        &aliasing_registry(),
        &spec(
            "width-cjs",
            VersionSpec::Exact("npm:string-width".to_string()),
        ),
        None,
    )
    .unwrap();

    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("package.json")).unwrap()).unwrap();
    assert_eq!(
        manifest["dependencies"]["width-cjs"],
        "npm:string-width@4.2.3"
    );

    // And what was recorded is installable on its own terms: a second install
    // from that manifest alone resolves without the request that created it.
    let fresh = TempDir::new().unwrap();
    let store = Store::new(fresh.path().join("store"));
    std::fs::remove_dir_all(root.join("node_modules")).unwrap();
    std::fs::remove_file(root.join("jerky-lock.json")).unwrap();
    sync(
        &solo(root),
        &store,
        &aliasing_registry(),
        None,
        Mode::Develop,
    )
    .unwrap();

    assert_eq!(linked_version(root, "width-cjs"), "4.2.3");
}

/// A scoped package, a second one in the same scope, and an unscoped package
/// that depends on one of them.
fn scoped_registry() -> FixtureRegistry {
    FixtureRegistry::new()
        .with_packument("@types/node", &[("20.0.0", &[])])
        .with_packument("@types/react", &[("18.0.0", &[])])
        .with_packument("uses-types", &[("1.0.0", &[("@types/node", "^20.0.0")])])
        // A scoped package that depends on things, so the *owner* of a link
        // inside the store is itself nested one level deeper.
        .with_packument(
            "@nodelib/fs.walk",
            &[("1.2.8", &[("fastq", "^1.0.0"), ("@types/node", "^20.0.0")])],
        )
        .with_packument("fastq", &[("1.17.1", &[])])
}

/// Every symlink under `dir` that resolves to nothing.
///
/// Symlinked directories are deliberately not followed: the virtual store's
/// links point back into the store, so following them does not terminate. An
/// unreadable directory is a panic rather than a skip, because a helper that
/// passes by not looking is worse than no helper.
fn dangling_links(dir: &Path) -> Vec<PathBuf> {
    let mut dangling = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let entries = std::fs::read_dir(&next)
            .unwrap_or_else(|err| panic!("cannot read {}: {err}", next.display()));
        for entry in entries.flatten() {
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            if meta.file_type().is_symlink() {
                if !path.exists() {
                    dangling.push(path);
                }
            } else if meta.is_dir() {
                stack.push(path);
            }
        }
    }
    dangling
}

#[test]
fn a_scoped_packages_own_dependencies_are_linked_from_where_it_sits() {
    // A scoped owner sits one level deeper in the virtual store, so the climb
    // out of it to reach a sibling entry is one longer. Getting this wrong
    // produces links that *exist* — so anything checking for presence passes —
    // and resolve to nothing.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));

    write_manifest(
        root,
        r#"{"name":"demo","dependencies":{"@nodelib/fs.walk":"1.2.8"}}"#,
    );
    sync(&solo(root), &store, &scoped_registry(), None, Mode::Develop).unwrap();

    let owner = root.join("node_modules/.jerky/@nodelib/fs.walk@1.2.8/node_modules");

    // An unscoped dependency of a scoped package, and a scoped one: the climb
    // differs again for the second, because the target is nested too.
    let fastq = std::fs::read_to_string(owner.join("fastq/package.json")).unwrap();
    assert!(fastq.contains("\"fastq\""), "got {fastq}");
    let types = std::fs::read_to_string(owner.join("@types/node/package.json")).unwrap();
    assert!(types.contains("@types/node"), "got {types}");

    assert_eq!(
        dangling_links(&root.join("node_modules")),
        Vec::<PathBuf>::new(),
        "the install left links that resolve to nothing"
    );
}

#[test]
fn a_scoped_package_installs_and_its_link_resolves() {
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));

    write_manifest(
        root,
        r#"{"name":"demo","dependencies":{"@types/node":"20.0.0"}}"#,
    );
    sync(&solo(root), &store, &scoped_registry(), None, Mode::Develop).unwrap();

    // Read through the link: proves the extra `../` the scope adds was
    // computed rather than assumed, since a target one level short resolves
    // nowhere.
    assert_eq!(linked_version(root, "@types/node"), "20.0.0");

    // The store nests, which is what `converge` and `prune_virtual_store` were
    // already written to expect.
    assert!(
        root.join("node_modules/.jerky/@types/node@20.0.0/node_modules/@types/node")
            .is_dir(),
        "the entry is not where the pruner looks for it"
    );
}

#[test]
fn a_scoped_dependency_of_a_package_is_linked_inside_the_store() {
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));

    write_manifest(
        root,
        r#"{"name":"demo","dependencies":{"uses-types":"1.0.0"}}"#,
    );
    sync(&solo(root), &store, &scoped_registry(), None, Mode::Develop).unwrap();

    let raw = std::fs::read_to_string(
        root.join("node_modules/.jerky/uses-types@1.0.0/node_modules/@types/node/package.json"),
    )
    .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(parsed["name"], "@types/node");
}

#[test]
fn a_nested_importer_reaches_a_scoped_package() {
    // Two climbs compound here: the importer's depth and the scope's extra
    // level. Both are derived from where the paths diverge rather than from a
    // count, which is the only reason this works at all.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));

    write_manifest(
        root,
        r#"{"name":"mono","private":true,"workspaces":["apps/*"]}"#,
    );
    write_manifest(
        &root.join("apps/web"),
        r#"{"name":"web","dependencies":{"@types/node":"20.0.0"}}"#,
    );
    sync(&solo(root), &store, &scoped_registry(), None, Mode::Develop).unwrap();

    assert_eq!(
        linked_version(&root.join("apps/web"), "@types/node"),
        "20.0.0"
    );
}

#[test]
fn dropping_a_scoped_package_removes_its_link_and_its_entry() {
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));
    let registry = scoped_registry();

    write_manifest(
        root,
        r#"{"name":"demo","dependencies":{"@types/node":"20.0.0","@types/react":"18.0.0"}}"#,
    );
    sync(&solo(root), &store, &registry, None, Mode::Develop).unwrap();

    // Drop one of the two. The scope survives because its sibling does.
    write_manifest(
        root,
        r#"{"name":"demo","dependencies":{"@types/react":"18.0.0"}}"#,
    );
    sync(&solo(root), &store, &registry, None, Mode::Develop).unwrap();

    assert!(!still_there(&root.join("node_modules/@types/node")));
    assert!(still_there(&root.join("node_modules/@types/react")));
    assert!(!still_there(
        &root.join("node_modules/.jerky/@types/node@20.0.0")
    ));
    assert!(still_there(
        &root.join("node_modules/.jerky/@types/react@18.0.0")
    ));

    // Drop the last one. Now the scope directory itself has nothing left to
    // hold, in the importer's `node_modules` and in the store.
    write_manifest(root, r#"{"name":"demo"}"#);
    sync(&solo(root), &store, &registry, None, Mode::Develop).unwrap();

    assert!(
        !still_there(&root.join("node_modules/@types")),
        "an empty scope directory outlived every package it was created for"
    );
    assert!(!still_there(&root.join("node_modules/.jerky/@types")));
}

#[test]
fn a_scoped_workspace_member_is_linked_in_place() {
    // The one link shape the registry-side tests cannot reach: a member is
    // linked straight at its own directory rather than into the store, and a
    // scoped member puts that link one level down like any other scoped name.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));

    write_manifest(
        root,
        r#"{"name":"mono","private":true,"workspaces":["packages/*"]}"#,
    );
    write_manifest(
        &root.join("packages/ui"),
        r#"{"name":"@myorg/ui","version":"1.0.0"}"#,
    );
    write_manifest(
        &root.join("packages/app"),
        r#"{"name":"app","dependencies":{"@myorg/ui":"workspace:*"}}"#,
    );
    sync(&solo(root), &store, &scoped_registry(), None, Mode::Develop).unwrap();

    // Read through: a target one level short resolves nowhere, and the member
    // is a real directory rather than a store entry, so the climb differs
    // again from the registry case.
    assert_eq!(
        linked_version(&root.join("packages/app"), "@myorg/ui"),
        "1.0.0"
    );
    assert_eq!(
        dangling_links(&root.join("packages")),
        Vec::<PathBuf>::new()
    );
}

#[test]
fn scope_directories_jerky_creates_are_not_world_writable() {
    // `create_dir_all` takes its mode from the umask, so under a permissive
    // one every scope level jerky creates would be writable by anyone — and a
    // scope directory another user can write into is one they can add a
    // package to, in the importer's tree and in the store that every project
    // on the machine hard-links from.
    use std::os::unix::fs::PermissionsExt as _;

    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();
    let store = Store::new(home.path().join("store"));

    write_manifest(
        root,
        r#"{"name":"demo","dependencies":{"@types/node":"20.0.0"}}"#,
    );
    sync(&solo(root), &store, &scoped_registry(), None, Mode::Develop).unwrap();

    for scope in [
        root.join("node_modules/@types"),
        root.join("node_modules/.jerky/@types"),
    ] {
        let mode = std::fs::metadata(&scope).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "{} is {mode:o}, not 0o755", scope.display());
    }
}
