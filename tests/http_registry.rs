use std::io::{BufRead, BufReader, Cursor, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use jerky::registry::{
    Fetched, Freshness, HttpRegistry, MAX_CONCURRENT_FETCHES, RegistryClient, RegistryError,
};

/// A canned response for one request path.
struct Route {
    status: u16,
    body: &'static str,
}

/// What every fixture server answers with. One concrete type so a handler can
/// decide between a canned body and a 429 without boxing.
type Reply = tiny_http::Response<Cursor<Vec<u8>>>;

/// Spin up a throwaway HTTP server that answers with `respond`. Returns its
/// base URL.
///
/// `respond` is a closure rather than a routing table so a test can answer the
/// same path differently on the second call — a 429 that succeeds on the retry
/// is exactly that shape.
fn serve_with(mut respond: impl FnMut(&tiny_http::Request) -> Reply + Send + 'static) -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();

    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            let response = respond(&request);
            let _ = request.respond(response);
        }
    });

    format!("http://127.0.0.1:{port}")
}

/// Spin up a server that counts the connections it accepts, answers every
/// request with `body`, and calls `on_request` as each one arrives.
///
/// It speaks HTTP by hand rather than through tiny_http because tiny_http
/// dispatches connections through a task pool that gives each keep-alive
/// connection a thread for its whole life, and a burst of sixteen arriving at
/// once can leave some of them queued behind a thread that never finishes —
/// which is exactly the shape of the fan-out under test. Counting accepts is
/// also the measurement itself, rather than something inferred from the
/// requests.
fn serve_counting_connections(
    body: &'static str,
    on_request: impl Fn() + Send + Sync + 'static,
) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let connections = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&connections);
    let on_request = Arc::new(on_request);

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            counter.fetch_add(1, Ordering::Relaxed);
            let on_request = Arc::clone(&on_request);
            std::thread::spawn(move || serve_one_connection(stream, body, &*on_request));
        }
    });

    (format!("http://127.0.0.1:{port}"), connections)
}

/// Answer every request on one connection until the client hangs up.
///
/// Only GETs arrive, so a request is its head and nothing else: lines up to
/// the blank one. The response carries a `Content-Length` and no `Connection`
/// header, which under HTTP/1.1 is the invitation to send another request on
/// the same socket — without it the client could not reuse a connection even
/// if it wanted to, and the test would measure the fixture.
fn serve_one_connection(mut stream: TcpStream, body: &'static str, on_request: &dyn Fn()) {
    let Ok(peer) = stream.try_clone() else { return };
    let mut head = BufReader::new(peer);

    loop {
        let mut line = String::new();
        loop {
            line.clear();
            match head.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            if line == "\r\n" || line == "\n" {
                break;
            }
        }

        on_request();

        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        if stream.write_all(response.as_bytes()).is_err() {
            return;
        }
    }
}

/// One response header, for the fixtures that answer with `ETag` or
/// `Retry-After`.
fn header(field: &str, value: &str) -> tiny_http::Header {
    tiny_http::Header::from_bytes(field.as_bytes(), value.as_bytes()).unwrap()
}

