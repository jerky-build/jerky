//! A package's `bin` field: what it published, and what jerky is willing to
//! link from it.
//!
//! Two shapes arrive here and one leaves. npm documents `bin` as either an
//! object of name-to-path or a bare string taking the package's own name, and
//! [`Declared`] parses both; everything downstream sees [`Bins`], the object
//! form, because the string form is a spelling rather than a distinction.
//!
//! The validation is the reason this is a module rather than a type alias.
//! `bin` is published by whoever published the package, both halves of every
//! entry become a path, and neither is jerky's. See
//! `docs/specs/2026-09-16-bin-linking-design.md` §4.

use std::collections::BTreeMap;
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

/// A package's bins: the name each takes inside a `.bin` directory -> the file
/// inside the package it points at, as a validated relative path.
pub type Bins = BTreeMap<String, String>;

/// `bin` in either shape npm documents.
///
/// Parsed rather than assumed to be an object because of what a parse failure
/// costs: [`crate::registry::Packument`] holds its versions in a map, so one
/// version serde refuses takes the whole packument with it and makes the
/// package unresolvable at *every* version. The string form appears to be
/// normalized away by the registry — no version of `coffee-script`,
/// `uglify-js`, `browserify`, `nodemon`, `jshint` or `mocha` carries one in
/// the abbreviated packument — so this is insurance rather than a code path
/// with a known caller, and it is cheap enough to be worth carrying anyway.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Declared {
    /// `"bin": "./cli.js"` — one bin, named for the package.
    One(String),
    /// `"bin": { "tsc": "bin/tsc" }`.
    Many(BTreeMap<String, String>),
}

/// Read a `bin` field, treating a shape jerky does not recognise as no `bin`
/// at all.
///
/// The leniency is not politeness, it is blast radius.
/// [`crate::registry::Packument`] holds every published version in one map, so
/// a version serde refuses takes the *package* down with it — at every
/// version, for every project that depends on it. npm has accepted more shapes
/// here over the years than it documents today, and the registry serves what
/// was published. Against that, the cost of quietly ignoring one is a missing
/// shim for one version of one package.
///
/// Deliberately narrow: it makes `bin` alone tolerant rather than
/// `VersionMetadata` as a whole. A malformed `dist` or a malformed
/// `dependencies` block is a version jerky genuinely cannot install, and
/// dropping the field there would install a wrong tree rather than none.
pub fn deserialize_lenient<'de, D>(deserializer: D) -> Result<Option<Declared>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).ok())
}

impl Declared {
    /// The bins this declares, keyed by the name each takes in a `.bin`
    /// directory, with everything jerky will not link dropped.
    ///
    /// `pkg_name` is the package's own name, which only the string form needs:
    /// `"bin": "./bin/babel.js"` published by `@babel/cli` means a bin called
    /// `cli`, because npm strips the scope. Passing it always keeps the caller
    /// from having to know which form it is holding.
    ///
    /// A declaration that fails validation is dropped and the rest survive,
    /// which is the call [`crate::registry::Packument::versions_sorted`]
    /// already makes about a malformed version: one bad entry must not make an
    /// otherwise-installable package refuse to install. The cost of dropping
    /// is a missing shim; the cost of not dropping is a link jerky had no
    /// business writing.
    pub fn named_for(&self, pkg_name: &str) -> Bins {
        match self {
            Declared::One(target) => accept(unscoped(pkg_name), target).into_iter().collect(),
            Declared::Many(declared) => declared
                .iter()
                .filter_map(|(name, target)| accept(name, target))
                .collect(),
        }
    }
}

/// `@babel/cli` -> `cli`, and an unscoped name unchanged.
///
/// npm's rule for the string form. A name with no `/` is returned whole, so
/// this is safe on anything — including a name that is not a package name at
/// all, which validation below is what actually catches.
fn unscoped(pkg_name: &str) -> &str {
    match pkg_name.rsplit_once('/') {
        Some((_, last)) => last,
        None => pkg_name,
    }
}

/// One declaration, if jerky is willing to link it.
///
/// The name must be a single path component, because it is joined onto a
/// `.bin` directory and `{"../../evil": "x"}` would otherwise plant a link
/// outside it. The target must stay inside the package, because a `.bin` entry
/// aimed at `../../../../etc/cron.daily/x` names a file the package does not
/// own — and jerky raises the execute bit on whatever a bin target turns out
/// to be.
fn accept(name: &str, target: &str) -> Option<(String, String)> {
    Some((single_component(name)?.to_string(), contained(target)?))
}

