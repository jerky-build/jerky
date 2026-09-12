//! Workspace discovery tests.
//!
//! Membership comes from the root `package.json`'s `workspaces` field, which
//! is what npm and yarn read — an existing monorepo is a jerky workspace with
//! no new file. These tests pin the consequences of that decision: the root is
//! always a member, absence of the field is a workspace of one rather than a
//! separate code path, and a set of directories is refused when two of them
//! claim the same package name.

use std::path::Path;

use jerky::resolver::ImporterPath;
use jerky::workspace::{Warning, Workspace, WorkspaceError};
use tempfile::TempDir;

fn write_manifest(dir: &Path, contents: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("package.json"), contents).unwrap();
}

fn importer(raw: &str) -> ImporterPath {
    ImporterPath::new(raw).unwrap()
}

fn keys(ws: &Workspace) -> Vec<String> {
    ws.members().keys().map(|k| k.to_string()).collect()
}

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
    // Keyed workspace-relative and sorted, because the lockfile's importers
    // map is keyed the same way and must serialize deterministically.
    let dir = TempDir::new().unwrap();
    write_manifest(dir.path(), r#"{"name":"root","workspaces":["packages/*"]}"#);
    write_manifest(&dir.path().join("packages/c"), r#"{"name":"c"}"#);
    write_manifest(&dir.path().join("packages/a"), r#"{"name":"a"}"#);
    write_manifest(&dir.path().join("packages/b"), r#"{"name":"b"}"#);

    let ws = Workspace::discover(dir.path()).unwrap();

    assert_eq!(
        keys(&ws),
        vec![".", "packages/a", "packages/b", "packages/c"],
        "the root is a member too, and iteration order is structural"
    );
}

#[test]
fn a_single_star_does_not_cross_a_directory_boundary() {
    // `packages/*` names the children of `packages`, not their descendants.
    // globset matches `*` across separators unless told otherwise, which would
    // silently make every nested directory a member.
    let dir = TempDir::new().unwrap();
    write_manifest(dir.path(), r#"{"name":"root","workspaces":["packages/*"]}"#);
    write_manifest(&dir.path().join("packages/ui"), r#"{"name":"ui"}"#);
    write_manifest(
        &dir.path().join("packages/ui/nested"),
        r#"{"name":"nested"}"#,
    );

    let ws = Workspace::discover(dir.path()).unwrap();

    assert_eq!(keys(&ws), vec![".", "packages/ui"]);
}

#[test]
fn a_match_without_a_manifest_is_skipped_not_fatal() {
    // `packages/.cache` exists and has no package.json. An install must not
    // fail because of a stray directory.
    let dir = TempDir::new().unwrap();
    write_manifest(dir.path(), r#"{"name":"root","workspaces":["packages/*"]}"#);
    write_manifest(&dir.path().join("packages/ui"), r#"{"name":"ui"}"#);
    std::fs::create_dir_all(dir.path().join("packages/.cache")).unwrap();

    let ws = Workspace::discover(dir.path()).unwrap();

    assert_eq!(keys(&ws), vec![".", "packages/ui"]);
}

#[test]
fn a_pattern_matching_nothing_warns() {
    // Usually a typo. Worth surfacing, not worth failing.
    let dir = TempDir::new().unwrap();
    write_manifest(
        dir.path(),
        r#"{"name":"root","workspaces":["packages/*","aps/*"]}"#,
    );
    write_manifest(&dir.path().join("packages/ui"), r#"{"name":"ui"}"#);

    let ws = Workspace::discover(dir.path()).unwrap();

    assert_eq!(keys(&ws), vec![".", "packages/ui"]);
    assert_eq!(
        ws.warnings(),
        &[Warning::PatternMatchedNothing("aps/*".to_string())],
        "the mistyped pattern is named, and the good one is not"
    );
}

#[test]
fn two_members_sharing_a_name_are_refused_naming_both_paths() {
    // Membership is a set of directories, so a name collision is found by
    // looking at paths — which is what the message must carry.
    let dir = TempDir::new().unwrap();
    write_manifest(
        dir.path(),
        r#"{"name":"root","workspaces":["packages/*","apps/*"]}"#,
    );
    write_manifest(&dir.path().join("packages/ui"), r#"{"name":"ui"}"#);
    write_manifest(&dir.path().join("apps/ui"), r#"{"name":"ui"}"#);

    // The paths themselves are asserted, not merely that they differ: the
    // whole point of the message is that a name collision is found by looking
    // at directories, so a message that named neither would still satisfy
    // `first != second`.
    let Err(WorkspaceError::DuplicateName {
        name,
        first,
        second,
    }) = Workspace::discover(dir.path())
    else {
        panic!("two members named `ui` must be refused");
    };
    assert_eq!(name, "ui");
    assert_eq!(
        (first.as_path(), second.as_path()),
        (Path::new("apps/ui"), Path::new("packages/ui")),
        "both colliding directories are named, workspace-relative"
    );
}

#[test]
fn a_symlinked_directory_is_not_a_member() {
    // A pattern may not name a directory outside the workspace, and following
    // a link would be a second door to the same place — the tar-slip shape
    // `archive` already guards against.
    let dir = TempDir::new().unwrap();
    let outside = dir.path().join("outside");
    write_manifest(&outside, r#"{"name":"smuggled"}"#);

    let root = dir.path().join("repo");
    write_manifest(&root, r#"{"name":"root","workspaces":["packages/*"]}"#);
    std::fs::create_dir_all(root.join("packages")).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("packages/ui")).unwrap();

    let ws = Workspace::discover(&root).unwrap();

    assert_eq!(keys(&ws), vec!["."]);
    assert_eq!(
        ws.warnings(),
        &[Warning::PatternMatchedNothing("packages/*".to_string())],
        "the pattern found nothing, and the user hears about it"
    );
}

#[test]
fn a_pattern_escaping_the_workspace_root_is_refused() {
    // npm permits `../siblings/*`. Silently including a directory outside the
    // repo has the same shape as the tar-slip class `archive` already guards
    // against, so it is refused rather than resolved.
    let dir = TempDir::new().unwrap();
    write_manifest(
        dir.path(),
        r#"{"name":"root","workspaces":["../outside/*"]}"#,
    );

    assert!(matches!(
        Workspace::discover(dir.path()),
        Err(WorkspaceError::EscapesRoot { .. })
    ));
}

#[test]
fn member_for_finds_the_nearest_enclosing_member() {
    // Standing in packages/ui/src/deep means the packages/ui importer.
    let dir = TempDir::new().unwrap();
    write_manifest(dir.path(), r#"{"name":"root","workspaces":["packages/*"]}"#);
    write_manifest(&dir.path().join("packages/ui"), r#"{"name":"ui"}"#);
    let deep = dir.path().join("packages/ui/src/deep");
    std::fs::create_dir_all(&deep).unwrap();

    let ws = Workspace::discover(dir.path()).unwrap();

    assert_eq!(
        ws.member_for(&deep).unwrap().importer,
        importer("packages/ui")
    );
    // A directory inside the workspace but inside no other member belongs to
    // the root, which is an ordinary member rather than a fallback.
    let scratch = dir.path().join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    assert_eq!(
        ws.member_for(&scratch).unwrap().importer,
        ImporterPath::root()
    );
}

#[test]
fn member_for_returns_none_outside_every_member() {
    // Ambiguous rather than obviously the root, so the caller decides.
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("repo");
    write_manifest(&root, r#"{"name":"root"}"#);
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();

    let ws = Workspace::discover(&root).unwrap();

    assert!(ws.member_for(&elsewhere).is_none());
}

#[test]
fn a_private_package_is_an_ordinary_member() {
    // Publishability is a property of a package, not of membership.
    let dir = TempDir::new().unwrap();
    write_manifest(dir.path(), r#"{"name":"root","workspaces":["packages/*"]}"#);
    write_manifest(
        &dir.path().join("packages/ui"),
        r#"{"name":"ui","private":true}"#,
    );

    let ws = Workspace::discover(dir.path()).unwrap();

    assert!(ws.members().contains_key(&importer("packages/ui")));
}

#[test]
fn find_root_walks_up_to_the_nearest_workspace_manifest() {
    // What main.rs uses, given a cwd.
    let dir = TempDir::new().unwrap();
    write_manifest(dir.path(), r#"{"name":"root","workspaces":["packages/*"]}"#);
    write_manifest(&dir.path().join("packages/ui"), r#"{"name":"ui"}"#);
    let deep = dir.path().join("packages/ui/src/deep");
    std::fs::create_dir_all(&deep).unwrap();

    let found = Workspace::find_root(&deep).unwrap();

    assert_eq!(
        found.canonicalize().unwrap(),
        dir.path().canonicalize().unwrap(),
        "a member's own package.json has no workspaces field, so the walk continues past it"
    );
}

#[test]
fn find_root_does_not_adopt_a_project_the_workspace_does_not_claim() {
    // A standalone project that merely sits underneath a monorepo is not a
    // member of it. Stopping at the first ancestor declaring `workspaces`
    // would install this project's dependencies into the monorepo instead.
    let dir = TempDir::new().unwrap();
    write_manifest(dir.path(), r#"{"name":"mono","workspaces":["packages/*"]}"#);
    write_manifest(&dir.path().join("packages/ui"), r#"{"name":"ui"}"#);
    let standalone = dir.path().join("unrelated/standalone");
    write_manifest(&standalone, r#"{"name":"standalone"}"#);

    let found = Workspace::find_root(&standalone).unwrap();

    assert_eq!(
        found.canonicalize().unwrap(),
        standalone.canonicalize().unwrap()
    );
}
