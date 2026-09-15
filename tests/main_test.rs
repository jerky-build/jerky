use std::path::Path;

use assert_cmd::cargo;
use jerky::registry::RegistryClient as _;
use jerky::testing::{FixtureRegistry, TarEntry, build_tarball};
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

/// Serve one package — its packument and its tarball — from a throwaway
/// server, and answer with the base URL.
///
/// `dist.tarball` is absolute in a real packument and absolute here, pointing
/// back at this server, which is the shape the benchmark's replay mirror
/// reproduces: a mirror redirects those URLs at itself rather than the client
/// resolving them against the override.
fn serve_one_package(name: &'static str, version: &'static str, tarball: Vec<u8>) -> String {
    // The integrity comes from the fixture registry, which derives it from the
    // same bytes. Computing it here would mean hashing in the test, and a test
    // that computes its own expected hash is testing its own arithmetic.
    let integrity = FixtureRegistry::new()
        .with_package(name, version, tarball.clone())
        .version_metadata(name, version)
        .unwrap()
        .dist
        .integrity
        .expect("the fixture registry derives an integrity from the bytes");

    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    let base = format!("http://127.0.0.1:{port}");

    let tarball_path = format!("/{name}/-/{name}-{version}.tgz");
    let packument = format!(
        r#"{{"name":"{name}","dist-tags":{{"latest":"{version}"}},"versions":{{
             "{version}":{{"name":"{name}","version":"{version}",
               "dist":{{"tarball":"{base}{tarball_path}","integrity":"{integrity}"}}}}}}}}"#
    );

    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            let url = request.url().to_string();
            let _ = if url == format!("/{name}") {
                request.respond(tiny_http::Response::from_string(packument.clone()))
            } else if url == tarball_path {
                request.respond(tiny_http::Response::from_data(tarball.clone()))
            } else {
                request.respond(tiny_http::Response::from_string("not found").with_status_code(404))
            };
        }
    });

    base
}

#[test]
fn the_registry_comes_from_the_environment() {
    // The seam the benchmark's replay mirror hangs off. `HttpRegistry` has
    // taken a base URL since it was written and ~20 integration tests drive it
    // that way, but the binary hardcoded the default at its composition point,
    // so nothing built out of `main` could be pointed anywhere else — which is
    // why a benchmark run had no choice but to hit the live registry.
    //
    // A full install rather than a probe: what has to be true is that every
    // request an install makes — the packument and the tarball both — lands on
    // the override, and a test that only proved the first would pass with
    // tarballs still going to npm.
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let base = serve_one_package(
        "lodash",
        "4.17.21",
        build_tarball(&[
            TarEntry::file(
                "package/package.json",
                r#"{"name":"lodash","version":"4.17.21"}"#,
            ),
            TarEntry::file("package/lodash.js", "module.exports = {};"),
        ]),
    );

    write_manifest(
        work.path(),
        r#"{"name":"demo","dependencies":{"lodash":"^4.17.0"}}"#,
    );

    cargo::cargo_bin_cmd!("jerky")
        .current_dir(work.path())
        .env("HOME", home.path())
        .env("JERKY_REGISTRY_URL", &base)
        .arg("install")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "installed 1 package across 1 importer",
        ));

    // Through the link, so this is the tarball the override served and not a
    // lockfile entry that merely names it.
    let linked = work.path().join("node_modules/lodash/lodash.js");
    assert_eq!(
        std::fs::read_to_string(&linked).unwrap(),
        "module.exports = {};"
    );

    // And the resolution was written down against the override, not npm.
    let lock = std::fs::read_to_string(work.path().join("jerky-lock.json")).unwrap();
    assert!(
        lock.contains(&base),
        "the lockfile records the registry that answered:\n{lock}"
    );
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
