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
    /// Install what the workspace declares, or add one package to it
    Install {
        /// Package to add, e.g. `lodash` or `lodash@4.17.21`. Omit it to
        /// install what every importer's package.json already declares.
        spec: Option<String>,
        // `requires` rather than a flag that is quietly ignored on its own:
        // `jerky install --save-dev` names no package to record anywhere, and
        // performing a bare install instead would answer a question nobody
        // asked. Rationale stays out of the doc comment because clap prints
        // that one to the user.
        /// Record the package under devDependencies rather than dependencies
        #[arg(long, short = 'D', requires = "spec", conflicts_with = "production")]
        save_dev: bool,
        // `conflicts_with_all` rather than a check inside the install: the two
        // halves contradict each other — one reproduces a lockfile exactly,
        // the other changes it — and the worst place to discover that is after
        // half a tree is linked. No `--omit=dev` alias; one spelling per
        // concept until someone asks.
        /// Install dependencies only, from a lockfile that must already match
        #[arg(long, conflicts_with_all = ["spec", "save_dev"])]
        production: bool,
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
    fn install_accepts_no_spec_at_all() {
        // The post-clone command. `spec` being required is what has meant
        // there was no way to say *install what this repo declares*.
        let cli = Cli::parse_from(["jerky", "install"]);
        assert!(matches!(cli.command, Command::Install { spec: None, .. }));
    }

    #[test]
    fn install_still_accepts_a_spec() {
        let cli = Cli::parse_from(["jerky", "install", "lodash@4.17.21"]);
        assert!(
            matches!(cli.command, Command::Install { spec: Some(ref s), .. } if s == "lodash@4.17.21")
        );
    }

    #[test]
    fn the_short_flag_does_the_same_thing() {
        // `-D` is universal muscle memory, and the only difference between it
        // working and it being a parse error is one `short` attribute. Asked
        // of clap rather than of an install, because what could go wrong here
        // is the spelling and nothing below it.
        let long = Cli::parse_from(["jerky", "install", "--save-dev", "lodash"]);
        let short = Cli::parse_from(["jerky", "install", "-D", "lodash"]);

        for cli in [long, short] {
            let Command::Install { spec, save_dev, .. } = cli.command else {
                panic!("expected an install");
            };
            assert_eq!(spec.as_deref(), Some("lodash"));
            assert!(save_dev);
        }
    }

    #[test]
    fn a_plain_install_asks_for_no_section() {
        let cli = Cli::parse_from(["jerky", "install", "lodash"]);
        assert!(matches!(
            cli.command,
            Command::Install {
                save_dev: false,
                ..
            }
        ));
    }

    #[test]
    fn production_conflicts_with_a_package_argument() {
        // Rejected by clap at parse time rather than partway through an
        // install. `jerky install --production lodash` is a contradiction —
        // one half reproduces a lockfile exactly, the other changes it — and
        // the worst place to discover that is after half a tree is linked.
        assert!(Cli::try_parse_from(["jerky", "install", "--production", "lodash"]).is_err());
    }

    #[test]
    fn production_and_save_dev_together_are_refused() {
        // Over-determined, and deliberately named for what it proves rather
        // than for the constraint it looks like it tests. Every vector that
        // sets both flags is already refused by the `spec` rules — with a
        // package, `--production` conflicts with it; without one, `--save-dev`
        // requires it — so this cannot isolate the `--production`/`--save-dev`
        // conflict, and a test claiming to would pass with that conflict
        // deleted. The conflict is declared anyway, because §7 asks for it and
        // because it is what keeps the combination refused if `--save-dev`
        // ever gains a meaning without a package.
        assert!(
            Cli::try_parse_from(["jerky", "install", "--production", "--save-dev", "lodash"])
                .is_err()
        );
        assert!(Cli::try_parse_from(["jerky", "install", "--production", "--save-dev"]).is_err());
    }

    #[test]
    fn production_on_its_own_parses() {
        let cli = Cli::parse_from(["jerky", "install", "--production"]);
        assert!(matches!(
            cli.command,
            Command::Install {
                spec: None,
                production: true,
                ..
            }
        ));
    }

    #[test]
    fn save_dev_with_nothing_to_save_is_refused() {
        // The alternative is a bare install that silently ignores the flag.
        assert!(Cli::try_parse_from(["jerky", "install", "--save-dev"]).is_err());
    }

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
