use std::path::{Path, PathBuf};

use crate::manifest::{Manifest, ManifestError};

/// Write a default `package.json` into `project_dir`.
///
/// Non-interactive by design: `npm init`'s questionnaire is a separate
/// concern. Errors rather than prompting or clobbering if one already exists.
pub fn init(project_dir: &Path) -> Result<PathBuf, ManifestError> {
    let manifest = Manifest::create_default(project_dir)?;
    manifest.save()?;
    Ok(manifest.path().to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn writes_a_manifest_and_returns_its_path() {
        let dir = TempDir::new().unwrap();
        let project = dir.path().join("demo");
        std::fs::create_dir(&project).unwrap();

        let written = init(&project).unwrap();

        assert_eq!(written, project.join("package.json"));
        let raw = std::fs::read_to_string(&written).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["name"], "demo");
    }

    #[test]
    fn refuses_to_overwrite_an_existing_manifest() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("package.json"), r#"{"name":"keep"}"#).unwrap();

        assert!(matches!(
            init(dir.path()),
            Err(crate::manifest::ManifestError::AlreadyExists(_))
        ));

        // The original file is untouched.
        let raw = std::fs::read_to_string(dir.path().join("package.json")).unwrap();
        assert!(raw.contains("keep"));
    }
}
