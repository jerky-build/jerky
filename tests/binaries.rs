//! `node_modules/.bin`: the shims that make a locally-installed tool runnable.
//!
//! Each test is a claim from `docs/specs/2026-09-16-bin-linking-design.md`
//! rather than coverage for its own sake, driven through a real `sync` against
//! real tarballs — the layer where a mistake in any one of the packument, the
//! lockfile, the plan and the linker shows up as a tool that will not run.
//!
//! The one that matters most is
//! `an_installed_tool_actually_runs`. Everything else here asserts that a link
//! is where it should be; that one asserts the point of having it.

use std::os::unix::fs::MetadataExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::Command;

use jerky::commands::install::{Mode, sync};
use jerky::store::Store;
use jerky::testing::{FixtureRegistry, TarEntry, build_tarball, mode_of};
use jerky::workspace::Workspace;

use tempfile::TempDir;

/// A package whose bin ships **without** the executable bit, which is what
/// npm packages routinely do and the case §3 is about.
fn tool_tarball(name: &str, script: &str) -> Vec<u8> {
    build_tarball(&[
        TarEntry::file(
            "package/package.json",
            &format!(r#"{{"name":"{name}","version":"1.0.0"}}"#),
        ),
        TarEntry::file_with_mode("package/bin/cli.js", script, 0o644),
    ])
}

/// One file with this name anywhere under `dir`, with its metadata.
///
/// The content store keys entries by integrity, so a test that wants the file
/// the virtual store is hard-linked from has to find it rather than spell it.
struct Found {
    path: std::path::PathBuf,
    metadata: std::fs::Metadata,
}

impl Found {
    fn ino(&self) -> u64 {
        self.metadata.ino()
    }
}

fn find(dir: &Path, name: &str) -> Option<Found> {
    for entry in std::fs::read_dir(dir).ok()? {
        let path = entry.ok()?.path();
        if path.is_dir() {
            if let Some(found) = find(&path, name) {
                return Some(found);
            }
        } else if path.file_name().is_some_and(|found| found == name) {
            let metadata = path.metadata().ok()?;
            return Some(Found { path, metadata });
        }
    }

    None
}

fn project(dir: &Path, manifest: &str) -> Workspace {
    std::fs::write(dir.join("package.json"), manifest).unwrap();
    Workspace::discover(dir).unwrap()
}

/// A registry serving one tool that publishes `cli` at `bin/cli.js`.
fn serving(name: &str, script: &str) -> FixtureRegistry {
    FixtureRegistry::new()
        .with_package(name, "1.0.0", tool_tarball(name, script))
        .with_bins(name, "1.0.0", &[(name, "bin/cli.js")])
}

#[test]
fn an_installed_tool_actually_runs() {
    // The test this whole issue exists for, and the only one that proves the
    // feature rather than its parts: install a package whose bin ships at
    // 0o644, then execute `node_modules/.bin/tool` and read what it printed.
    // A shim in the right place pointing at a file with no execute bit passes
    // every other assertion in this file and fails this one.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let workspace = project(
        work.path(),
        r#"{"name":"demo","dependencies":{"tool":"^1.0.0"}}"#,
    );
    let store = Store::new(home.path().join("store"));

    sync(
        &workspace,
        &store,
        &serving("tool", "#!/bin/sh\necho 'ran the tool'\n"),
        None,
        Mode::Develop,
    )
    .unwrap();

    let shim = work.path().join("node_modules/.bin/tool");
    let output = Command::new(&shim)
        .output()
        .unwrap_or_else(|err| panic!("{} could not be executed: {err}", shim.display()));

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "ran the tool\n");
}

#[test]
fn the_bin_target_and_its_store_entry_are_both_made_executable() {
    // The consequence the research note had to argue for before the chmod was
    // allowed: the virtual store is hard links, so raising the bit on a bin
    // raises it on the machine-global store entry every project shares.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let workspace = project(
        work.path(),
        r#"{"name":"demo","dependencies":{"tool":"^1.0.0"}}"#,
    );
    let store = Store::new(home.path().join("store"));

    sync(
        &workspace,
        &store,
        &serving("tool", "#!/bin/sh\ntrue\n"),
        None,
        Mode::Develop,
    )
    .unwrap();

    let linked = work
        .path()
        .join("node_modules/.jerky/tool@1.0.0/node_modules/tool/bin/cli.js");
    assert_eq!(
        mode_of(&linked),
        0o755,
        "the bin target was left unrunnable"
    );

    // Found by walking rather than derived, so that the assertion is about the
    // file the content store actually holds and not about a path this test
    // believes it holds.
    let in_store = find(&home.path().join("store"), "cli.js")
        .expect("the content store holds the package's bin file");
    assert_eq!(
        (in_store.ino(), mode_of(&in_store.path)),
        (linked.metadata().unwrap().ino(), 0o755),
        "the store entry the link shares an inode with disagrees with it"
    );
}