/// `name`, if it is one ordinary path component and nothing else.
///
/// `Component::Normal` is the whole check: it excludes the empty string, `.`,
/// `..`, anything absolute, and anything with a separator in it, because each
/// of those parses as a different component or as more than one.
fn single_component(name: &str) -> Option<&str> {
    let mut components = Path::new(name).components();
    let Some(Component::Normal(only)) = components.next() else {
        return None;
    };
    if components.next().is_some() {
        return None;
    }
    only.to_str()
}

/// `target` as a relative path that stays inside the package, cleaned of the
/// `./` prefix real packages publish.
///
/// Lexical, like every other path comparison in jerky: the package is not on
/// disk yet when a plan is built, and a check that needed it to be would be a
/// check that could not run at planning time. Climbing is refused rather than
/// resolved — a `..` that a later component would have cancelled out is still
/// a declaration jerky declines to interpret.
///
/// A target that cleans away to nothing at all is refused too. `"bin": "."`
/// names the package directory, which is not a file to run.
fn contained(target: &str) -> Option<String> {
    let mut cleaned = std::path::PathBuf::new();
    for component in Path::new(target).components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => cleaned.push(part),
            // RootDir, Prefix and ParentDir all name something outside the
            // package: the first two absolutely, the third by climbing.
            _ => return None,
        }
    }

    cleaned
        .to_str()
        .filter(|path| !path.is_empty())
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn many(pairs: &[(&str, &str)]) -> Declared {
        Declared::Many(
            pairs
                .iter()
                .map(|(name, target)| (name.to_string(), target.to_string()))
                .collect(),
        )
    }

    #[test]
    fn the_object_form_is_taken_as_published() {
        let bins =
            many(&[("tsc", "bin/tsc"), ("tsserver", "bin/tsserver")]).named_for("typescript");

        assert_eq!(bins["tsc"], "bin/tsc");
        assert_eq!(bins["tsserver"], "bin/tsserver");
        assert_eq!(bins.len(), 2);
    }

    #[test]
    fn the_string_form_takes_the_package_name_with_any_scope_stripped() {
        // npm's rule, and the reason `named_for` takes the name at all.
        assert_eq!(
            Declared::One("./bin/babel.js".into()).named_for("@babel/cli"),
            [("cli".to_string(), "bin/babel.js".to_string())].into()
        );

        assert_eq!(
            Declared::One("cli.js".into()).named_for("rimraf"),
            [("rimraf".to_string(), "cli.js".to_string())].into()
        );
    }

    #[test]
    fn a_leading_dot_slash_is_noise_and_is_cleaned_off() {
        // Both spellings are live on the registry — `typescript@5.3.3` says
        // `bin/tsc` and `typescript@0.8.0` says `./bin/tsc` — and they name
        // the same file. Cleaning here is what keeps the two from producing
        // two different symlink targets for one package.
        let bins = many(&[("tsc", "./bin/tsc")]).named_for("typescript");

        assert_eq!(bins["tsc"], "bin/tsc");
    }

    #[test]
    fn a_name_that_is_not_one_path_component_is_dropped() {
        // Each of these is joined onto a `.bin` directory, and each would put
        // the link somewhere else.
        for name in ["../evil", "a/b", "..", ".", "", "/abs"] {
            let bins = many(&[(name, "cli.js"), ("fine", "cli.js")]).named_for("p");

            assert_eq!(
                bins,
                [("fine".to_string(), "cli.js".to_string())].into(),
                "`{name}` was accepted as a bin name, or took the valid one down with it"
            );
        }
    }

    #[test]
    fn a_target_that_leaves_the_package_is_dropped() {
        // The link would resolve to a file the package does not own, and then
        // jerky would raise the execute bit on it.
        for target in [
            "../../../../etc/cron.daily/x",
            "/etc/passwd",
            "..",
            "a/../../b",
            ".",
            "",
        ] {
            let bins = many(&[("evil", target), ("fine", "cli.js")]).named_for("p");

            assert_eq!(
                bins,
                [("fine".to_string(), "cli.js".to_string())].into(),
                "`{target}` was accepted as a bin target, or took the valid one down with it"
            );
        }
    }

    #[test]
    fn a_deep_target_inside_the_package_is_kept() {
        // Contained, not shallow, is the rule. `dist/esm/bin.mjs` is what
        // `rimraf@6` actually publishes.
        let bins = many(&[("rimraf", "./dist/esm/bin.mjs")]).named_for("rimraf");

        assert_eq!(bins["rimraf"], "dist/esm/bin.mjs");
    }

    #[test]
    fn a_string_form_whose_package_name_is_unusable_is_dropped() {
        // A package cannot be named `..`, so this is unreachable through the
        // registry — but the name arrives from the same untrusted document the
        // target does, and the string form is the one place it becomes a path.
        assert!(Declared::One("cli.js".into()).named_for("..").is_empty());
        assert!(Declared::One("cli.js".into()).named_for("").is_empty());
    }
}
