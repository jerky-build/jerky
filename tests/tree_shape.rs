//! The shape of a linked tree, pinned byte for byte.
//!
//! Every other test in this suite asks whether one thing is true of the tree —
//! that a link resolves, that a mode is `0o755`, that a dropped dependency lost
//! its link. This one asks whether the *whole* tree is what it was, which is a
//! different question and the only one that can answer "did that refactor
//! change anything". It exists because #85 moved linking behind a plan and had
//! to show the resulting tree was unchanged; the fixture beside it was
//! generated from the code as it stood *before* that change and has been left
//! alone since.
//!
//! The rendering is everything a `node_modules` is: every path in sorted order,
//! what kind of thing it is, its mode, the digest of each file's bytes, each
//! symlink's literal target and whether it resolves — plus both sync outcomes
//! and the lockfile. Paths are workspace-relative and link targets are already
//! relative, so nothing in it depends on where the temp directory landed.
//!
//! The fixture is worth keeping past #85. #86 will materialise the virtual
//! store in parallel, whose whole obligation is to produce this same tree; a
//! reordering that drops a mode or writes a link one `../` short will show up
//! here as a one-line diff and nowhere else.
//!
//! **To regenerate**, after a change that is *meant* to alter the tree:
//!
//! ```sh
//! UPDATE_TREE_SHAPE=1 cargo test --test tree_shape
//! ```
//!
//! and read the diff before committing it. A regeneration that was not looked
//! at is worse than no fixture at all.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use jerky::commands::install::{Mode, Request, sync};
use jerky::store::Store;
use jerky::testing::FixtureRegistry;
use jerky::workspace::Workspace;

use std::os::unix::fs::PermissionsExt as _;
use tempfile::TempDir;

/// What the tree should render as. Generated from the pre-#85 linker.
const EXPECTED: &str = include_str!("fixtures/linked-tree.txt");

fn write_manifest(dir: &Path, json: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("package.json"), json).unwrap();
}