#[test]
fn a_shim_survives_the_install_that_resolves_nothing() {
    // The cache-hit hole §1 records `bin` in the lockfile to close. An install
    // whose importers all still match resolves nothing at all and builds its
    // graph out of the lockfile alone — so a file that did not name each
    // package's bins would write no shims on the most common install there is.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let workspace = project(
        work.path(),
        r#"{"name":"demo","dependencies":{"tool":"^1.0.0"}}"#,
    );
    let store = Store::new(home.path().join("store"));
    let registry = serving("tool", "#!/bin/sh\ntrue\n");

    sync(&workspace, &store, &registry, None, Mode::Develop).unwrap();

    // Taken away, so that the second install can only put it back from what
    // the lockfile recorded.
    std::fs::remove_dir_all(work.path().join("node_modules/.bin")).unwrap();

    let before = registry.metadata_calls() + registry.packument_calls();
    sync(&workspace, &store, &registry, None, Mode::Develop).unwrap();
    assert_eq!(
        registry.metadata_calls() + registry.packument_calls(),
        before,
        "the second install resolved, so this proves nothing about a cache hit"
    );

    assert!(
        work.path().join("node_modules/.bin/tool").is_file(),
        "the shim did not survive an install that read only the lockfile"
    );
}

#[test]
fn a_workspace_member_publishes_its_bin_to_the_members_that_depend_on_it() {
    // A monorepo whose `packages/cli` cannot be called from `apps/web` is
    // missing the case monorepos exist for. A member has no tarball and no
    // packument, so its `bin` comes off its own manifest — the local half of
    // what a packument answers for a registry package.
    //
    // More than one importer, which the standing invariant asks of at least
    // one test and which a single-importer suite cannot see at all.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();

    std::fs::create_dir_all(root.join("packages/cli/bin")).unwrap();
    std::fs::create_dir_all(root.join("apps/web")).unwrap();
    std::fs::write(
        root.join("packages/cli/package.json"),
        r#"{"name":"cli","version":"1.0.0","bin":{"cli":"bin/cli.js"}}"#,
    )
    .unwrap();
    let member_bin = root.join("packages/cli/bin/cli.js");
    std::fs::write(&member_bin, "console.log('local')").unwrap();
    // Pinned rather than left to the umask, because the assertion at the end
    // is that jerky did *not* change this mode — and a fixture that starts at
    // whatever the umask allows is asserting about the umask instead. The
    // suite runs under `umask 0` as well as the default.
    std::fs::set_permissions(&member_bin, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::write(
        root.join("apps/web/package.json"),
        r#"{"name":"web","dependencies":{"cli":"workspace:*"}}"#,
    )
    .unwrap();

    let workspace = project(
        root,
        r#"{"name":"root","workspaces":["packages/*","apps/*"]}"#,
    );
    let store = Store::new(home.path().join("store"));

    sync(
        &workspace,
        &store,
        &FixtureRegistry::new(),
        None,
        Mode::Develop,
    )
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(root.join("apps/web/node_modules/.bin/cli")).unwrap(),
        "console.log('local')",
        "a member's bin was not linked into the member that depends on it"
    );

    // Never chmodded: it is a file in the user's repository under version
    // control, where a raised execute bit is a change git reports and nobody
    // else shares the inode to justify it.
    assert_eq!(mode_of(&member_bin), 0o644);
}

#[test]
fn dropping_a_dependency_takes_its_shim_with_it() {
    // "Nothing stays on disk that is no longer recorded", applied to a shim. A
    // `tool` that still runs after the manifest stopped declaring it is the
    // tree disagreeing with the manifest, which is the drift convergence
    // exists to prevent.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let workspace = project(
        work.path(),
        r#"{"name":"demo","dependencies":{"tool":"^1.0.0"}}"#,
    );
    let store = Store::new(home.path().join("store"));
    let registry = serving("tool", "#!/bin/sh\ntrue\n");

    sync(&workspace, &store, &registry, None, Mode::Develop).unwrap();
    assert!(work.path().join("node_modules/.bin/tool").is_file());

    let workspace = project(work.path(), r#"{"name":"demo"}"#);
    let outcome = sync(&workspace, &store, &registry, None, Mode::Develop).unwrap();

    assert!(outcome.left_alone.is_empty(), "{:?}", outcome.left_alone);
    assert!(
        !work.path().join("node_modules/.bin").exists(),
        "the shim, and the `.bin` jerky emptied, outlived the dependency"
    );
}

