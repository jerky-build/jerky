use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use thiserror::Error;

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
    ///
    /// `devDependencies` are deliberately absent: only the root project's are
    /// ever followed, and that is spec 3.
    pub fn dependencies(&self) -> BTreeMap<String, String> {
        self.value
            .get("dependencies")
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

    pub fn add_dependency(&mut self, name: &str, version: &str) {
        let deps = self
            .value
            .entry("dependencies")
            .or_insert_with(|| Value::Object(Map::new()));

        if !deps.is_object() {
            *deps = Value::Object(Map::new());
        }

        deps.as_object_mut()
            .expect("dependencies was just forced to an object")
            .insert(name.to_string(), Value::String(version.to_string()));
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
        manifest.add_dependency("lodash", "4.17.21");
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
        manifest.add_dependency("lodash", "4.17.21");
        manifest.save().unwrap();

        let raw = std::fs::read_to_string(dir.path().join("package.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["dependencies"]["lodash"], "4.17.21");
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
    fn a_manifest_without_dependencies_declares_none() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("package.json"), r#"{"name":"web"}"#).unwrap();

        assert!(
            Manifest::load(dir.path())
                .unwrap()
                .dependencies()
                .is_empty()
        );
    }
}