/// Create a directory the test itself owns, at an explicit `0o755`.
///
/// Explicit because the suite runs under `umask 0` as well as the default, and
/// a directory this test made with `create_dir_all` would be `0o777` under one
/// and `0o755` under the other — a difference in the *fixture* that would read
/// as a difference in jerky's own output. Nothing jerky writes needs this; it
/// sets its own modes, which is the property the rest of the rendering checks.
///
/// Every level, not only the last, for the same reason `directory::create_all`
/// exists: `create_dir_all` leaves the parents it fills in at whatever the
/// umask allowed.
fn debris_dir(path: &Path) {
    let mut level = PathBuf::new();
    for component in path.components() {
        level.push(component);
        if !level.exists() {
            std::fs::create_dir(&level).unwrap();
            std::fs::set_permissions(&level, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
}

fn debris_file(path: &Path, contents: &str) {
    std::fs::write(path, contents).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
}

/// Everything under `dir`, sorted, one line per entry.
fn render(root: &Path, dir: &Path, out: &mut String) {
    let mut paths: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(read) => read.map(|entry| entry.unwrap().path()).collect(),
        // A member that declares nothing and has never been installed into has
        // no `node_modules` at all, which is a fact about the tree worth
        // recording rather than a gap to skip over.
        Err(_) => {
            let _ = writeln!(
                out,
                "{}  <absent>",
                dir.strip_prefix(root).unwrap().display()
            );
            return;
        }
    };
    paths.sort();

    for path in paths {
        let rel = path.strip_prefix(root).unwrap().display().to_string();
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        let kind = metadata.file_type();
        let mode = metadata.permissions().mode();

        if kind.is_symlink() {
            // The literal target, not where it lands: the climb is the thing
            // an importer's depth decides, and a link that resolves correctly
            // by accident of an extra `..` and a symlinked temp directory
            // would pass a resolution check and fail this one.
            let target = std::fs::read_link(&path).unwrap();
            let _ = writeln!(out, "link {rel} -> {}", target.display());
            let _ = writeln!(
                out,
                "     resolves={} is_dir={}",
                path.exists(),
                path.is_dir()
            );
        } else if kind.is_dir() {
            let _ = writeln!(out, "dir  {rel} {mode:o}");
            render(root, &path, out);
        } else {
            let digest = <sha2::Sha256 as sha2::Digest>::digest(std::fs::read(&path).unwrap());
            let _ = writeln!(out, "file {rel} {mode:o} {}", hex(&digest));
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A registry with depth, sharing, scopes and an alias target — the shapes
/// that have each broken a link target at least once.
fn registry() -> FixtureRegistry {
    FixtureRegistry::new()
        .with_tree(&[
            ("alpha", "1.0.0", &[("beta", "^1.0.0"), ("gamma", "^2.0.0")]),
            ("beta", "1.4.2", &[("gamma", "^2.0.0")]),
            // Two versions, so the prune has a real choice to make.
            ("gamma", "2.1.0", &[]),
            ("gamma", "1.0.0", &[]),
            ("delta", "3.0.0", &[("gamma", "^1.0.0")]),
            ("string-width", "4.2.3", &[]),
        ])
        .with_packument("@types/node", &[("20.0.0", &[])])
        .with_packument("@types/react", &[("18.0.0", &[])])
        // A scoped owner with a scoped and an unscoped dependency: the climb
        // out of its entry differs from an unscoped owner's by one level.
        .with_packument(
            "@nodelib/fs.walk",
            &[("1.2.8", &[("alpha", "^1.0.0"), ("@types/node", "^20.0.0")])],
        )
        .with_packument("uses-types", &[("1.0.0", &[("@types/node", "^20.0.0")])])
}

/// The first line the two disagree on, which is what a reader actually needs.
///
/// `assert_eq!` on two hundred lines of tree prints both in full and leaves the
/// difference to be found by eye.
fn first_difference(actual: &str, expected: &str) -> Option<String> {
    let mut actual = actual.lines();
    let mut expected = expected.lines();

    for line in 1.. {
        match (actual.next(), expected.next()) {
            (None, None) => return None,
            (Some(got), Some(want)) if got == want => continue,
            (Some(got), Some(want)) => {
                return Some(format!(
                    "line {line}:\n  expected: {want}\n  actual:   {got}"
                ));
            }
            (Some(got), None) => return Some(format!("line {line}: unexpected extra line {got}")),
            (None, Some(want)) => return Some(format!("line {line}: missing line {want}")),
        }
    }

    unreachable!("the loop returns on the first disagreement or on running out of both")
}

#[test]
fn a_linked_workspace_has_the_shape_it_has_always_had() {
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    // Canonical, because `discover` canonicalizes its root and the paths that
    // come back out of `sync` are therefore canonical too. On macOS the temp
    // directory lands under `/var`, a symlink to `/private/var`, so the
    // uncanonicalized path is not a prefix of them and rendering an `Unowned`
    // relative to it fails outright. Taken once here so that every path in the
    // rendering — the ones this test builds and the ones jerky hands back —
    // is measured from the same base.
    let root = &work.path().canonicalize().unwrap();
    let store = Store::new(home.path().join("store"));

    // Four importers, because a suite that only ever sees `.` is not testing
    // workspaces — and one of the four declares nothing, which is the importer
    // convergence still has to visit.
    write_manifest(
        root,
        r#"{"name":"ws","workspaces":["packages/*","apps/*"],
            "dependencies":{"alpha":"1.0.0","@types/react":"18.0.0"},
            "devDependencies":{"delta":"3.0.0"}}"#,
    );
    write_manifest(
        &root.join("packages/ui"),
        r#"{"name":"ui","dependencies":{"@nodelib/fs.walk":"1.2.8","uses-types":"1.0.0",
            "width-cjs":"npm:string-width@^4.0.0"}}"#,
    );
    write_manifest(&root.join("packages/empty"), r#"{"name":"empty"}"#);
    write_manifest(
        &root.join("apps/web"),
        r#"{"name":"web","dependencies":{"ui":"workspace:*","beta":"1.4.2"}}"#,
    );

    // What a repository half-migrated from npm looks like, for convergence to
    // judge and decline: a real directory, a link pointing somewhere else
    // entirely, and an empty scope directory that was already empty.
    let web_modules = root.join("apps/web/node_modules");
    debris_dir(&web_modules.join("legacy/lib"));
    debris_file(&web_modules.join("legacy/lib/index.js"), "old");
    std::os::unix::fs::symlink("/nowhere/at/all", web_modules.join("elsewhere")).unwrap();
    debris_dir(&web_modules.join("@empty"));

    let first = sync(
        &Workspace::discover(root).unwrap(),
        &store,
        &registry(),
        None::<&Request>,
        Mode::Develop,
    )
    .unwrap();

    let mut rendered = String::new();
    let _ = writeln!(rendered, "--- first sync ---");
    render_outcome(&first, root, &mut rendered);
    render_tree(root, &mut rendered);

    // Drop dependencies from two manifests, so the second sync has links to
    // converge away and store entries to prune rather than only work to add.
    write_manifest(
        root,
        r#"{"name":"ws","workspaces":["packages/*","apps/*"],
            "dependencies":{"alpha":"1.0.0"}}"#,
    );
    write_manifest(
        &root.join("packages/ui"),
        r#"{"name":"ui","dependencies":{"@nodelib/fs.walk":"1.2.8"}}"#,
    );

    let second = sync(
        &Workspace::discover(root).unwrap(),
        &store,
        &registry(),
        None::<&Request>,
        Mode::Develop,
    )
    .unwrap();

    let _ = writeln!(rendered, "--- second sync ---");
    render_outcome(&second, root, &mut rendered);
    render_tree(root, &mut rendered);

    let _ = writeln!(rendered, "--- lockfile ---");
    rendered.push_str(&std::fs::read_to_string(root.join("jerky-lock.json")).unwrap());

    if std::env::var_os("UPDATE_TREE_SHAPE").is_some() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/linked-tree.txt");
        std::fs::write(&fixture, &rendered).unwrap();
        panic!(
            "rewrote {} — read the diff, then run the suite again without \
             UPDATE_TREE_SHAPE",
            fixture.display()
        );
    }

    if let Some(difference) = first_difference(&rendered, EXPECTED) {
        panic!(
            "the linked tree is not the shape it was.\n\n{difference}\n\n\
             If the change was intended, regenerate with \
             `UPDATE_TREE_SHAPE=1 cargo test --test tree_shape` and read the diff."
        );
    }
}

fn render_outcome(outcome: &jerky::commands::install::Outcome, root: &Path, out: &mut String) {
    for installed in &outcome.linked {
        let _ = writeln!(out, "linked {}@{}", installed.name, installed.version);
    }
    // Sorted: which order convergence reports two importers' findings in is
    // not something this fixture should pin.
    let reported: BTreeSet<String> = outcome
        .left_alone
        .iter()
        .map(|unowned| {
            format!(
                "left {} {:?}",
                unowned.path.strip_prefix(root).unwrap().display(),
                unowned.reason
            )
        })
        .collect();
    for line in reported {
        let _ = writeln!(out, "{line}");
    }
}

fn render_tree(root: &Path, out: &mut String) {
    render(root, &root.join("node_modules"), out);
    for member in ["packages/ui", "packages/empty", "apps/web"] {
        render(root, &root.join(member).join("node_modules"), out);
    }
}