#[test]
fn a_bin_a_package_may_not_spell_is_dropped_without_failing_the_install() {
    // `bin` is published by whoever published the package and both halves of
    // every entry become a path. A name that is not one path component escapes
    // `.bin`; a target that climbs out of the package names a file the package
    // does not own — and then jerky would raise the execute bit on it.
    //
    // Dropped rather than fatal, and the rest of the package's bins survive:
    // one bad entry must not make an otherwise-installable package refuse.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let workspace = project(
        work.path(),
        r#"{"name":"demo","dependencies":{"tool":"^1.0.0"}}"#,
    );
    let store = Store::new(home.path().join("store"));

    let registry = FixtureRegistry::new()
        .with_package("tool", "1.0.0", tool_tarball("tool", "#!/bin/sh\ntrue\n"))
        .with_bins(
            "tool",
            "1.0.0",
            &[
                ("../escape", "bin/cli.js"),
                ("climber", "../../../../etc/cron.daily/x"),
                ("tool", "bin/cli.js"),
            ],
        );

    sync(&workspace, &store, &registry, None, Mode::Develop).unwrap();

    // `symlink_metadata`, never `exists`. Both of the links this is looking
    // for would be *dangling* — one aimed at a file outside the package, the
    // other written relative to a directory it was then placed outside of —
    // and `exists` follows a link and reports false for a dangling one. A
    // check written that way passes with the validation deleted, which is how
    // this was found.
    let bin = work.path().join("node_modules/.bin");
    assert!(
        bin.join("tool").is_file(),
        "the valid bin was taken down too"
    );
    assert!(
        std::fs::symlink_metadata(bin.parent().unwrap().join("escape")).is_err(),
        "a bin name climbed out of `.bin`"
    );
    assert!(
        std::fs::symlink_metadata(bin.join("climber")).is_err(),
        "a bin target pointing outside the package was linked"
    );
}

#[test]
fn a_shim_npm_wrote_is_left_alone_and_reported() {
    // The first `jerky install` in a repository that has seen npm must not be
    // a destructive surprise. npm's shims live in the same directory and point
    // into `node_modules/<pkg>`, which is inside the importer — so what makes
    // this provable is that jerky's own never route through a `node_modules`.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();

    std::fs::create_dir_all(root.join("node_modules/other/bin")).unwrap();
    std::fs::create_dir_all(root.join("node_modules/.bin")).unwrap();
    std::fs::write(root.join("node_modules/other/bin/x.js"), "npm's").unwrap();
    std::os::unix::fs::symlink("../other/bin/x.js", root.join("node_modules/.bin/other")).unwrap();

    let workspace = project(root, r#"{"name":"demo","dependencies":{"tool":"^1.0.0"}}"#);
    let store = Store::new(home.path().join("store"));

    let outcome = sync(
        &workspace,
        &store,
        &serving("tool", "#!/bin/sh\ntrue\n"),
        None,
        Mode::Develop,
    )
    .unwrap();

    assert!(
        std::fs::symlink_metadata(root.join("node_modules/.bin/other")).is_ok(),
        "jerky deleted a shim another tool wrote"
    );
    // Compared against the workspace's own root, not the temp directory it was
    // built in: `discover` canonicalizes, and on macOS `/var` is a symlink to
    // `/private/var`, so the two spell the same directory differently.
    assert!(
        outcome
            .left_alone
            .iter()
            .any(|entry| entry.path == workspace.root().join("node_modules/.bin/other")),
        "the shim jerky kept was not reported: {:?}",
        outcome.left_alone
    );

    // And jerky's own landed beside it rather than instead of it.
    assert!(root.join("node_modules/.bin/tool").is_file());
}

#[test]
fn two_dependencies_publishing_one_name_pick_the_same_winner_every_run() {
    // A `.bin` is a flat namespace, so one of them loses. npm's answer is
    // whichever link was written last; jerky's is the alphabetically first
    // dependency, which makes the tree a function of the graph rather than of
    // iteration luck — and says which one it gave the name to.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let workspace = project(
        work.path(),
        r#"{"name":"demo","dependencies":{"alpha":"^1.0.0","zeta":"^1.0.0"}}"#,
    );
    let store = Store::new(home.path().join("store"));

    let registry = FixtureRegistry::new()
        .with_package(
            "alpha",
            "1.0.0",
            tool_tarball("alpha", "#!/bin/sh\necho a\n"),
        )
        .with_bins("alpha", "1.0.0", &[("fmt", "bin/cli.js")])
        .with_package("zeta", "1.0.0", tool_tarball("zeta", "#!/bin/sh\necho z\n"))
        .with_bins("zeta", "1.0.0", &[("fmt", "bin/cli.js")]);

    let outcome = sync(&workspace, &store, &registry, None, Mode::Develop).unwrap();

    let ran = Command::new(work.path().join("node_modules/.bin/fmt"))
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&ran.stdout),
        "a\n",
        "the contested name did not go to the alphabetically first dependency"
    );

    let collision = &outcome.bin_collisions[0];
    assert_eq!(
        (
            collision.name.as_str(),
            collision.winner.as_str(),
            collision.loser.as_str()
        ),
        ("fmt", "alpha", "zeta"),
        "the loser was overwritten without saying so"
    );
}

