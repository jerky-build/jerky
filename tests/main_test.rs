use assert_cmd::cargo;
use predicates::prelude::predicate;

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
