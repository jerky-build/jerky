use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use thiserror::Error;

use crate::resolver::Kind;

/// The manifest section a `Kind` names.
///
/// One place that spells these, so a third `Kind` cannot be added and write
/// to a section this module never heard of.
fn section_for(kind: Kind) -> &'static str {
    match kind {
        Kind::Prod => "dependencies",
        Kind::Dev => "devDependencies",
    }
}

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("no package.json found in {0}")]
    NotFound(PathBuf),
    #[error("package.json already exists at {0}")]
    AlreadyExists(PathBuf),
    #[error("package.json at {path} is not valid JSON")]
    Malformed {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("package.json at {0} must contain a JSON object")]
    NotAnObject(PathBuf),
    #[error("could not determine a package name from {0}")]
    UnnameableDirectory(PathBuf),
    #[error("failed to access {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// An order-preserving view of a `package.json`.
///
/// Backed by `serde_json::Map`, which is an `IndexMap` because the crate is
/// built with the `preserve_order` feature. Without that feature this type
/// would silently reshuffle the user's file on every save.
#[derive(Debug)]
pub struct Manifest {
    path: PathBuf,
    value: Map<String, Value>,
}

impl Manifest {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(project_dir: &Path) -> Result<Self, ManifestError> {
        let path = project_dir.join("package.json");
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Err(ManifestError::NotFound(project_dir.to_path_buf()));
            }
            Err(source) => return Err(ManifestError::Io { path, source }),
        };

        let value: Value =
            serde_json::from_str(&raw).map_err(|source| ManifestError::Malformed {
                path: path.clone(),
                source,
            })?;

        match value {
            Value::Object(value) => Ok(Self { path, value }),
            _ => Err(ManifestError::NotAnObject(path)),
        }
    }

    pub fn create_default(project_dir: &Path) -> Result<Self, ManifestError> {
        let path = project_dir.join("package.json");
        // `symlink_metadata` rather than `exists`: `exists` follows symlinks and
        // reports false for a dangling one, so a `package.json` symlinked at a
        // missing target would look absent here and then be written *through*,
        // landing the manifest outside the project. Anything at this path at
        // all, of any kind, means refuse.
        if path.symlink_metadata().is_ok() {
            return Err(ManifestError::AlreadyExists(path));
        }

        let name = project_dir
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| ManifestError::UnnameableDirectory(project_dir.to_path_buf()))?;

        let mut value = Map::new();
        value.insert("name".into(), Value::String(name.to_string()));
        value.insert("version".into(), Value::String("1.0.0".into()));
        value.insert("main".into(), Value::String("index.js".into()));
        value.insert("scripts".into(), Value::Object(Map::new()));
        value.insert("license".into(), Value::String("ISC".into()));

        Ok(Self { path, value })
    }

    pub fn name(&self) -> Option<&str> {
        self.value.get("name").and_then(Value::as_str)
    }

    pub fn version(&self) -> Option<&str> {
        self.value.get("version").and_then(Value::as_str)
    }

    /// The `workspaces` patterns this manifest declares.
    ///
    /// Empty when the field is absent, which is the degenerate case rather
    /// than a separate one: a manifest without `workspaces` is a workspace of
    /// one, and callers need no branch to see it that way.
    pub fn workspaces(&self) -> Vec<String> {
        self.value
            .get("workspaces")
            .and_then(Value::as_array)
            .map(|patterns| {
                patterns
                    .iter()
                    .filter_map(|pattern| pattern.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The `dependencies` this manifest declares, specifier verbatim.
    ///
    /// `BTreeMap` because these seed the resolver, and the resolver's output
    /// reaches the lockfile: iteration order is serialization order, and two
    /// machines must agree. Specifiers are returned exactly as written —
    /// `workspace:*` is a protocol the resolver keys on, and normalizing here
    /// would also make staleness undetectable once the lockfile compares what
    /// was recorded against what is declared.
    pub fn dependencies(&self) -> BTreeMap<String, String> {
        self.section("dependencies")
    }

    /// The `devDependencies` this manifest declares, specifier verbatim.
    ///
    /// Read only from a *workspace member's* manifest, which is what this type
    /// always describes. A registry package's dev dependencies must never be
    /// followed — doing so pulls in most of the registry — and that is
    /// enforced by `VersionMetadata` having no field to read them from, not by
    /// anything here. The two sections are separate reads rather than one
    /// merged map because the caller is the one that knows what it wants: the
    /// resolver wants both, and a production install wants only the first.
    pub fn dev_dependencies(&self) -> BTreeMap<String, String> {
        self.section("devDependencies")
    }

    /// One `name -> specifier` section of the manifest, or nothing when the
    /// field is absent or is not an object.
    fn section(&self, field: &str) -> BTreeMap<String, String> {
        self.value
            .get(field)
            .and_then(Value::as_object)
            .map(|deps| {
                deps.iter()
                    .filter_map(|(name, spec)| {
                        spec.as_str().map(|spec| (name.clone(), spec.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Record `name` at `version` in the section `kind` names, leaving every
    /// other section exactly as it was found.
    ///
    /// Deliberately not a move. A name declared in *both* sections is a
    /// contradiction jerky reconciles for the resolver with a prod-wins rule
    /// and does not resolve in the file, for the reason `declared_by` gives:
    /// the manifest in front of jerky may belong to a member of someone
    /// else's monorepo or be generated by a tool, so it may not be the user's
    /// to edit. `jerky install lodash@4.18.0` is a version change, and a
    /// version change that also deleted a `devDependencies` entry the user
    /// never mentioned would be jerky arbitrating a duplicate it was not
    /// asked about.
    ///
    /// Which section is asked for is the caller's answer and deliberately not
    /// this type's to infer. A plain install passes the kind already
    /// declared, so this writes where the name already lives.
    pub fn add_dependency(&mut self, name: &str, version: &str, kind: Kind) {
        let deps = self
            .value
            .entry(section_for(kind))
            .or_insert_with(|| Value::Object(Map::new()));

        if !deps.is_object() {
            *deps = Value::Object(Map::new());
        }

        deps.as_object_mut()
            .expect("the section was just forced to an object")
            .insert(name.to_string(), Value::String(version.to_string()));
    }

    /// Record `name` at `version` under `kind`, and take it out of the other
    /// section.
    ///
    /// The removal is what makes this a move rather than a second
    /// declaration. Writing to one section without clearing the other leaves
    /// a name declared in both, and nothing ever takes it out again: the
    /// prod-wins rule that reconciles the two for the resolver masks the
    /// duplicate rather than resolving it, so the manifest never settles even
    /// though the lockfile does.
    ///
    /// Reached only from `--save-dev`, which is a statement about the section
    /// and the one thing that outranks what the manifest currently says. That
    /// is what makes the removal legitimate here and not in
    /// [`Manifest::add_dependency`]: the user named the section, so moving
    /// the name into it is the instruction rather than a guess about a
    /// duplicate nobody mentioned.
    pub fn move_dependency(&mut self, name: &str, version: &str, kind: Kind) {
        let vacated = match kind {
            Kind::Prod => section_for(Kind::Dev),
            Kind::Dev => section_for(Kind::Prod),
        };

        self.undeclare(vacated, name);
        self.add_dependency(name, version, kind);
    }

    /// Take `name` out of `section`, and the section out of the manifest if
    /// that leaves it empty.
    ///
    /// An emptied section is dropped rather than left behind as
    /// `"dependencies": {}`, because the object jerky is deleting the last
    /// entry from is one jerky wrote in the first place. A section the user
    /// wrote empty is never reached: there is nothing in it to remove, so the
    /// early return fires before the question of deleting it comes up.
    ///
    /// `shift_remove` rather than `remove`, in both places. With
    /// `preserve_order` enabled — which is the whole reason this type is
    /// backed by a `Map` — `remove` is `swap_remove`, so taking a dependency
    /// out of the middle would silently move the file's last key into the hole
    /// and reorder a user's `package.json` around an edit they did not make.
    fn undeclare(&mut self, section: &str, name: &str) {
        let Some(deps) = self.value.get_mut(section).and_then(Value::as_object_mut) else {
            return;
        };
        if deps.shift_remove(name).is_none() {
            return;
        }
        if deps.is_empty() {
            self.value.shift_remove(section);
        }
    }

    pub fn save(&self) -> Result<(), ManifestError> {
        let mut serialized = serde_json::to_string_pretty(&Value::Object(self.value.clone()))
            .expect("a JSON object is always serializable");
        serialized.push('\n');

        std::fs::write(&self.path, serialized).map_err(|source| ManifestError::Io {
            path: self.path.clone(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(dir: &Path, contents: &str) {
        std::fs::write(dir.join("package.json"), contents).unwrap();
    }

    #[test]
    fn create_default_uses_the_directory_name() {
        let dir = TempDir::new().unwrap();
        let project = dir.path().join("my-app");
        std::fs::create_dir(&project).unwrap();

        let manifest = Manifest::create_default(&project).unwrap();
        manifest.save().unwrap();

        let raw = std::fs::read_to_string(project.join("package.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["name"], "my-app");
        assert_eq!(parsed["version"], "1.0.0");
        assert_eq!(parsed["main"], "index.js");
        assert_eq!(parsed["license"], "ISC");
        assert!(parsed["scripts"].is_object());
    }

    #[test]
    fn create_default_refuses_to_overwrite() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), r#"{"name":"existing"}"#);

        assert!(matches!(
            Manifest::create_default(dir.path()),
            Err(ManifestError::AlreadyExists(_))
        ));
    }

    #[test]
    fn create_default_refuses_a_symlinked_manifest() {
        let dir = TempDir::new().unwrap();
        let outside = dir.path().join("outside.json");
        let project = dir.path().join("proj");
        std::fs::create_dir(&project).unwrap();

        // A dangling symlink: `exists()` follows it, finds no target, and
        // reports false. Writing through it would land the manifest at the
        // link target, outside the project directory entirely.
        std::os::unix::fs::symlink(&outside, project.join("package.json")).unwrap();

        assert!(matches!(
            Manifest::create_default(&project),
            Err(ManifestError::AlreadyExists(_))
        ));
        assert!(!outside.exists(), "must not write through the symlink");
    }

    #[test]
    fn load_reports_a_missing_manifest() {
        let dir = TempDir::new().unwrap();
        assert!(matches!(
            Manifest::load(dir.path()),
            Err(ManifestError::NotFound(_))
        ));
    }

    #[test]
    fn load_reports_malformed_json() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "{ not json");
        assert!(matches!(
            Manifest::load(dir.path()),
            Err(ManifestError::Malformed { .. })
        ));
    }

    #[test]
    fn round_trip_preserves_unknown_fields_and_key_order() {
        let dir = TempDir::new().unwrap();
        // Byte-identical to what `to_string_pretty` emits, so a byte comparison
        // isolates ordering and field retention from whitespace. Arrays are
        // expanded one element per line because that is the only form the
        // serializer produces; re-indenting to match the input file is #24.
        let original = concat!(
            "{\n",
            "  \"name\": \"demo\",\n",
            "  \"zzz\": \"last\",\n",
            "  \"aaa\": [\n",
            "    1,\n",
            "    2\n",
            "  ],\n",
            "  \"version\": \"1.0.0\"\n",
            "}\n",
        );
        write(dir.path(), original);

        let manifest = Manifest::load(dir.path()).unwrap();
        manifest.save().unwrap();

        let after = std::fs::read_to_string(dir.path().join("package.json")).unwrap();
        assert_eq!(after, original, "save() must not reorder or drop fields");
    }

    #[test]
    fn add_dependency_inserts_into_a_new_dependencies_object() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "{\n  \"name\": \"demo\"\n}\n");

        let mut manifest = Manifest::load(dir.path()).unwrap();
        manifest.add_dependency("lodash", "4.17.21", Kind::Prod);
        manifest.save().unwrap();

        let raw = std::fs::read_to_string(dir.path().join("package.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["dependencies"]["lodash"], "4.17.21");
        assert_eq!(parsed["name"], "demo");
    }

    #[test]
    fn add_dependency_replaces_an_existing_entry() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), r#"{"dependencies":{"lodash":"3.0.0"}}"#);

        let mut manifest = Manifest::load(dir.path()).unwrap();
        manifest.add_dependency("lodash", "4.17.21", Kind::Prod);
        manifest.save().unwrap();

        let raw = std::fs::read_to_string(dir.path().join("package.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["dependencies"]["lodash"], "4.17.21");
    }

    #[test]
    fn add_dependency_leaves_a_duplicate_in_the_other_section_alone() {
        // A plain `jerky install lodash@4.18.0` against a manifest that
        // declares lodash in both sections is a version change, and nothing
        // more. Removing the other declaration would settle a contradiction
        // the user never asked about — and the manifest may not be theirs to
        // edit, which is the same reason `declared_by` masks the duplicate
        // rather than refusing the file. Only `--save-dev` moves a name, and
        // it goes through `move_dependency` to do it.
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            r#"{"dependencies":{"lodash":"4.17.21"},"devDependencies":{"lodash":"3.0.0"}}"#,
        );

        let mut manifest = Manifest::load(dir.path()).unwrap();
        manifest.add_dependency("lodash", "4.18.0", Kind::Prod);
        manifest.save().unwrap();

        let raw = std::fs::read_to_string(dir.path().join("package.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["dependencies"]["lodash"], "4.18.0");
        assert_eq!(
            parsed["devDependencies"]["lodash"], "3.0.0",
            "a version change must not delete a declaration it was not asked about: {parsed}"
        );
    }

    #[test]
    fn move_dependency_takes_the_name_out_of_the_section_it_left() {
        // The manifest half of `--save-dev`. Leaving the old entry behind
        // would declare lodash twice, which nothing downstream ever undoes.
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            r#"{"dependencies":{"alpha":"1.0.0","lodash":"4.17.21"}}"#,
        );

        let mut manifest = Manifest::load(dir.path()).unwrap();
        manifest.move_dependency("lodash", "4.17.21", Kind::Dev);
        manifest.save().unwrap();

        let raw = std::fs::read_to_string(dir.path().join("package.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["devDependencies"]["lodash"], "4.17.21");
        assert!(parsed["dependencies"]["lodash"].is_null());
        assert_eq!(
            parsed["dependencies"]["alpha"], "1.0.0",
            "the rest of the section it left is not jerky's to touch"
        );
    }

    #[test]
    fn a_section_emptied_by_a_move_is_dropped() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), r#"{"dependencies":{"lodash":"4.17.21"}}"#);

        let mut manifest = Manifest::load(dir.path()).unwrap();
        manifest.move_dependency("lodash", "4.17.21", Kind::Dev);
        manifest.save().unwrap();

        let raw = std::fs::read_to_string(dir.path().join("package.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(
            parsed["dependencies"].is_null(),
            "an empty object jerky wrote is jerky's to take away: {parsed}"
        );
    }

    #[test]
    fn dependencies_are_read_back_as_declared() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"web","dependencies":{"lodash":"^4.17.21","ui":"workspace:*"}}"#,
        )
        .unwrap();

        let deps = Manifest::load(dir.path()).unwrap().dependencies();

        // The specifier is carried through verbatim. Normalizing here would
        // lose the `workspace:` protocol the resolver keys on, and would make
        // staleness undetectable once the lockfile compares the two.
        assert_eq!(deps["lodash"], "^4.17.21");
        assert_eq!(deps["ui"], "workspace:*");
    }

    #[test]
    fn the_two_sections_are_read_separately() {
        // A name in both is not this type's problem to settle: it reports each
        // section as written, and the caller building the resolver's input is
        // where the two are reconciled.
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"web",
                "dependencies":{"lodash":"^4.17.21"},
                "devDependencies":{"vitest":"^1.0.0","lodash":"^3.0.0"}}"#,
        )
        .unwrap();

        let manifest = Manifest::load(dir.path()).unwrap();

        assert_eq!(manifest.dependencies().len(), 1);
        assert_eq!(manifest.dependencies()["lodash"], "^4.17.21");
        assert_eq!(manifest.dev_dependencies()["vitest"], "^1.0.0");
        assert_eq!(manifest.dev_dependencies()["lodash"], "^3.0.0");
    }

    #[test]
    fn a_manifest_without_dependencies_declares_none() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("package.json"), r#"{"name":"web"}"#).unwrap();

        let manifest = Manifest::load(dir.path()).unwrap();
        assert!(manifest.dependencies().is_empty());
        assert!(manifest.dev_dependencies().is_empty());
    }
}