#[test]
fn a_real_file_where_a_shim_must_go_stops_the_install_and_names_it() {
    // yarn classic writes shell *scripts* into `.bin` where npm and jerky
    // write symlinks, so this is the first `jerky install` in a repository
    // migrated from yarn. jerky will not overwrite a file it cannot prove it
    // wrote: clobbering silently deletes someone's work, and skipping the bin
    // would leave the project without a tool it declared while reporting
    // success. Refusing names the file and leaves the remedy to whoever knows
    // whether it mattered.
    //
    // The asymmetry with the test above is the same rule from the other side:
    // convergence leaves alone what the plan does not name, and this refuses
    // to overwrite what the plan does.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();

    std::fs::create_dir_all(root.join("node_modules/.bin")).unwrap();
    std::fs::write(
        root.join("node_modules/.bin/tool"),
        "#!/bin/sh\n# yarn's shim\n",
    )
    .unwrap();

    let workspace = project(root, r#"{"name":"demo","dependencies":{"tool":"^1.0.0"}}"#);
    let store = Store::new(home.path().join("store"));

    let failed = sync(
        &workspace,
        &store,
        &serving("tool", "#!/bin/sh\ntrue\n"),
        None,
        Mode::Develop,
    )
    .expect_err("a real file where a shim must go was overwritten");

    let said = failed.to_string();
    assert!(
        said.contains(".bin/tool") && said.contains("not a symlink"),
        "the refusal did not name the file or say why: {said}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("node_modules/.bin/tool")).unwrap(),
        "#!/bin/sh\n# yarn's shim\n",
        "the file jerky refused to replace was modified anyway"
    );
}

#[test]
fn a_hand_edited_lockfile_cannot_spell_a_bin_a_packument_could_not() {
    // §4's validation has to hold on the lockfile path, not only the packument
    // one — and §1 makes the lockfile the *primary* source of bins rather than
    // a secondary one: "an install whose importers all still match resolves
    // nothing and builds its graph out of the lockfile alone". This file is
    // checked in and arrives through pull requests, so it is exactly as
    // untrusted as a registry response.
    //
    // Trusted on load, the first entry plants a link outside `.bin` and the
    // second aims one at a file the package does not own — which jerky then
    // raises the execute bit on.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let workspace = project(
        work.path(),
        r#"{"name":"demo","dependencies":{"tool":"^1.0.0"}}"#,
    );
    let store = Store::new(home.path().join("store"));
    let registry = serving("tool", "#!/bin/sh\ntrue\n");

    sync(&workspace, &store, &registry, None, Mode::Develop).unwrap();

    let planted = work.path().join("planted.sh");
    std::fs::write(&planted, "#!/bin/sh\necho planted\n").unwrap();
    std::fs::set_permissions(&planted, std::fs::Permissions::from_mode(0o644)).unwrap();

    // Hand-edit the lockfile the way a hostile pull request would.
    let path = work.path().join("jerky-lock.json");
    let raw = std::fs::read_to_string(&path).unwrap();
    let edited = raw.replace(
        r#""bin": {"#,
        &format!(
            r#""bin": {{ "../../../escaped": "bin/cli.js", "planted": "{}", "#,
            "../".repeat(8) + planted.strip_prefix("/").unwrap().to_str().unwrap()
        ),
    );
    assert_ne!(
        raw, edited,
        "the lockfile did not record a `bin` block to edit"
    );
    std::fs::write(&path, edited).unwrap();

    std::fs::remove_dir_all(work.path().join("node_modules")).unwrap();
    sync(&workspace, &store, &registry, None, Mode::Develop).unwrap();

    // `symlink_metadata`, never `exists`: both of these would dangle, and
    // `exists` follows a link and reports false for a dangling one.
    assert!(
        std::fs::symlink_metadata(work.path().join("node_modules/escaped")).is_err(),
        "a lockfile bin name climbed out of `.bin`"
    );
    assert!(
        std::fs::symlink_metadata(work.path().join("node_modules/.bin/planted")).is_err(),
        "a lockfile bin target pointing outside the package was linked"
    );
    assert_eq!(
        mode_of(&planted),
        0o644,
        "a lockfile made jerky raise the execute bit on a file outside the package"
    );

    // And the entry that is spelled properly still installs.
    assert!(work.path().join("node_modules/.bin/tool").is_file());
}