/// Answer from a routing table, counting requests and recording each request's
/// `Accept` header. Returns the base URL, the counter, and the headers.
fn serve_capturing(
    routes: Vec<(&'static str, Route)>,
) -> (String, Arc<AtomicUsize>, Arc<Mutex<Vec<String>>>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let accepts = Arc::new(Mutex::new(Vec::new()));
    let counter = Arc::clone(&hits);
    let recorder = Arc::clone(&accepts);

    let base = serve_with(move |request| {
        counter.fetch_add(1, Ordering::Relaxed);

        if let Some(header) = request.headers().iter().find(|h| h.field.equiv("Accept")) {
            recorder
                .lock()
                .unwrap()
                .push(header.value.as_str().to_string());
        }

        let url = request.url().to_string();
        match routes.iter().find(|(path, _)| *path == url) {
            Some((_, route)) => {
                tiny_http::Response::from_string(route.body).with_status_code(route.status)
            }
            None => tiny_http::Response::from_string("not found").with_status_code(404),
        }
    });

    (base, hits, accepts)
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

    registry
        .packument("lodash", Freshness::MayBeCached)
        .unwrap();

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

    let p = registry
        .packument("lodash", Freshness::MayBeCached)
        .unwrap();

    assert_eq!(p.name, "lodash");
    assert_eq!(p.versions.len(), 2);
    assert_eq!(p.resolve_tag("latest"), Some("4.17.21"));
}

#[test]
fn packument_reports_an_unknown_package() {
    let (base, _) = serve(vec![]);
    let registry = HttpRegistry::with_base_url(base);

    assert!(matches!(
        registry.packument("nope", Freshness::MayBeCached),
        Err(RegistryError::PackageNotFound(_))
    ));
}

#[test]
fn packument_does_not_retry_a_404() {
    // A missing package is a definite answer, exactly as for a missing version.
    let (base, hits) = serve(vec![]);
    let registry = HttpRegistry::with_base_url(base);

    let _ = registry.packument("nope", Freshness::MayBeCached);

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
        registry.packument("lodash", Freshness::MayBeCached),
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
    let packument = registry
        .packument("express", Freshness::MayBeCached)
        .unwrap();

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

/// A server that speaks conditional requests: it holds one ETag, answers `304`
/// to anyone who presents it, and records every `If-None-Match` it saw.
///
/// Worth a real server rather than a fake client. The whole revalidation path
/// turns on ureq surfacing a `304` as a *successful* response — it is neither a
/// 4xx nor a redirect to follow — and a hand-written fake would simply encode
/// whatever this code already believes about that.
fn serve_conditional(
    etag: &'static str,
    body: &'static str,
) -> (String, Arc<Mutex<Vec<Option<String>>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);

    let base = serve_with(move |request| {
        let sent = request
            .headers()
            .iter()
            .find(|h| h.field.equiv("If-None-Match"))
            .map(|h| h.value.as_str().to_string());
        recorder.lock().unwrap().push(sent.clone());

        if sent.as_deref() == Some(etag) {
            tiny_http::Response::from_string("").with_status_code(304)
        } else {
            tiny_http::Response::from_string(body)
                .with_status_code(200)
                .with_header(header("ETag", etag))
        }
    });

    (base, seen)
}

const LODASH_PACKUMENT: &str = r#"{
    "name": "lodash",
    "dist-tags": { "latest": "4.17.21" },
    "versions": {
        "4.17.21": {
            "name": "lodash", "version": "4.17.21",
            "dist": { "tarball": "https://example.test/lodash-4.17.21.tgz" }
        }
    }
}"#;

#[test]
fn an_unconditional_packument_fetch_reports_the_etag() {
    // Without the ETag coming back out, nothing downstream could ever make a
    // conditional request.
    let (base, _) = serve_conditional("\"abc123\"", LODASH_PACKUMENT);
    let registry = HttpRegistry::with_base_url(base);

    let fetched = registry.packument_conditional("lodash", None).unwrap();

    let jerky::registry::Fetched::Body { packument, etag } = fetched else {
        panic!("an unconditional request must come back with a body");
    };
    assert_eq!(packument.name, "lodash");
    assert_eq!(etag.as_deref(), Some("\"abc123\""));
}

#[test]
fn a_matching_etag_comes_back_as_not_modified() {
    // The assumption the whole freshness design rests on: ureq hands a 304
    // back as a successful response, so it is read off the status rather than
    // caught as an error.
    let (base, seen) = serve_conditional("\"abc123\"", LODASH_PACKUMENT);
    let registry = HttpRegistry::with_base_url(base);

    let fetched = registry
        .packument_conditional("lodash", Some("\"abc123\""))
        .unwrap();

    assert!(
        matches!(fetched, jerky::registry::Fetched::NotModified),
        "a matching ETag must be reported as NotModified, not as a body"
    );
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        [Some("\"abc123\"".to_string())],
        "the If-None-Match header must actually have been sent"
    );
}

