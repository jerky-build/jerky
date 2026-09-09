use serde::Deserialize;
use thiserror::Error;

use crate::integrity::{Integrity, IntegrityError};

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

        let body = Self::with_retries(|| match self.agent.get(&url).call() {
            Ok(mut response) => response
                .body_mut()
                .with_config()
                .limit(MAX_METADATA_BYTES)
                .read_to_string()
                .map_err(|source| {
                    (
                        RegistryError::MalformedResponse {
                            url: url.clone(),
                            source: Box::new(source),
                        },
                        Retry::Yes,
                    )
                }),
            Err(ureq::Error::StatusCode(404)) => {
                let err = if self.package_exists(name) {
                    RegistryError::VersionNotFound {
                        name: name.to_string(),
                        version: version.to_string(),
                    }
                } else {
                    RegistryError::PackageNotFound(name.to_string())
                };
                Err((err, Retry::No))
            }
            Err(source) => Err((
                RegistryError::Network {
                    url: url.clone(),
                    source: Box::new(source),
                },
                Retry::Yes,
            )),
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
