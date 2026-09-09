use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use sha1::Sha1;
use sha2::{Digest, Sha512};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum IntegrityError {
    #[error("package metadata contains neither an integrity nor a shasum field")]
    Missing,
    #[error("could not parse integrity string `{0}`")]
    Unparseable(String),
    #[error("unsupported integrity algorithm `{0}`")]
    UnsupportedAlgorithm(String),
    #[error("integrity check failed: expected {expected}, got {actual}")]
    Mismatch { expected: String, actual: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algo {
    Sha512,
    Sha1,
}

impl Algo {
    fn name(self) -> &'static str {
        match self {
            Algo::Sha512 => "sha512",
            Algo::Sha1 => "sha1",
        }
    }

    fn digest_len(self) -> usize {
        match self {
            Algo::Sha512 => 64,
            Algo::Sha1 => 20,
        }
    }

    fn hash(self, bytes: &[u8]) -> Vec<u8> {
        match self {
            Algo::Sha512 => Sha512::digest(bytes).to_vec(),
            Algo::Sha1 => Sha1::digest(bytes).to_vec(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Integrity {
    pub algo: Algo,
    pub digest: Vec<u8>,
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

impl Integrity {
    /// Parse an SSRI string of the form `sha512-<base64>`, as the npm registry
    /// reports in `dist.integrity`.
    pub fn parse(input: &str) -> Result<Self, IntegrityError> {
        let (algo_name, encoded) = input
            .split_once('-')
            .ok_or_else(|| IntegrityError::Unparseable(input.to_string()))?;

        let algo = match algo_name {
            "sha512" => Algo::Sha512,
            "sha1" => Algo::Sha1,
            other => return Err(IntegrityError::UnsupportedAlgorithm(other.to_string())),
        };

        let digest = BASE64
            .decode(encoded)
            .map_err(|_| IntegrityError::Unparseable(input.to_string()))?;

        if digest.len() != algo.digest_len() {
            return Err(IntegrityError::Unparseable(input.to_string()));
        }

        Ok(Self { algo, digest })
    }

    /// Parse a legacy hex `dist.shasum`. Packages published before roughly
    /// 2017 carry only this, with no `integrity` field.
    pub fn from_shasum_hex(input: &str) -> Result<Self, IntegrityError> {
        if input.len() != Algo::Sha1.digest_len() * 2 {
            return Err(IntegrityError::Unparseable(input.to_string()));
        }
        let digest = (0..input.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&input[i..i + 2], 16))
            .collect::<Result<Vec<u8>, _>>()
            .map_err(|_| IntegrityError::Unparseable(input.to_string()))?;

        Ok(Self {
            algo: Algo::Sha1,
            digest,
        })
    }

    /// The store directory name for this digest.
    ///
    /// Lowercase hex rather than base64: macOS filesystems are
    /// case-insensitive by default, so base64 keys can collide. The algorithm
    /// prefix keeps sha1 and sha512 entries in separate namespaces.
    pub fn store_key(&self) -> String {
        format!("{}-{}", self.algo.name(), to_hex(&self.digest))
    }

    pub fn to_ssri(&self) -> String {
        format!("{}-{}", self.algo.name(), BASE64.encode(&self.digest))
    }

    pub fn verify(&self, bytes: &[u8]) -> Result<(), IntegrityError> {
        let actual = self.algo.hash(bytes);
        if actual == self.digest {
            return Ok(());
        }
        Err(IntegrityError::Mismatch {
            expected: self.to_ssri(),
            actual: format!("{}-{}", self.algo.name(), BASE64.encode(&actual)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::testing::ABC_SHA512_SSRI;

    #[test]
    fn parses_a_sha512_ssri_string() {
        let integrity = Integrity::parse(ABC_SHA512_SSRI).unwrap();
        assert_eq!(integrity.algo, Algo::Sha512);
        assert_eq!(integrity.digest.len(), 64);
    }

    #[test]
    fn verifies_matching_bytes() {
        let integrity = Integrity::parse(ABC_SHA512_SSRI).unwrap();
        assert!(integrity.verify(b"abc").is_ok());
    }

    #[test]
    fn rejects_mismatched_bytes() {
        let integrity = Integrity::parse(ABC_SHA512_SSRI).unwrap();
        assert!(matches!(
            integrity.verify(b"abd"),
            Err(IntegrityError::Mismatch { .. })
        ));
    }

    #[test]
    fn store_keys_are_lowercase_and_algo_prefixed() {
        let integrity = Integrity::parse(ABC_SHA512_SSRI).unwrap();
        let key = integrity.store_key();
        assert!(key.starts_with("sha512-"));
        assert_eq!(
            key,
            key.to_lowercase(),
            "store keys must be case-safe on macOS"
        );
        // "sha512-" plus 64 bytes rendered as two hex chars each.
        assert_eq!(key.len(), 7 + 128);
    }

    #[test]
    fn parses_a_legacy_sha1_shasum() {
        // sha1 of "abc"
        let integrity =
            Integrity::from_shasum_hex("a9993e364706816aba3e25717850c26c9cd0d89d").unwrap();
        assert_eq!(integrity.algo, Algo::Sha1);
        assert!(integrity.verify(b"abc").is_ok());
    }

    #[test]
    fn rejects_an_unsupported_algorithm() {
        assert!(matches!(
            Integrity::parse("md5-abcdef"),
            Err(IntegrityError::UnsupportedAlgorithm(_))
        ));
    }

    #[test]
    fn rejects_a_string_without_a_separator() {
        assert!(matches!(
            Integrity::parse("sha512"),
            Err(IntegrityError::Unparseable(_))
        ));
    }

    #[test]
    fn rejects_invalid_base64() {
        assert!(matches!(
            Integrity::parse("sha512-!!!not base64!!!"),
            Err(IntegrityError::Unparseable(_))
        ));
    }

    #[test]
    fn rejects_a_digest_of_the_wrong_length() {
        // Valid base64, but only three bytes rather than 64.
        assert!(matches!(
            Integrity::parse("sha512-YWJj"),
            Err(IntegrityError::Unparseable(_))
        ));
    }

    #[test]
    fn round_trips_through_ssri() {
        let integrity = Integrity::parse(ABC_SHA512_SSRI).unwrap();
        assert_eq!(integrity.to_ssri(), ABC_SHA512_SSRI);
    }
}
