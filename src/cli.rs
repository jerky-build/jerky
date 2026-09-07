use clap::{Parser, Subcommand};
use thiserror::Error;

#[derive(Debug, Parser)]
#[command(
    name = "jerky",
    version,
    about = "A JavaScript package manager and build tool"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a package.json in the current directory
    Init,
    /// Install a package into node_modules
    Install {
        /// Package to install, e.g. `lodash` or `lodash@4.17.21`
        spec: String,
    },
}

#[derive(Debug, Error)]
pub enum CliError {
    #[error("package spec cannot be empty")]
    EmptySpec,
    #[error("version cannot be empty in `{0}`")]
    EmptyVersion(String),
    #[error("`{0}` is not a valid package name")]
    InvalidName(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionSpec {
    Latest,
    Exact(String),
}

impl VersionSpec {
    /// The string sent to the registry: either a concrete version or a dist-tag.
    /// `Latest` becomes the literal tag `latest`, which the registry resolves
    /// through the same endpoint as a version.
    pub fn as_request(&self) -> &str {
        match self {
            VersionSpec::Latest => "latest",
            VersionSpec::Exact(v) => v,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageSpec {
    pub name: String,
    pub version: VersionSpec,
}

fn validate_name(name: &str, input: &str) -> Result<(), CliError> {
    if name.is_empty() {
        return Err(CliError::InvalidName(input.to_string()));
    }
    // A scoped name must be `@scope/name`, with both halves non-empty.
    if let Some(rest) = name.strip_prefix('@') {
        match rest.split_once('/') {
            Some((scope, pkg)) if !scope.is_empty() && !pkg.is_empty() => {}
            _ => return Err(CliError::InvalidName(input.to_string())),
        }
    }
    Ok(())
}

/// Split a package spec into a name and a version request.
///
/// A leading `@` is part of a scope, not a separator, so the search for the
/// separator starts at index 1 for scoped names. Splitting naively on the
/// first `@` breaks `@types/node` *quietly*, yielding an empty name and a
/// version of `types/node`.
pub fn parse_package_spec(input: &str) -> Result<PackageSpec, CliError> {
    if input.is_empty() {
        return Err(CliError::EmptySpec);
    }
    let search_from = usize::from(input.starts_with('@'));

    match input[search_from..].find('@') {
        Some(offset) => {
            let idx = search_from + offset;
            let name = &input[..idx];
            let version = &input[idx + 1..];
            validate_name(name, input)?;
            if version.is_empty() {
                return Err(CliError::EmptyVersion(input.to_string()));
            }
            Ok(PackageSpec {
                name: name.to_string(),
                version: VersionSpec::Exact(version.to_string()),
            })
        }
        None => {
            validate_name(input, input)?;
            Ok(PackageSpec {
                name: input.to_string(),
                version: VersionSpec::Latest,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_name_resolves_to_latest() {
        let spec = parse_package_spec("lodash").unwrap();
        assert_eq!(spec.name, "lodash");
        assert_eq!(spec.version, VersionSpec::Latest);
    }

    #[test]
    fn name_with_version_is_exact() {
        let spec = parse_package_spec("lodash@4.17.21").unwrap();
        assert_eq!(spec.name, "lodash");
        assert_eq!(spec.version, VersionSpec::Exact("4.17.21".into()));
    }

    #[test]
    fn scoped_name_without_version_resolves_to_latest() {
        let spec = parse_package_spec("@types/node").unwrap();
        assert_eq!(spec.name, "@types/node");
        assert_eq!(spec.version, VersionSpec::Latest);
    }

    #[test]
    fn scoped_name_with_version_splits_on_the_second_at() {
        let spec = parse_package_spec("@types/node@20.1.0").unwrap();
        assert_eq!(spec.name, "@types/node");
        assert_eq!(spec.version, VersionSpec::Exact("20.1.0".into()));
    }

    #[test]
    fn dist_tag_is_treated_as_an_exact_request() {
        let spec = parse_package_spec("react@next").unwrap();
        assert_eq!(spec.version, VersionSpec::Exact("next".into()));
        assert_eq!(spec.version.as_request(), "next");
    }

    #[test]
    fn latest_requests_the_latest_tag() {
        assert_eq!(VersionSpec::Latest.as_request(), "latest");
    }

    #[test]
    fn empty_spec_is_rejected() {
        assert!(matches!(parse_package_spec(""), Err(CliError::EmptySpec)));
    }

    #[test]
    fn empty_version_is_rejected() {
        assert!(matches!(
            parse_package_spec("lodash@"),
            Err(CliError::EmptyVersion(_))
        ));
    }

    #[test]
    fn bare_at_sign_is_rejected() {
        assert!(matches!(
            parse_package_spec("@"),
            Err(CliError::InvalidName(_))
        ));
    }

    #[test]
    fn scope_without_a_package_name_is_rejected() {
        assert!(matches!(
            parse_package_spec("@scope"),
            Err(CliError::InvalidName(_))
        ));
    }
}