#[test]
fn a_stale_etag_comes_back_as_a_fresh_body() {
    let (base, _) = serve_conditional("\"new\"", LODASH_PACKUMENT);
    let registry = HttpRegistry::with_base_url(base);

    let fetched = registry
        .packument_conditional("lodash", Some("\"old\""))
        .unwrap();

    let jerky::registry::Fetched::Body { etag, .. } = fetched else {
        panic!("a non-matching ETag must yield a body");
    };
    assert_eq!(etag.as_deref(), Some("\"new\""));
}

#[test]
fn a_scoped_name_is_escaped_in_a_conditional_request_too() {
    // `@types/node` must be requested as `@types%2fnode`; a path with a real
    // slash in it is a different resource.
    let (base, _) = serve_conditional("\"abc\"", LODASH_PACKUMENT);
    let registry = HttpRegistry::with_base_url(base);

    let fetched = registry.packument_conditional("@types/node", None);

    assert!(
        fetched.is_ok(),
        "a scoped name should reach the server, got {fetched:?}"
    );
}

/// How many requests each worker makes in the fan-out test. Several apiece is
/// the point: a pool sized to the fan-out serves all of them on the
/// connections the workers opened first, so the connection count tracks the
/// worker count rather than the request count.
const REQUESTS_PER_WORKER: usize = 8;

/// Holds each request at the fixture server until a full round of them has
/// arrived, so the fan-out really is in flight at once rather than sixteen
/// workers taking turns at a server fast enough to serve them one at a time.
///
/// A round that never fills — one worker retried, so the requests no longer
/// divide evenly — is released by the settle window instead, which is why this
/// is a window and not a `Barrier`: a fixture that can wedge the suite is worse
/// than one that occasionally lets a round through half-full. Nothing is
/// asserted on the timing either way; the window only buys overlap.
struct Rendezvous {
    width: usize,
    /// How many have arrived this round, and which round it is.
    state: Mutex<(usize, u64)>,
    released: Condvar,
}

/// Long enough to cover the scheduling of sixteen threads on a loaded machine,
/// short enough that eight misaligned rounds still cost under half a second.
const SETTLE: std::time::Duration = std::time::Duration::from_millis(50);

impl Rendezvous {
    fn new(width: usize) -> Self {
        Self {
            width,
            state: Mutex::new((0, 0)),
            released: Condvar::new(),
        }
    }

    fn meet(&self) {
        let mut state = self.state.lock().unwrap();
        let round = state.1;
        state.0 += 1;
        if state.0 == self.width {
            *state = (0, round + 1);
            self.released.notify_all();
            return;
        }
        let _ = self
            .released
            .wait_timeout_while(state, SETTLE, |state| state.1 == round);
    }
}

#[test]
fn a_full_fan_out_reuses_its_connections() {
    // The client advertises a 16-way fan-out at one host, so it must keep 16
    // connections to that host. Keeping fewer means most requests pay a fresh
    // TCP handshake — slow, and the connection-churn signature a registry
    // rate-limits on.
    let served = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&served);
    let rendezvous = Rendezvous::new(MAX_CONCURRENT_FETCHES);
    // Every request is answered with a packument, tarball fetches included:
    // `fetch_tarball` hands back whatever bytes arrive, and one canned body
    // keeps the fixture to the one thing it is measuring.
    let (base, connections) = serve_counting_connections(PACKUMENT, move || {
        counter.fetch_add(1, Ordering::Relaxed);
        rendezvous.meet();
    });
    let registry = HttpRegistry::with_base_url(&base);

    // One client shared by every worker, as install and resolution share one.
    std::thread::scope(|scope| {
        for worker in 0..MAX_CONCURRENT_FETCHES {
            let registry = &registry;
            let base = &base;
            scope.spawn(move || {
                for n in 0..REQUESTS_PER_WORKER {
                    // Both endpoints, because both fan out and both share the
                    // pool.
                    if (worker + n) % 2 == 0 {
                        registry
                            .packument("lodash", Freshness::MustBeCurrent)
                            .unwrap();
                    } else {
                        registry
                            .fetch_tarball(&format!("{base}/lodash-4.17.21.tgz"))
                            .unwrap();
                    }
                }
            });
        }
    });

    let requests = MAX_CONCURRENT_FETCHES * REQUESTS_PER_WORKER;
    let accepted = connections.load(Ordering::Relaxed);
    assert_eq!(
        served.load(Ordering::Relaxed),
        requests,
        "every request ran"
    );
    assert!(
        accepted <= 2 * MAX_CONCURRENT_FETCHES,
        "{requests} requests from {MAX_CONCURRENT_FETCHES} workers opened {accepted} \
         connections; the count should track the workers, not the requests",
    );
}

