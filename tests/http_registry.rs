use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use jerky::registry::{HttpRegistry, RegistryClient, RegistryError};

/// A canned response for one request path.
struct Route {
    status: u16,
    body: &'static str,
}

/// Spin up a throwaway HTTP server that answers from a routing table, counts
/// requests, and records each request's `Accept` header. Returns the base URL,
/// the counter, and the recorded headers.
fn serve_capturing(
    routes: Vec<(&'static str, Route)>,
) -> (String, Arc<AtomicUsize>, Arc<Mutex<Vec<String>>>) {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    let hits = Arc::new(AtomicUsize::new(0));
    let accepts = Arc::new(Mutex::new(Vec::new()));
    let counter = Arc::clone(&hits);
    let recorder = Arc::clone(&accepts);

    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            counter.fetch_add(1, Ordering::Relaxed);

            if let Some(header) = request.headers().iter().find(|h| h.field.equiv("Accept")) {
                recorder
                    .lock()
                    .unwrap()
                    .push(header.value.as_str().to_string());
            }

            let url = request.url().to_string();
            let matched = routes.iter().find(|(path, _)| *path == url);

            let response = match matched {
                Some((_, route)) => {
                    tiny_http::Response::from_string(route.body).with_status_code(route.status)
                }
                None => tiny_http::Response::from_string("not found").with_status_code(404),
            };
            let _ = request.respond(response);
        }
    });

    (format!("http://127.0.0.1:{port}"), hits, accepts)
}

/// The common case, for tests that do not care about headers.
fn serve(routes: Vec<(&'static str, Route)>) -> (String, Arc<AtomicUsize>) {
    let (base, hits, _) = serve_capturing(routes);
    (base, hits)
}

const LODASH: &str = r#"{
    "name": "lodash",
    "version": "4.17.21",
    "dist": {
        "tarball": "https://example.test/lodash-4.17.21.tgz",
        "integrity": "sha512-v2kDEe57lecTulaDIuNTPy3Ry4gLGJ6Z1O3vE1krgXZNrsQ+LFTGHVxVjcXPs17LhbZVGedAJv8XZ1tvj5FvSg=="
    }
}"#;

#[test]
fn fetches_a_version_manifest() {
    let (base, _) = serve(vec![(
        "/lodash/4.17.21",
        Route {
            status: 200,
            body: LODASH,
        },
    )]);
    let registry = HttpRegistry::with_base_url(base);

    let metadata = registry.version_metadata("lodash", "4.17.21").unwrap();

    assert_eq!(metadata.version, "4.17.21");
}

#[test]
fn resolves_a_dist_tag_through_the_same_endpoint() {
    let (base, _) = serve(vec![(
        "/lodash/latest",
        Route {
            status: 200,
            body: LODASH,
        },
    )]);
    let registry = HttpRegistry::with_base_url(base);

    let metadata = registry.version_metadata("lodash", "latest").unwrap();

    assert_eq!(metadata.version, "4.17.21", "the response is authoritative");
}

#[test]
fn distinguishes_an_unknown_package_from_an_unknown_version() {
    // The package exists, the version does not: the probe of /lodash succeeds.
    let (base, _) = serve(vec![(
        "/lodash",
        Route {
            status: 200,
            body: "{}",
        },
    )]);
    let registry = HttpRegistry::with_base_url(base);

    assert!(matches!(
        registry.version_metadata("lodash", "9.9.9"),
        Err(RegistryError::VersionNotFound { .. })
    ));
}

#[test]
fn reports_an_unknown_package() {
    // Nothing is routed, so both the version request and the probe 404.
    let (base, _) = serve(vec![]);
    let registry = HttpRegistry::with_base_url(base);

    assert!(matches!(
        registry.version_metadata("nope", "1.0.0"),
        Err(RegistryError::PackageNotFound(_))
    ));
}

#[test]
fn does_not_retry_a_404() {
    let (base, hits) = serve(vec![]);
    let registry = HttpRegistry::with_base_url(base);

    let _ = registry.version_metadata("nope", "1.0.0");

    // One version request plus one existence probe. Retrying a definite
    // answer only makes the tool slow at being wrong.
    assert_eq!(hits.load(Ordering::Relaxed), 2);
}

#[test]
fn retries_a_server_error() {
    let (base, hits) = serve(vec![(
        "/lodash/4.17.21",
        Route {
            status: 503,
            body: "down",
        },
    )]);
    let registry = HttpRegistry::with_base_url(base);

    let result = registry.version_metadata("lodash", "4.17.21");

    assert!(matches!(result, Err(RegistryError::Network { .. })));
    assert_eq!(hits.load(Ordering::Relaxed), 3, "three attempts");
}

#[test]
fn reports_a_malformed_response() {
    let (base, _) = serve(vec![(
        "/lodash/4.17.21",
        Route {
            status: 200,
            body: "{ not json",
        },
    )]);
    let registry = HttpRegistry::with_base_url(base);

    assert!(matches!(
        registry.version_metadata("lodash", "4.17.21"),
        Err(RegistryError::MalformedResponse { .. })
    ));
}

