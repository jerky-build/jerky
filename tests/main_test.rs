use std::path::Path;

use assert_cmd::cargo;
use predicates::prelude::predicate;
use tempfile::TempDir;

fn write_manifest(dir: &Path, json: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("package.json"), json).unwrap();
}

#[test]
fn a_bare_install_runs_where_a_named_one_is_ambiguous() {
    // The routing decision `run` makes, through the binary, because that is
    // the only place it exists: with a spec it asks which importer the user
    // is standing in, and without one it must not — `tools/scripts` has no
    // answer to give and a bare install has no need of one.
    //
    // Nothing is declared, so this stays off the network by construction:
    // what is under test is which branch runs, not what resolution does.
    let work = TempDir::new().unwrap();
    let root = work.path();
    write_manifest(root, r#"{"name":"ws","workspaces":["packages/*"]}"#);
    write_manifest(&root.join("packages/ui"), r#"{"name":"ui"}"#);
    let outsider = root.join("tools/scripts");
    std::fs::create_dir_all(&outsider).unwrap();

    cargo::cargo_bin_cmd!("jerky")
        .current_dir(&outsider)
        .arg("install")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "installed 0 packages across 2 importers",
        ));

    cargo::cargo_bin_cmd!("jerky")
        .current_dir(&outsider)
        .args(["install", "lodash"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("belongs to no workspace member"));
}

#[test]
fn it_runs_help() {
    let mut cmd = cargo::cargo_bin_cmd!("jerky");
    cmd.arg("-h").assert().success();
}

#[test]
fn it_errors_on_non_existent_flag() {
    let mut cmd = cargo::cargo_bin_cmd!("jerky");
    cmd.arg("--fake-flag-123")
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains(
            "unexpected argument '--fake-flag-123'",
        ));
}