/// The delay the rate-limited fixture advertises. One second is the shortest a
/// registry can ask for — `Retry-After` counts whole seconds — so it is both
/// realistic and the cheapest honest test of the policy.
const ADVERTISED_DELAY: Duration = Duration::from_secs(1);

#[test]
fn a_429_waits_for_as_long_as_it_was_asked_to() {
    // A 429 is an instruction, not a transient fault: the registry has said
    // when to come back, and coming back sooner is what gets an IP blocked.
    let served = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&served);
    let base = serve_with(move |_| {
        if counter.fetch_add(1, Ordering::Relaxed) == 0 {
            tiny_http::Response::from_string("slow down")
                .with_status_code(429)
                .with_header(header("Retry-After", "1"))
        } else {
            tiny_http::Response::from_string(LODASH)
        }
    });
    let registry = HttpRegistry::with_base_url(base);

    let started = Instant::now();
    let metadata = registry.version_metadata("lodash", "4.17.21").unwrap();
    let waited = started.elapsed();

    assert_eq!(metadata.version, "4.17.21", "the retry succeeded");
    assert_eq!(served.load(Ordering::Relaxed), 2, "one 429, then one retry");
    // A lower bound only: how much longer than the advertised delay the retry
    // took is the machine's business, not the policy's.
    assert!(
        waited >= ADVERTISED_DELAY,
        "retried after {waited:?}, sooner than the {ADVERTISED_DELAY:?} the registry asked for",
    );
}

/// Every sleep a 5xx earns, added up: 100ms before the second attempt and
/// 200ms before the third. A 429 that arrived with no delay attached has to
/// wait longer than all of it before its *first* retry, or the distinction
/// between a fault and an instruction is decorative.
const FIVE_XX_SCHEDULE: Duration = Duration::from_millis(300);

#[test]
fn a_429_without_a_delay_backs_off_further_than_a_5xx() {
    let served = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&served);
    let base = serve_with(move |_| {
        if counter.fetch_add(1, Ordering::Relaxed) == 0 {
            tiny_http::Response::from_string("slow down").with_status_code(429)
        } else {
            tiny_http::Response::from_string(LODASH)
        }
    });
    let registry = HttpRegistry::with_base_url(base);

    let started = Instant::now();
    let metadata = registry.version_metadata("lodash", "4.17.21").unwrap();
    let waited = started.elapsed();

    assert_eq!(metadata.version, "4.17.21", "the retry succeeded");
    assert_eq!(served.load(Ordering::Relaxed), 2, "one 429, then one retry");
    assert!(
        waited > FIVE_XX_SCHEDULE,
        "retried after {waited:?}; a 429 without a delay must wait longer than \
         the whole {FIVE_XX_SCHEDULE:?} a 5xx is given",
    );
}