#[test]
fn fetches_tarball_bytes() {
    let (base, _) = serve(vec![(
        "/x.tgz",
        Route {
            status: 200,
            body: "tarball-bytes",
        },
    )]);
    let registry = HttpRegistry::new();

    let bytes = registry.fetch_tarball(&format!("{base}/x.tgz")).unwrap();

    assert_eq!(bytes, b"tarball-bytes");
}

#[test]
#[ignore = "hits the real npm registry; run on a schedule, not per-PR"]
fn resolves_lodash_against_the_real_registry() {
    let registry = HttpRegistry::new();
    let metadata = registry.version_metadata("lodash", "4.17.21").unwrap();

    assert_eq!(metadata.version, "4.17.21");
    assert!(metadata.dist.integrity.is_some());
}

const PACKUMENT: &str = r#"{
    "name": "lodash",
    "dist-tags": { "latest": "4.17.21", "next": "5.0.0-beta.1" },
    "versions": {
        "4.17.20": { "name": "lodash", "version": "4.17.20",
            "dist": { "tarball": "https://r.test/l-4.17.20.tgz", "integrity": "sha512-v2kDEe57lecTulaDIuNTPy3Ry4gLGJ6Z1O3vE1krgXZNrsQ+LFTGHVxVjcXPs17LhbZVGedAJv8XZ1tvj5FvSg==" } },
        "4.17.21": { "name": "lodash", "version": "4.17.21",
            "dist": { "tarball": "https://r.test/l-4.17.21.tgz", "integrity": "sha512-v2kDEe57lecTulaDIuNTPy3Ry4gLGJ6Z1O3vE1krgXZNrsQ+LFTGHVxVjcXPs17LhbZVGedAJv8XZ1tvj5FvSg==" } }
    }
}"#;

#[test]
fn packument_requests_the_abbreviated_form() {
    // The unabbreviated document for a popular package is megabytes of every
    // version ever published, so the Accept header is not an optimisation.
    let (base, _, accepts) = serve_capturing(vec![(
        "/lodash",
        Route {
            status: 200,
            body: PACKUMENT,
        },
    )]);
    let registry = HttpRegistry::with_base_url(base);

    registry.packument("lodash").unwrap();

    let seen = accepts.lock().unwrap().clone();
    assert!(
        seen.iter()
            .any(|h| h == "application/vnd.npm.install-v1+json"),
        "abbreviated packument not requested; Accept headers seen: {seen:?}"
    );
}

#[test]
fn packument_returns_every_published_version() {
    let (base, _) = serve(vec![(
        "/lodash",
        Route {
            status: 200,
            body: PACKUMENT,
        },
    )]);
    let registry = HttpRegistry::with_base_url(base);

    let p = registry.packument("lodash").unwrap();

    assert_eq!(p.name, "lodash");
    assert_eq!(p.versions.len(), 2);
    assert_eq!(p.resolve_tag("latest"), Some("4.17.21"));
}

#[test]
fn packument_reports_an_unknown_package() {
    let (base, _) = serve(vec![]);
    let registry = HttpRegistry::with_base_url(base);

    assert!(matches!(
        registry.packument("nope"),
        Err(RegistryError::PackageNotFound(_))
    ));
}

#[test]
fn packument_does_not_retry_a_404() {
    // A missing package is a definite answer, exactly as for a missing version.
    let (base, hits) = serve(vec![]);
    let registry = HttpRegistry::with_base_url(base);

    let _ = registry.packument("nope");

    assert_eq!(hits.load(Ordering::Relaxed), 1);
}

#[test]
fn packument_retries_a_server_error() {
    let (base, hits) = serve(vec![(
        "/lodash",
        Route {
            status: 503,
            body: "down",
        },
    )]);
    let registry = HttpRegistry::with_base_url(base);

    assert!(matches!(
        registry.packument("lodash"),
        Err(RegistryError::Network { .. })
    ));
    assert_eq!(hits.load(Ordering::Relaxed), 3, "three attempts");
}

#[test]
#[ignore = "hits the real npm registry; run on a schedule, not per-PR"]
fn resolves_a_real_packument_and_selects_a_version() {
    // Real packuments are far messier than fixtures: hundreds of versions,
    // fields jerky ignores, and historical entries that predate current
    // conventions. This is what proves the abbreviated form actually parses.
    use jerky::range::Range;

    let registry = HttpRegistry::new();
    let packument = registry.packument("express").unwrap();

    assert_eq!(packument.name, "express");
    assert!(
        packument.versions.len() > 100,
        "expected express to have a long history, got {}",
        packument.versions.len()
    );
    assert!(packument.resolve_tag("latest").is_some());

    let versions = packument.versions_sorted();
    assert!(
        versions.len() > 100,
        "versions were dropped as unparseable: {} of {}",
        versions.len(),
        packument.versions.len()
    );

    // And a range actually selects from it.
    let chosen = Range::parse("^4.0.0")
        .unwrap()
        .max_satisfying(&versions)
        .expect("express has a 4.x release");
    assert!(chosen.as_str().starts_with('4'), "chose {chosen}");
}