#[test]
fn a_hand_written_shim_pointing_into_the_repository_is_left_alone() {
    // The workspace root is itself a member, so an ownership test phrased as
    // "inside a member" reduces to "anywhere in the repository" — and would
    // delete a `.bin/lint -> ../../scripts/lint.sh` that an install never
    // wrote. Excluding paths that pass through a `node_modules` narrows that
    // and does not fix it: `scripts/lint.sh` passes through none.
    //
    // "Convergence removes only what it can prove jerky wrote", and the proof
    // is the finite set of files the members actually publish as bins.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();

    std::fs::create_dir_all(root.join("scripts")).unwrap();
    std::fs::create_dir_all(root.join("node_modules/.bin")).unwrap();
    std::fs::write(root.join("scripts/lint.sh"), "#!/bin/sh\necho lint\n").unwrap();
    std::os::unix::fs::symlink("../../scripts/lint.sh", root.join("node_modules/.bin/lint"))
        .unwrap();

    let workspace = project(root, r#"{"name":"demo","dependencies":{"tool":"^1.0.0"}}"#);
    let store = Store::new(home.path().join("store"));

    let outcome = sync(
        &workspace,
        &store,
        &serving("tool", "#!/bin/sh\ntrue\n"),
        None,
        Mode::Develop,
    )
    .unwrap();

    assert!(
        std::fs::symlink_metadata(root.join("node_modules/.bin/lint")).is_ok(),
        "an install deleted a shim somebody wrote by hand"
    );
    // The workspace's own root, for the reason the test above gives: `discover`
    // canonicalizes and macOS resolves `/var` to `/private/var`.
    assert!(
        outcome
            .left_alone
            .iter()
            .any(|entry| entry.path == workspace.root().join("node_modules/.bin/lint")),
        "the shim jerky kept was not reported: {:?}",
        outcome.left_alone
    );
}

#[test]
fn a_dropped_member_dependency_still_loses_its_shim() {
    // The other side of the rule above: tightening ownership to the files
    // members actually publish must not disown a shim jerky really did write.
    // The member still publishes `cli`; what changed is that nothing depends
    // on it any more — which is exactly the case the plan no longer names, and
    // the reason `Plan::members` carries every member rather than only the
    // depended-on ones.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let root = work.path();

    std::fs::create_dir_all(root.join("packages/cli/bin")).unwrap();
    std::fs::create_dir_all(root.join("apps/web")).unwrap();
    std::fs::write(
        root.join("packages/cli/package.json"),
        r#"{"name":"cli","version":"1.0.0","bin":{"cli":"bin/cli.js"}}"#,
    )
    .unwrap();
    std::fs::write(root.join("packages/cli/bin/cli.js"), "console.log('hi')").unwrap();
    std::fs::write(
        root.join("apps/web/package.json"),
        r#"{"name":"web","dependencies":{"cli":"workspace:*"}}"#,
    )
    .unwrap();

    let workspace = project(
        root,
        r#"{"name":"root","workspaces":["packages/*","apps/*"]}"#,
    );
    let store = Store::new(home.path().join("store"));
    sync(
        &workspace,
        &store,
        &FixtureRegistry::new(),
        None,
        Mode::Develop,
    )
    .unwrap();
    assert!(root.join("apps/web/node_modules/.bin/cli").is_file());

    // `apps/web` stops depending on it; `packages/cli` still publishes it.
    std::fs::write(root.join("apps/web/package.json"), r#"{"name":"web"}"#).unwrap();
    let workspace = project(
        root,
        r#"{"name":"root","workspaces":["packages/*","apps/*"]}"#,
    );
    let outcome = sync(
        &workspace,
        &store,
        &FixtureRegistry::new(),
        None,
        Mode::Develop,
    )
    .unwrap();

    assert!(outcome.left_alone.is_empty(), "{:?}", outcome.left_alone);
    assert!(
        std::fs::symlink_metadata(root.join("apps/web/node_modules/.bin/cli")).is_err(),
        "a shim jerky wrote for a member outlived the dependency on it"
    );
}