#[test]
fn a_delay_longer_than_jerky_will_wait_is_reported_rather_than_slept_off() {
    // An hour is longer than a command-line tool can sit there for, and
    // sleeping on it would cost more than the failure it avoids. The whole
    // point of honouring the header is not to retry sooner than asked, so the
    // only honest alternative to waiting is to stop.
    let served = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&served);
    let base = serve_with(move |_| {
        counter.fetch_add(1, Ordering::Relaxed);
        tiny_http::Response::from_string("come back later")
            .with_status_code(429)
            .with_header(header("Retry-After", "3600"))
    });
    let registry = HttpRegistry::with_base_url(base);

    let result = registry.version_metadata("lodash", "4.17.21");

    assert!(matches!(result, Err(RegistryError::Network { .. })));
    // Counted rather than timed: one attempt is the whole claim, and a clock
    // would only add a way for a loaded machine to disagree.
    assert_eq!(
        served.load(Ordering::Relaxed),
        1,
        "the client tried again instead of reporting",
    );
}

// Revalidation is where the two halves of this module meet: the conditional
// request the metadata cache makes runs through the same client whose
// statuses no longer arrive as errors. A 304 has to stay a 304, and
// everything else the registry can answer a revalidation with has to be
// classified rather than parsed. Neither branch could have tested this on its
// own.

/// A conditional fixture that answers one status to everything, whatever the
/// caller presented, optionally advertising a `Retry-After`.
fn serve_conditional_status(
    status: u16,
    retry_after: Option<&'static str>,
) -> (String, Arc<AtomicUsize>) {
    let served = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&served);
    let base = serve_with(move |_| {
        counter.fetch_add(1, Ordering::Relaxed);
        // A body that would parse as a packument if anything were careless
        // enough to read it. That is the failure being excluded.
        let response = tiny_http::Response::from_string(LODASH_PACKUMENT).with_status_code(status);
        match retry_after {
            Some(delay) => response.with_header(header("Retry-After", delay)),
            None => response,
        }
    });
    (base, served)
}

#[test]
fn a_revalidation_that_is_rate_limited_is_not_a_packument() {
    // The dangerous one. A 429 body read as metadata would hand the resolver a
    // packument the registry never sent, on the path the cache takes for every
    // stale entry.
    // `Retry-After: 0` keeps the test instant while still going through the
    // rate-limit path: the schedule itself is pinned by the tests above, and
    // what is at stake here is that the response is classified at all.
    let (base, served) = serve_conditional_status(429, Some("0"));
    let registry = HttpRegistry::with_base_url(base);

    let result = registry.packument_conditional("lodash", Some("\"old\""));

    assert!(
        matches!(result, Err(RegistryError::Network { .. })),
        "a rate-limited revalidation must be an error, got {result:?}"
    );
    assert_eq!(
        served.load(Ordering::Relaxed),
        3,
        "a 429 is retried on the rate-limit schedule, like any other"
    );
}

#[test]
fn a_revalidation_of_a_package_that_is_gone_is_not_a_packument() {
    let (base, served) = serve_conditional_status(404, None);
    let registry = HttpRegistry::with_base_url(base);

    let result = registry.packument_conditional("lodash", Some("\"old\""));

    assert!(
        matches!(result, Err(RegistryError::PackageNotFound(_))),
        "a 404 revalidation must report the package gone, got {result:?}"
    );
    assert_eq!(
        served.load(Ordering::Relaxed),
        1,
        "a 404 is a definite answer on this path too"
    );
}

#[test]
fn a_revalidation_that_matches_is_not_a_retry() {
    // 304 is not `is_success`, so a status check that asked only that question
    // would turn every confirmed-current entry into three requests and an
    // error. The cache would then re-download exactly what it revalidated.
    let (base, seen) = serve_conditional("\"abc123\"", LODASH_PACKUMENT);
    let registry = HttpRegistry::with_base_url(base);

    let fetched = registry
        .packument_conditional("lodash", Some("\"abc123\""))
        .unwrap();

    assert!(matches!(fetched, Fetched::NotModified));
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "a 304 is an answer, not something to try again"
    );
}
