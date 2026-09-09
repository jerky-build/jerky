use std::collections::BTreeMap;

use serde::Deserialize;
use thiserror::Error;

use crate::integrity::{Integrity, IntegrityError};
use crate::range::Version;

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("package `{0}` was not found in the registry")]
    PackageNotFound(String),
    #[error("package `{name}` has no version `{version}`")]
    VersionNotFound { name: String, version: String },
    #[error("could not reach the registry at {url}")]
    Network {
        url: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("the registry returned a response jerky could not understand for {url}")]
    MalformedResponse {
        url: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

/// The subset of a version manifest jerky needs. Unknown fields are ignored,
/// so registry additions do not break deserialization.
#[derive(Debug, Clone, Deserialize)]
pub struct VersionMetadata {
    pub name: String,
    pub version: String,
    pub dist: Dist,
    /// Runtime dependencies only.
    ///
    /// There is deliberately no `devDependencies` field. A dependency's dev
    /// dependencies must never be followed — doing so pulls in most of the
    /// registry — and a field that exists is a field someone will read.
    #[serde(default)]
    pub dependencies: BTreeMap<String, String>,
}

/// Every published version of one package, in the registry's abbreviated form.
#[derive(Debug, Clone, Deserialize)]
pub struct Packument {
    pub name: String,
    #[serde(default)]
    pub versions: BTreeMap<String, VersionMetadata>,
    #[serde(rename = "dist-tags", default)]
    pub dist_tags: BTreeMap<String, String>,
}

impl Packument {
    /// Every version that parses, in precedence order.
    ///
    /// Unparseable versions are dropped rather than fatal: the registry has
    /// accumulated some genuinely malformed entries, and one of them must not
    /// make an otherwise-fine package unresolvable.
    ///
    /// Sorting matters. The registry's own key order is lexical, which puts
    /// `1.10.0` before `1.9.0`.
    pub fn versions_sorted(&self) -> Vec<Version> {
        let mut out: Vec<Version> = self
            .versions
            .keys()
            .filter_map(|v| Version::parse(v).ok())
            .collect();
        out.sort();
        out
    }

    /// Resolve a dist-tag such as `latest` to a concrete version.
    pub fn resolve_tag(&self, tag: &str) -> Option<&str> {
        self.dist_tags.get(tag).map(String::as_str)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Dist {
    pub tarball: String,
    #[serde(default)]
    pub integrity: Option<String>,
    #[serde(default)]
    pub shasum: Option<String>,
}

impl Dist {
    /// Prefer the modern `integrity` field, fall back to the legacy hex
    /// `shasum`, and report `Missing` when a package offers neither.
    pub fn integrity(&self) -> Result<Integrity, IntegrityError> {
        if let Some(raw) = &self.integrity {
            return Integrity::parse(raw);
        }
        if let Some(raw) = &self.shasum {
            return Integrity::from_shasum_hex(raw);
        }
        Err(IntegrityError::Missing)
    }
}

/// Everything jerky needs from a package registry.
///
/// The trait exists so the install pipeline can be driven by a fixture in
/// tests: offline, deterministic, and fast. Its cost is that the fixture
/// covers everything *except* the module it replaces, so `HttpRegistry` needs
/// its own tests.
pub trait RegistryClient {
    /// Fetch one version's manifest. `version` may be a concrete version or a
    /// dist-tag such as `latest`; the registry resolves both on this endpoint.
    fn version_metadata(&self, name: &str, version: &str)
    -> Result<VersionMetadata, RegistryError>;

    /// Every published version of a package, for range resolution.
    fn packument(&self, name: &str) -> Result<Packument, RegistryError>;

    fn fetch_tarball(&self, url: &str) -> Result<Vec<u8>, RegistryError>;
}

pub const DEFAULT_REGISTRY: &str = "https://registry.npmjs.org";

const MAX_ATTEMPTS: u32 = 3;

/// ureq's `read_to_vec`/`read_to_string` default to a 10 MB cap, which many
/// real tarballs exceed, so both limits are raised explicitly. A version
/// manifest is a few KB; the generous ceiling is for pathological ones.
///
/// Exceeding the limit is an error (`BodyExceedsLimit`), not a truncation, so
/// the failure mode of leaving these at the default would be a loud refusal to
/// install a large package rather than a corrupted one. The caps stay because
/// an unbounded read lets a hostile registry exhaust memory.
///
/// Note that ureq rejects a body of *exactly* the limit: its `LimitReader`
/// errors once the remaining allowance reaches zero rather than when it would
/// go negative. Harmless at these sizes, but it is why the ceilings are round
/// numbers well clear of any real payload rather than tight fits.
const MAX_METADATA_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TARBALL_BYTES: u64 = 512 * 1024 * 1024;

/// The npm registry over HTTP.
pub struct HttpRegistry {
    base_url: String,
    agent: ureq::Agent,
}

impl Default for HttpRegistry {
    fn default() -> Self {
        Self::new()
    }
}

enum Retry {
    Yes,
    No,
}

impl HttpRegistry {
    pub fn new() -> Self {
        Self::with_base_url(DEFAULT_REGISTRY)
    }

    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        let config = ureq::Agent::config_builder()
            .user_agent(concat!("jerky/", env!("CARGO_PKG_VERSION")))
            .build();

        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            agent: config.into(),
        }
    }

    /// Scoped names carry a `/` that must be escaped in a path segment:
    /// `@types/node` is requested as `@types%2fnode`.
    fn encode_name(name: &str) -> String {
        name.replace('/', "%2f")
    }

    /// Run an idempotent read, retrying transport failures and 5xx responses.
    ///
    /// `Retry::No` results short-circuit: a 404 is a definite answer, and
    /// retrying it only makes the tool slow at being wrong.
    fn with_retries<T>(
        mut attempt: impl FnMut() -> Result<T, (RegistryError, Retry)>,
    ) -> Result<T, RegistryError> {
        let mut last = None;
        for n in 0..MAX_ATTEMPTS {
            match attempt() {
                Ok(value) => return Ok(value),
                Err((err, Retry::No)) => return Err(err),
                Err((err, Retry::Yes)) => {
                    last = Some(err);
                    if n + 1 < MAX_ATTEMPTS {
                        std::thread::sleep(std::time::Duration::from_millis(100 * 2_u64.pow(n)));
                    }
                }
            }
        }
        Err(last.expect("at least one attempt ran"))
    }

    /// Fetch a JSON body, applying the retry policy and the size limit.
    ///
    /// The two metadata endpoints differ only in their URL, whether they ask
    /// for the abbreviated form, and what a 404 means, so everything else —
    /// retries, limits, error mapping — lives here rather than twice.
    fn get_json(
        &self,
        url: &str,
        abbreviated: bool,
        not_found: impl Fn() -> RegistryError,
    ) -> Result<String, RegistryError> {
        Self::with_retries(|| {
            let mut request = self.agent.get(url);
            if abbreviated {
                // The unabbreviated document for a popular package is
                // megabytes of every version ever published, so this is not
                // an optimisation.
                request = request.header("Accept", "application/vnd.npm.install-v1+json");
            }

            match request.call() {
                Ok(mut response) => response
                    .body_mut()
                    .with_config()
                    .limit(MAX_METADATA_BYTES)
                    .read_to_string()
                    .map_err(|source| {
                        (
                            RegistryError::MalformedResponse {
                                url: url.to_string(),
                                source: Box::new(source),
                            },
                            Retry::Yes,
                        )
                    }),
                Err(ureq::Error::StatusCode(404)) => Err((not_found(), Retry::No)),
                Err(source) => Err((
                    RegistryError::Network {
                        url: url.to_string(),
                        source: Box::new(source),
                    },
                    Retry::Yes,
                )),
            }
        })
    }

    /// Does this package exist at all? Used only on the error path, to turn a
    /// 404 into either `PackageNotFound` or `VersionNotFound`.
    fn package_exists(&self, name: &str) -> bool {
        let url = format!("{}/{}", self.base_url, Self::encode_name(name));
        self.agent.get(&url).call().is_ok()
    }
}

impl RegistryClient for HttpRegistry {
    fn version_metadata(
        &self,
        name: &str,
        version: &str,
    ) -> Result<VersionMetadata, RegistryError> {
        let url = format!("{}/{}/{}", self.base_url, Self::encode_name(name), version);

        // A 404 here is ambiguous: the package may not exist, or it may exist
        // without this version. Only the error path pays for the distinction.
        let body = self.get_json(&url, false, || {
            if self.package_exists(name) {
                RegistryError::VersionNotFound {
                    name: name.to_string(),
                    version: version.to_string(),
                }
            } else {
                RegistryError::PackageNotFound(name.to_string())
            }
        })?;

        serde_json::from_str(&body).map_err(|source| RegistryError::MalformedResponse {
            url,
            source: Box::new(source),
        })
    }

    fn packument(&self, name: &str) -> Result<Packument, RegistryError> {
        let url = format!("{}/{}", self.base_url, Self::encode_name(name));

        let body = self.get_json(&url, true, || {
            RegistryError::PackageNotFound(name.to_string())
        })?;

        serde_json::from_str(&body).map_err(|source| RegistryError::MalformedResponse {
            url,
            source: Box::new(source),
        })
    }

    fn fetch_tarball(&self, url: &str) -> Result<Vec<u8>, RegistryError> {
        Self::with_retries(|| match self.agent.get(url).call() {
            Ok(mut response) => response
                .body_mut()
                .with_config()
                .limit(MAX_TARBALL_BYTES)
                .read_to_vec()
                .map_err(|source| {
                    (
                        RegistryError::Network {
                            url: url.to_string(),
                            source: Box::new(source),
                        },
                        Retry::Yes,
                    )
                }),
            Err(ureq::Error::StatusCode(404)) => {
                Err((RegistryError::PackageNotFound(url.to_string()), Retry::No))
            }
            Err(source) => Err((
                RegistryError::Network {
                    url: url.to_string(),
                    source: Box::new(source),
                },
                Retry::Yes,
            )),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::testing::ABC_SHA512_SSRI;

    #[test]
    fn dist_prefers_the_integrity_field() {
        let dist = Dist {
            tarball: "https://example.test/x.tgz".into(),
            integrity: Some(ABC_SHA512_SSRI.into()),
            shasum: Some("a9993e364706816aba3e25717850c26c9cd0d89d".into()),
        };
        assert_eq!(
            dist.integrity().unwrap().algo,
            crate::integrity::Algo::Sha512
        );
    }

    #[test]
    fn dist_falls_back_to_a_legacy_shasum() {
        // Packages published before roughly 2017 have no integrity field.
        let dist = Dist {
            tarball: "https://example.test/x.tgz".into(),
            integrity: None,
            shasum: Some("a9993e364706816aba3e25717850c26c9cd0d89d".into()),
        };
        assert_eq!(dist.integrity().unwrap().algo, crate::integrity::Algo::Sha1);
    }

    #[test]
    fn dist_reports_when_neither_is_present() {
        let dist = Dist {
            tarball: "https://example.test/x.tgz".into(),
            integrity: None,
            shasum: None,
        };
        assert!(matches!(
            dist.integrity(),
            Err(crate::integrity::IntegrityError::Missing)
        ));
    }

    #[test]
    fn packument_deserializes_a_registry_response() {
        let raw = r#"{
            "name": "lodash",
            "dist-tags": { "latest": "4.17.21", "next": "5.0.0-beta.1" },
            "versions": {
                "4.17.20": { "name": "lodash", "version": "4.17.20",
                    "dist": { "tarball": "https://r.test/a.tgz" } },
                "4.17.21": { "name": "lodash", "version": "4.17.21",
                    "dist": { "tarball": "https://r.test/b.tgz" } }
            }
        }"#;

        let p: Packument = serde_json::from_str(raw).unwrap();
        assert_eq!(p.name, "lodash");
        assert_eq!(p.versions.len(), 2);
        assert_eq!(p.resolve_tag("latest"), Some("4.17.21"));
        assert_eq!(p.resolve_tag("nope"), None);
    }

    #[test]
    fn packument_skips_versions_it_cannot_parse() {
        // The registry has accumulated some genuinely malformed versions over
        // the years. One bad entry must not make a package unresolvable.
        let raw = r#"{
            "name": "old",
            "dist-tags": {},
            "versions": {
                "1.0.0":     { "name": "old", "version": "1.0.0",
                    "dist": { "tarball": "https://r.test/a.tgz" } },
                "not-a-ver": { "name": "old", "version": "not-a-ver",
                    "dist": { "tarball": "https://r.test/b.tgz" } }
            }
        }"#;

        let p: Packument = serde_json::from_str(raw).unwrap();
        let sorted = p.versions_sorted();
        assert_eq!(
            sorted.len(),
            1,
            "the unparseable version is dropped, not fatal"
        );
        assert_eq!(sorted[0].as_str(), "1.0.0");
    }

    #[test]
    fn packument_versions_come_back_in_precedence_order() {
        // Registry key order is lexical, which puts 1.10.0 before 1.9.0.
        let raw = r#"{
            "name": "p", "dist-tags": {},
            "versions": {
                "1.9.0":  { "name": "p", "version": "1.9.0",  "dist": { "tarball": "https://r.test/a.tgz" } },
                "1.10.0": { "name": "p", "version": "1.10.0", "dist": { "tarball": "https://r.test/b.tgz" } }
            }
        }"#;

        let p: Packument = serde_json::from_str(raw).unwrap();
        let sorted = p.versions_sorted();
        let order: Vec<&str> = sorted.iter().map(|v| v.as_str()).collect();
        assert_eq!(order, ["1.9.0", "1.10.0"]);
    }

    #[test]
    fn version_metadata_deserializes_a_registry_response() {
        let raw = r#"{
            "name": "lodash",
            "version": "4.17.21",
            "description": "ignored",
            "dist": {
                "tarball": "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz",
                "shasum": "679591c564c3bffaae8454cf0b3df370c3d6911c",
                "integrity": "sha512-v2kDEe57lecTulaDIuNTPy3Ry4gLGJ6Z1O3vE1krgXZNrsQ+LFTGHVxVjcXPs17LhbZVGedAJv8XZ1tvj5FvSg=="
            }
        }"#;

        let metadata: VersionMetadata = serde_json::from_str(raw).unwrap();
        assert_eq!(metadata.name, "lodash");
        assert_eq!(metadata.version, "4.17.21");
        assert!(metadata.dist.integrity.is_some());
    }
}
