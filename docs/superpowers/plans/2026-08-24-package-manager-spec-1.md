# Package Manager Spec 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `jerky init` and `jerky install <pkg>[@<version>]` end to end for a dependency-free package, using a pnpm-style content-addressed store.

**Architecture:** A lib + bin split where `main.rs` only parses arguments and renders errors. Leaf modules (`cli`, `manifest`, `integrity`, `archive`) are pure and touch no network or global state; `store` and `linker` own all filesystem mutation and do it atomically via stage-and-rename; `commands` is the only layer that knows about more than one module. Network access sits behind a `RegistryClient` trait so the whole install pipeline can be tested offline against a fixture.

**Tech Stack:** Rust 2024, clap (derive), serde_json with `preserve_order`, ureq, flate2, tar, sha2, sha1, tempfile, thiserror.

**Spec:** `docs/superpowers/specs/2026-08-24-package-manager-design.md`

## Global Constraints

- **Rust edition 2024**, which requires toolchain **1.85 or newer**.
- **Platforms: Linux and macOS only.** Unix-only APIs (`std::os::unix`) are acceptable. Do not add Windows branches.
- **Store keys are lowercase.** macOS filesystems are case-insensitive by default; a mixed-case key can collide.
- **Nothing below `main.rs` may read `$HOME` or the current directory.** All paths are function parameters. This is a correctness requirement for tests, not a style preference.
- **Never record a dependency in `package.json` that is not already on disk.** The manifest write is always the last step.
- **Record exact versions, never caret ranges.** Spec 1 has no range resolution; a caret would be a constraint jerky cannot honour.
- **The version written to `package.json` comes from the registry response, never from the CLI input.**
- **Integrity is verified on a complete in-memory buffer before any byte is extracted.** Never stream-extract-while-hashing.
- **Staging directories always live inside the destination's own root** so `rename` never crosses a filesystem.
- **Every task ends with `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` passing.**

---

## File Structure

| File | Responsibility |
|---|---|
| `Cargo.toml` | Dependencies, lib + bin targets |
| `src/main.rs` | Arg parsing, error rendering, exit codes. The only place that reads `$HOME`/cwd. |
| `src/lib.rs` | Module declarations |
| `src/cli.rs` | clap definitions, `PackageSpec` parsing |
| `src/error.rs` | Top-level `JerkyError` |
| `src/manifest.rs` | `package.json` read/write, order-preserving |
| `src/integrity.rs` | SSRI parsing, digest verification, store keys |
| `src/archive.rs` | gzip + tar extraction, hostile-input rejection |
| `src/registry.rs` | `RegistryClient` trait, `VersionMetadata`, `HttpRegistry` |
| `src/store.rs` | Content-addressed store, atomic commit |
| `src/linker.rs` | Hard links, symlinks, EXDEV fallback |
| `src/commands/mod.rs` | Command module declarations |
| `src/commands/init.rs` | `jerky init` |
| `src/commands/install.rs` | `jerky install` orchestration |
| `src/testing.rs` | Tarball builder and `FixtureRegistry`, shared by unit and integration tests |
| `tests/install.rs` | End-to-end install tests against the fixture registry |
| `.github/workflows/ci.yml` | Ubuntu + macOS matrix |

**Note on `src/testing.rs`:** it is compiled into the library unconditionally rather than behind `#[cfg(test)]`. Rust integration tests in `tests/` cannot see a parent crate's `#[cfg(test)]` items, and the alternatives (a `test-support` feature with a self-referential dev-dependency, or duplicating the tarball builder) are worse. The cost is a few dozen unused bytes in the release binary.

---

### Task 1: Project skeleton and CLI parsing

**Files:**
- Modify: `Cargo.toml`
- Create: `src/lib.rs`, `src/cli.rs`, `src/error.rs`, `.github/workflows/ci.yml`
- Modify: `src/main.rs`
- Test: inline `#[cfg(test)]` module in `src/cli.rs`

**Interfaces:**
- Consumes: nothing
- Produces: `cli::VersionSpec` (`Latest` | `Exact(String)`), `cli::PackageSpec { name: String, version: VersionSpec }`, `cli::parse_package_spec(&str) -> Result<PackageSpec, CliError>`, `VersionSpec::as_request(&self) -> &str`, `error::JerkyError`

- [ ] **Step 1: Replace `Cargo.toml`**

```toml
[package]
name = "jerky"
version = "0.1.0"
edition = "2024"

[lib]
name = "jerky"
path = "src/lib.rs"

[[bin]]
name = "jerky"
path = "src/main.rs"

[dependencies]
base64 = "0.22"
clap = { version = "4", features = ["derive"] }
dirs = "5"
flate2 = "1"
serde = { version = "1", features = ["derive"] }
serde_json = { version = "1", features = ["preserve_order"] }
sha1 = "0.10"
sha2 = "0.10"
tar = "0.4"
thiserror = "2"
ureq = { version = "2", features = ["json"] }

[dev-dependencies]
tempfile = "3"
tiny_http = "0.12"
```

`tempfile` is a dev-dependency only. `store.rs` and `linker.rs` hand-roll their
own staging directories (Task 6) because a staging directory must be *moved* on
success, and `TempDir`'s API for disabling its cleanup has churned across
versions. Tests still use `TempDir` freely, and `#[cfg(test)]` code can see
dev-dependencies.

- [ ] **Step 2: Write the failing test**

Create `src/cli.rs` containing only this test module for now:

```rust
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
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test --lib cli`
Expected: FAIL — compile error, `parse_package_spec` not found.

- [ ] **Step 4: Write the implementation**

Prepend to `src/cli.rs`, above the test module:

```rust
use clap::{Parser, Subcommand};
use thiserror::Error;

#[derive(Debug, Parser)]
#[command(name = "jerky", version, about = "A JavaScript package manager and build tool")]
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
```

- [ ] **Step 5: Create `src/error.rs`**

```rust
use thiserror::Error;

use crate::cli::CliError;

#[derive(Debug, Error)]
pub enum JerkyError {
    #[error(transparent)]
    Cli(#[from] CliError),
    #[error("`jerky {0}` is not implemented yet")]
    NotImplemented(&'static str),
}
```

`NotImplemented` is temporary scaffolding. Task 3 removes the `init` arm and Task 10 removes the variant entirely.

- [ ] **Step 6: Create `src/lib.rs`**

```rust
pub mod cli;
pub mod error;
```

- [ ] **Step 7: Replace `src/main.rs`**

```rust
use std::process::ExitCode;

use clap::Parser;
use jerky::cli::{Cli, Command};
use jerky::error::JerkyError;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            let mut source = std::error::Error::source(&err);
            while let Some(cause) = source {
                eprintln!("  caused by: {cause}");
                source = cause.source();
            }
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), JerkyError> {
    match cli.command {
        Command::Init => Err(JerkyError::NotImplemented("init")),
        Command::Install { .. } => Err(JerkyError::NotImplemented("install")),
    }
}
```

- [ ] **Step 8: Run the tests to verify they pass**

Run: `cargo test --lib cli`
Expected: PASS, 10 tests.

- [ ] **Step 9: Create `.github/workflows/ci.yml`**

```yaml
name: CI
on: [push, pull_request]

jobs:
  test:
    strategy:
      fail-fast: false
      matrix:
        os: [ubuntu-latest, macos-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy, rustfmt
      - run: cargo fmt --check
      - run: cargo clippy --all-targets -- -D warnings
      - run: cargo test
```

The macOS leg is what catches a case-insensitivity bug in store keys, which is invisible on Linux.

- [ ] **Step 10: Verify the whole suite is clean**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: all PASS.

- [ ] **Step 11: Commit**

```bash
git add Cargo.toml Cargo.lock src/ .github/
git commit -m "feat: project skeleton and package spec parsing"
```

---

### Task 2: Manifest read and write

**Files:**
- Create: `src/manifest.rs`
- Modify: `src/lib.rs`, `src/error.rs`
- Test: inline `#[cfg(test)]` module in `src/manifest.rs`

**Interfaces:**
- Consumes: nothing
- Produces: `manifest::Manifest`, `Manifest::load(&Path) -> Result<Manifest, ManifestError>`, `Manifest::create_default(&Path) -> Result<Manifest, ManifestError>`, `Manifest::add_dependency(&mut self, &str, &str)`, `Manifest::save(&self) -> Result<(), ManifestError>`, `Manifest::path(&self) -> &Path`, `manifest::ManifestError`

- [ ] **Step 1: Write the failing test**

Create `src/manifest.rs` with this test module:

```rust
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
        // Already 2-space pretty-printed, so a byte comparison is meaningful.
        let original = "{\n  \"name\": \"demo\",\n  \"zzz\": \"last\",\n  \"aaa\": [1, 2],\n  \"version\": \"1.0.0\"\n}\n";
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
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib manifest`
Expected: FAIL — compile error, `Manifest` not found.

- [ ] **Step 3: Write the implementation**

Prepend to `src/manifest.rs`:

```rust
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

        let value: Value = serde_json::from_str(&raw).map_err(|source| ManifestError::Malformed {
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
        if path.exists() {
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
```

- [ ] **Step 4: Register the module**

In `src/lib.rs`, add `pub mod manifest;` after `pub mod error;`.

In `src/error.rs`, add the variant and import:

```rust
use crate::manifest::ManifestError;
```

```rust
    #[error(transparent)]
    Manifest(#[from] ManifestError),
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib manifest`
Expected: PASS, 7 tests.

- [ ] **Step 6: Record the known limitation**

Append to the spec's section 12, `## 12. Open questions`:

```markdown
**`package.json` reformatting.** `Manifest::save` re-serializes with
`serde_json::to_string_pretty`, which always emits two-space indentation. npm
detects and preserves the file's existing indentation. A project using tabs or
four spaces will see its `package.json` reformatted on first install. Not
blocking for spec 1, but worth an issue before jerky touches anyone's real
project.
```

- [ ] **Step 7: Commit**

```bash
git add src/ docs/
git commit -m "feat: order-preserving package.json read and write"
```

---

### Task 3: `jerky init`

**Files:**
- Create: `src/commands/mod.rs`, `src/commands/init.rs`
- Modify: `src/lib.rs`, `src/main.rs`, `src/error.rs`
- Test: inline `#[cfg(test)]` module in `src/commands/init.rs`

**Interfaces:**
- Consumes: `Manifest::create_default`, `Manifest::save`, `Manifest::path`, `ManifestError`
- Produces: `commands::init::init(project_dir: &Path) -> Result<PathBuf, ManifestError>`

- [ ] **Step 1: Write the failing test**

Create `src/commands/init.rs` with:

```rust
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
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib init`
Expected: FAIL — compile error, `init` not found.

- [ ] **Step 3: Write the implementation**

Prepend to `src/commands/init.rs`:

```rust
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
```

- [ ] **Step 4: Create `src/commands/mod.rs`**

```rust
pub mod init;
```

- [ ] **Step 5: Register the module and wire the command**

In `src/lib.rs`, add `pub mod commands;`.

In `src/main.rs`, replace the `run` function:

```rust
fn run(cli: Cli) -> Result<(), JerkyError> {
    match cli.command {
        Command::Init => {
            let project_dir = std::env::current_dir().map_err(JerkyError::Cwd)?;
            let path = jerky::commands::init::init(&project_dir)?;
            println!("wrote {}", path.display());
            Ok(())
        }
        Command::Install { .. } => Err(JerkyError::NotImplemented("install")),
    }
}
```

In `src/error.rs`, add:

```rust
    #[error("could not determine the current directory")]
    Cwd(#[source] std::io::Error),
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --lib init`
Expected: PASS, 2 tests.

- [ ] **Step 7: Verify the command works end to end**

```bash
mkdir -p /tmp/jerky-smoke && cd /tmp/jerky-smoke
cargo run --manifest-path "$OLDPWD/Cargo.toml" -- init
cat package.json
```
Expected: a `package.json` naming the directory `jerky-smoke`. Running `init` a second time must fail with `error: package.json already exists at ...` and exit code 1.

- [ ] **Step 8: Commit**

```bash
git add src/
git commit -m "feat: jerky init"
```

---

### Task 4: Integrity parsing and verification

**Files:**
- Create: `src/integrity.rs`
- Modify: `src/lib.rs`, `src/error.rs`
- Test: inline `#[cfg(test)]` module in `src/integrity.rs`

**Interfaces:**
- Consumes: nothing
- Produces: `integrity::Algo` (`Sha512` | `Sha1`), `integrity::Integrity { algo, digest }`, `Integrity::parse(&str)`, `Integrity::from_shasum_hex(&str)`, `Integrity::store_key(&self) -> String`, `Integrity::verify(&self, &[u8])`, `Integrity::to_ssri(&self) -> String`, `integrity::IntegrityError`

- [ ] **Step 1: Write the failing test**

Create `src/integrity.rs` with:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    // sha512 of the three bytes "abc", base64-encoded, as npm would report it.
    const ABC_SHA512_SSRI: &str = "sha512-3a81oZNherrMQXNJriBBMRLm+k6JqX6iCp7u5ktV05ohkpkqJ0/BqDa6PCOj/uu9RU7XvQFt+DR3B7bLDgnnFQ==";

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
        assert_eq!(key, key.to_lowercase(), "store keys must be case-safe on macOS");
        // "sha512-" plus 64 bytes rendered as two hex chars each.
        assert_eq!(key.len(), 7 + 128);
    }

    #[test]
    fn parses_a_legacy_sha1_shasum() {
        // sha1 of "abc"
        let integrity = Integrity::from_shasum_hex("a9993e364706816aba3e25717850c26c9cd0d89d").unwrap();
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
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib integrity`
Expected: FAIL — compile error, `Integrity` not found.

- [ ] **Step 3: Write the implementation**

Prepend to `src/integrity.rs`:

```rust
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
```

- [ ] **Step 4: Register the module**

In `src/lib.rs`, add `pub mod integrity;`.

In `src/error.rs`, add `use crate::integrity::IntegrityError;` and:

```rust
    #[error(transparent)]
    Integrity(#[from] IntegrityError),
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib integrity`
Expected: PASS, 10 tests.

If `parses_a_sha512_ssri_string` fails on the constant, regenerate it with
`printf 'abc' | openssl dgst -sha512 -binary | base64 -w0` (use `base64` with
no `-w0` on macOS) and update `ABC_SHA512_SSRI`.

- [ ] **Step 6: Commit**

```bash
git add src/
git commit -m "feat: SSRI parsing and integrity verification"
```

---

### Task 5: Archive extraction and the tarball test builder

**Files:**
- Create: `src/archive.rs`, `src/testing.rs`
- Modify: `src/lib.rs`, `src/error.rs`
- Test: inline `#[cfg(test)]` module in `src/archive.rs`

**Interfaces:**
- Consumes: nothing
- Produces: `archive::extract(tarball: &[u8], dest: &Path) -> Result<(), ArchiveError>`, `archive::ArchiveError`, `testing::TarEntry`, `testing::build_tarball(&[TarEntry]) -> Vec<u8>`

- [ ] **Step 1: Create the tarball builder**

Create `src/testing.rs`:

```rust
//! Test helpers shared by unit tests and the integration tests in `tests/`.
//!
//! Compiled unconditionally rather than behind `#[cfg(test)]`: integration
//! tests cannot see a parent crate's test-only items, and the alternatives
//! (a `test-support` feature with a self-referential dev-dependency, or
//! duplicating this builder) are worse for a few dozen bytes of binary.

use std::io::Write as _;

use flate2::Compression;
use flate2::write::GzEncoder;

/// One entry in a generated tarball.
pub enum TarEntry<'a> {
    /// A regular file at `path` with `contents`.
    File { path: &'a str, contents: &'a [u8] },
    /// A symlink at `path` pointing at `target`. Used to build hostile
    /// fixtures that a committed binary tarball could not safely carry.
    Symlink { path: &'a str, target: &'a str },
}

impl<'a> TarEntry<'a> {
    pub fn file(path: &'a str, contents: &'a str) -> Self {
        TarEntry::File {
            path,
            contents: contents.as_bytes(),
        }
    }
}

/// Build a gzipped tar archive in memory.
///
/// Paths are written verbatim, so callers include the leading `package/`
/// component that real npm tarballs carry — and can deliberately omit it, or
/// write `../` escapes, to exercise the rejection paths.
pub fn build_tarball(entries: &[TarEntry<'_>]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());

    for entry in entries {
        match entry {
            TarEntry::File { path, contents } => {
                let mut header = tar::Header::new_gnu();
                header.set_size(contents.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder
                    .append_data(&mut header, path, *contents)
                    .expect("in-memory tar append cannot fail");
            }
            TarEntry::Symlink { path, target } => {
                let mut header = tar::Header::new_gnu();
                header.set_size(0);
                header.set_mode(0o777);
                header.set_entry_type(tar::EntryType::Symlink);
                header
                    .set_link_name(target)
                    .expect("link name is valid");
                header.set_cksum();
                builder
                    .append_data(&mut header, path, std::io::empty())
                    .expect("in-memory tar append cannot fail");
            }
        }
    }

    let tar = builder.into_inner().expect("in-memory tar finish cannot fail");
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(&tar).expect("in-memory gzip cannot fail");
    encoder.finish().expect("in-memory gzip cannot fail")
}
```

- [ ] **Step 2: Write the failing test**

Create `src/archive.rs` with:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{TarEntry, build_tarball};
    use tempfile::TempDir;

    #[test]
    fn strips_the_leading_package_component() {
        let tarball = build_tarball(&[
            TarEntry::file("package/package.json", r#"{"name":"demo"}"#),
            TarEntry::file("package/lib/index.js", "module.exports = 1;"),
        ]);
        let dir = TempDir::new().unwrap();

        extract(&tarball, dir.path()).unwrap();

        assert!(dir.path().join("package.json").is_file());
        assert!(dir.path().join("lib/index.js").is_file());
        assert!(!dir.path().join("package").exists(), "prefix must be stripped");
    }

    #[test]
    fn preserves_file_contents() {
        let tarball = build_tarball(&[TarEntry::file("package/index.js", "hello")]);
        let dir = TempDir::new().unwrap();

        extract(&tarball, dir.path()).unwrap();

        let contents = std::fs::read_to_string(dir.path().join("index.js")).unwrap();
        assert_eq!(contents, "hello");
    }

    #[test]
    fn rejects_parent_directory_escapes() {
        let tarball = build_tarball(&[TarEntry::file("package/../../evil.js", "pwned")]);
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("dest");
        std::fs::create_dir(&dest).unwrap();

        assert!(matches!(
            extract(&tarball, &dest),
            Err(ArchiveError::UnsafePath { .. })
        ));
        assert!(!dir.path().join("evil.js").exists(), "nothing may escape dest");
    }

    #[test]
    fn rejects_absolute_paths() {
        let tarball = build_tarball(&[TarEntry::file("/etc/evil.js", "pwned")]);
        let dir = TempDir::new().unwrap();

        assert!(matches!(
            extract(&tarball, dir.path()),
            Err(ArchiveError::UnsafePath { .. })
        ));
    }

    #[test]
    fn rejects_symlink_entries() {
        let tarball = build_tarball(&[TarEntry::Symlink {
            path: "package/link",
            target: "/etc/passwd",
        }]);
        let dir = TempDir::new().unwrap();

        assert!(matches!(
            extract(&tarball, dir.path()),
            Err(ArchiveError::UnsafePath { .. })
        ));
    }

    #[test]
    fn rejects_an_entry_that_is_only_the_prefix() {
        // A bare `package` entry has nothing left after stripping.
        let tarball = build_tarball(&[TarEntry::file("package", "")]);
        let dir = TempDir::new().unwrap();

        // Stripping yields an empty path, which is skipped rather than an error.
        extract(&tarball, dir.path()).unwrap();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn rejects_garbage_that_is_not_gzip() {
        let dir = TempDir::new().unwrap();
        assert!(matches!(
            extract(b"this is not a gzip stream", dir.path()),
            Err(ArchiveError::Read(_))
        ));
    }
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test --lib archive`
Expected: FAIL — compile error, `extract` not found.

- [ ] **Step 4: Write the implementation**

Prepend to `src/archive.rs`:

```rust
use std::path::{Component, Path, PathBuf};

use flate2::read::GzDecoder;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ArchiveError {
    #[error("could not read the package tarball")]
    Read(#[source] std::io::Error),
    #[error("tarball entry `{entry}` is unsafe: {reason}")]
    UnsafePath { entry: String, reason: &'static str },
    #[error("failed to write {path}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Extract an npm package tarball into `dest`.
///
/// The tarball is untrusted input. Two protections apply:
///
/// 1. The leading `package/` component that npm tarballs carry is stripped,
///    so `dest` receives the package's own root.
/// 2. Any entry whose path escapes `dest` is rejected — parent-directory
///    components, absolute paths, and link entries. This is the tar-slip
///    class of bug, and the one place in spec 1 where a mistake is a security
///    hole rather than a crash.
///
/// Link entries are rejected outright rather than resolved. Some real
/// packages do contain symlinks, so this is a known limitation to revisit
/// when a package that needs them appears.
pub fn extract(tarball: &[u8], dest: &Path) -> Result<(), ArchiveError> {
    let decoder = GzDecoder::new(tarball);
    let mut archive = tar::Archive::new(decoder);

    for entry in archive.entries().map_err(ArchiveError::Read)? {
        let mut entry = entry.map_err(ArchiveError::Read)?;
        let raw = entry.path().map_err(ArchiveError::Read)?.into_owned();
        let display = raw.display().to_string();

        if entry.header().entry_type().is_symlink() || entry.header().entry_type().is_hard_link() {
            return Err(ArchiveError::UnsafePath {
                entry: display,
                reason: "link entries are not supported",
            });
        }

        let relative = strip_prefix_component(&raw, &display)?;
        if relative.as_os_str().is_empty() {
            continue;
        }

        let target = dest.join(&relative);
        entry.unpack(&target).map_err(|source| ArchiveError::Write {
            path: target,
            source,
        })?;
    }

    Ok(())
}

/// Drop the first path component and reject anything that could escape.
fn strip_prefix_component(raw: &Path, display: &str) -> Result<PathBuf, ArchiveError> {
    let mut components = raw.components();

    // The first component must be a plain directory name — the `package/`
    // prefix. Anything else is already an escape attempt.
    match components.next() {
        Some(Component::Normal(_)) | None => {}
        Some(_) => {
            return Err(ArchiveError::UnsafePath {
                entry: display.to_string(),
                reason: "path is absolute or starts outside the package root",
            });
        }
    }

    let mut relative = PathBuf::new();
    for component in components {
        match component {
            Component::Normal(part) => relative.push(part),
            Component::CurDir => {}
            _ => {
                return Err(ArchiveError::UnsafePath {
                    entry: display.to_string(),
                    reason: "path escapes the destination directory",
                });
            }
        }
    }

    Ok(relative)
}
```

- [ ] **Step 5: Register the modules**

In `src/lib.rs`, add `pub mod archive;` and `pub mod testing;`.

In `src/error.rs`, add `use crate::archive::ArchiveError;` and:

```rust
    #[error(transparent)]
    Archive(#[from] ArchiveError),
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --lib archive`
Expected: PASS, 7 tests.

- [ ] **Step 7: Commit**

```bash
git add src/
git commit -m "feat: tarball extraction with tar-slip rejection"
```

---
### Task 6: Content-addressed store

**Files:**
- Create: `src/store.rs`
- Modify: `src/lib.rs`, `src/error.rs`
- Test: inline `#[cfg(test)]` module in `src/store.rs`

**Interfaces:**
- Consumes: nothing
- Produces: `store::Store`, `Store::new(impl Into<PathBuf>) -> Store`, `Store::entry_path(&self, &str) -> PathBuf`, `Store::contains(&self, &str) -> bool`, `Store::commit<F, E>(&self, key: &str, populate: F) -> Result<PathBuf, StoreError>` where `F: FnOnce(&Path) -> Result<(), E>`, `store::StoreError`

The `populate` closure inverts the dependency: the store owns atomicity and knows nothing about tarballs, while `archive` owns extraction and knows nothing about the store. It also makes the store testable without building a single tarball.

- [ ] **Step 1: Write the failing test**

Create `src/store.rs` with:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[derive(Debug, thiserror::Error)]
    #[error("populate failed on purpose")]
    struct Boom;

    fn write_one_file(dir: &Path) -> Result<(), std::io::Error> {
        std::fs::write(dir.join("index.js"), "contents")
    }

    #[test]
    fn commit_creates_an_entry_and_contains_reports_it() {
        let root = TempDir::new().unwrap();
        let store = Store::new(root.path());

        assert!(!store.contains("sha512-abc"));
        let path = store.commit("sha512-abc", write_one_file).unwrap();

        assert!(store.contains("sha512-abc"));
        assert_eq!(path, store.entry_path("sha512-abc"));
        assert_eq!(
            std::fs::read_to_string(path.join("index.js")).unwrap(),
            "contents"
        );
    }

    #[test]
    fn entries_live_under_a_versioned_directory() {
        let root = TempDir::new().unwrap();
        let store = Store::new(root.path());
        assert_eq!(store.entry_path("k"), root.path().join("v1").join("k"));
    }

    #[test]
    fn staging_happens_inside_the_store_root() {
        // rename(2) is atomic only within one filesystem. If staging escaped to
        // TMPDIR — often a separate tmpfs — commit would fail with EXDEV.
        let root = TempDir::new().unwrap();
        let store = Store::new(root.path());

        store
            .commit("sha512-abc", |staging| {
                assert!(
                    staging.starts_with(root.path()),
                    "staging dir {} escaped the store root",
                    staging.display()
                );
                write_one_file(staging)
            })
            .unwrap();
    }

    #[test]
    fn a_failed_populate_leaves_the_store_empty() {
        let root = TempDir::new().unwrap();
        let store = Store::new(root.path());

        let result = store.commit("sha512-abc", |_| Err(Boom));

        assert!(matches!(result, Err(StoreError::Populate { .. })));
        assert!(!store.contains("sha512-abc"));
        // And no staging debris is left behind.
        let staging = root.path().join("v1").join(".staging");
        if staging.exists() {
            assert_eq!(std::fs::read_dir(&staging).unwrap().count(), 0);
        }
    }

    #[test]
    fn an_existing_entry_is_not_repopulated() {
        let root = TempDir::new().unwrap();
        let store = Store::new(root.path());
        store.commit("sha512-abc", write_one_file).unwrap();

        // The race loser: another process already committed this key.
        let path = store
            .commit("sha512-abc", |dir| {
                std::fs::write(dir.join("index.js"), "different")
            })
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(path.join("index.js")).unwrap(),
            "contents",
            "an existing entry must never be clobbered"
        );
    }

    #[test]
    fn commit_is_idempotent() {
        let root = TempDir::new().unwrap();
        let store = Store::new(root.path());

        let first = store.commit("sha512-abc", write_one_file).unwrap();
        let second = store.commit("sha512-abc", write_one_file).unwrap();

        assert_eq!(first, second);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib store`
Expected: FAIL — compile error, `Store` not found.

- [ ] **Step 3: Write the implementation**

Prepend to `src/store.rs`:

```rust
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use thiserror::Error;

/// Bumped when the on-disk store format changes. Package-level content
/// addressing is `v1`; a future file-level CAS would write `v2` and leave
/// old stores ignorable rather than corrupt.
const STORE_VERSION: &str = "v1";
const STAGING_DIR: &str = ".staging";

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("failed to prepare the store at {path}")]
    StagingFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to commit {key} into the store")]
    CommitFailed {
        key: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to populate the store entry for {key}")]
    Populate {
        key: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

/// A content-addressed package store.
///
/// Keys are `Integrity::store_key()` values, so a package version's identity
/// is the hash of the bytes we already verified. Two projects wanting the same
/// package share one directory.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

/// A staging directory that deletes itself on drop unless kept.
///
/// Hand-rolled rather than using `tempfile::TempDir` because the directory has
/// to be *moved* on success, and disabling `TempDir`'s cleanup for that is an
/// API that has churned across tempfile versions.
struct StagingDir {
    path: PathBuf,
    keep: bool,
}

impl StagingDir {
    fn keep(&mut self) -> PathBuf {
        self.keep = true;
        self.path.clone()
    }
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// A name unique among concurrent processes and within this one, without
/// pulling in a random number generator.
fn unique_suffix() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{}-{}", std::process::id(), nanos, n)
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn versioned_root(&self) -> PathBuf {
        self.root.join(STORE_VERSION)
    }

    pub fn entry_path(&self, key: &str) -> PathBuf {
        self.versioned_root().join(key)
    }

    pub fn contains(&self, key: &str) -> bool {
        self.entry_path(key).is_dir()
    }

    /// Populate a store entry atomically.
    ///
    /// `populate` writes into a staging directory; on success the directory is
    /// renamed into its final content-addressed location. Extracting straight
    /// to the final path would mean a killed process leaves a *partial*
    /// directory at exactly the path that means "present and verified", so
    /// every later install would hard-link a truncated package into a project.
    ///
    /// Returns the entry path. If the entry already exists — because this
    /// process committed it earlier or another process won a race — `populate`
    /// is never called and the existing entry is left untouched.
    pub fn commit<F, E>(&self, key: &str, populate: F) -> Result<PathBuf, StoreError>
    where
        F: FnOnce(&Path) -> Result<(), E>,
        E: std::error::Error + Send + Sync + 'static,
    {
        let target = self.entry_path(key);
        if target.is_dir() {
            return Ok(target);
        }

        // The staging directory lives inside the store root so the rename below
        // never crosses a filesystem boundary.
        let staging_root = self.versioned_root().join(STAGING_DIR);
        std::fs::create_dir_all(&staging_root).map_err(|source| StoreError::StagingFailed {
            path: staging_root.clone(),
            source,
        })?;

        let staging_path = staging_root.join(unique_suffix());
        std::fs::create_dir(&staging_path).map_err(|source| StoreError::StagingFailed {
            path: staging_path.clone(),
            source,
        })?;
        let mut staging = StagingDir {
            path: staging_path,
            keep: false,
        };

        populate(&staging.path).map_err(|source| StoreError::Populate {
            key: key.to_string(),
            source: Box::new(source),
        })?;

        let staged = staging.keep();
        match std::fs::rename(&staged, &target) {
            Ok(()) => Ok(target),
            Err(source) => {
                // Renaming a directory onto a non-empty one fails rather than
                // clobbering, so losing the race is a success: the entry the
                // winner wrote is byte-identical, since the key is its hash.
                let _ = std::fs::remove_dir_all(&staged);
                if target.is_dir() {
                    Ok(target)
                } else {
                    Err(StoreError::CommitFailed {
                        key: key.to_string(),
                        source,
                    })
                }
            }
        }
    }
}
```

- [ ] **Step 4: Register the module**

In `src/lib.rs`, add `pub mod store;`.

In `src/error.rs`, add `use crate::store::StoreError;` and:

```rust
    #[error(transparent)]
    Store(#[from] StoreError),
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib store`
Expected: PASS, 6 tests.

- [ ] **Step 6: Commit**

```bash
git add src/
git commit -m "feat: content-addressed store with atomic commit"
```

---

### Task 7: Hard links and symlinks

**Files:**
- Create: `src/linker.rs`
- Modify: `src/lib.rs`, `src/error.rs`
- Test: inline `#[cfg(test)]` module in `src/linker.rs`

**Interfaces:**
- Consumes: nothing
- Produces: `linker::link_file(&Path, &Path)`, `linker::copy_tree(&Path, &Path)`, `linker::hard_link_tree(&Path, &Path)`, `linker::populate_virtual_store(store_entry: &Path, node_modules: &Path, dir_name: &str, pkg_name: &str) -> Result<PathBuf, LinkError>`, `linker::symlink_dependency(node_modules: &Path, pkg_name: &str, dir_name: &str) -> Result<(), LinkError>`, `linker::LinkError`

**Spec correction applied here.** Section 5 of the spec writes the symlink target as `../.jerky/lodash@4.17.21/node_modules/lodash`. That leading `../` is wrong: a symlink target is resolved relative to the directory *containing the link*, which is `node_modules/` itself, so `../` would point at the project root. The correct target is `.jerky/lodash@4.17.21/node_modules/lodash`. (pnpm uses `../` only for links *inside* the virtual store, where the link sits one level deeper.) Step 7 below fixes the spec.

- [ ] **Step 1: Write the failing test**

Create `src/linker.rs` with:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt as _;
    use tempfile::TempDir;

    fn store_entry(root: &Path) -> PathBuf {
        let entry = root.join("store-entry");
        std::fs::create_dir_all(entry.join("lib")).unwrap();
        std::fs::write(entry.join("package.json"), r#"{"name":"lodash"}"#).unwrap();
        std::fs::write(entry.join("lib/core.js"), "core").unwrap();
        entry
    }

    #[test]
    fn hard_link_tree_shares_inodes_rather_than_copying() {
        let root = TempDir::new().unwrap();
        let src = store_entry(root.path());
        let dst = root.path().join("dst");

        hard_link_tree(&src, &dst).unwrap();

        // The whole point of the store: one copy of the bytes, many names.
        // A silent regression to fs::copy passes every other assertion here.
        for relative in ["package.json", "lib/core.js"] {
            let a = std::fs::metadata(src.join(relative)).unwrap();
            let b = std::fs::metadata(dst.join(relative)).unwrap();
            assert_eq!(
                (a.dev(), a.ino()),
                (b.dev(), b.ino()),
                "{relative} was copied, not hard-linked"
            );
        }
    }

    #[test]
    fn hard_link_tree_recreates_nested_directories() {
        let root = TempDir::new().unwrap();
        let src = store_entry(root.path());
        let dst = root.path().join("dst");

        hard_link_tree(&src, &dst).unwrap();

        assert!(dst.join("lib").is_dir());
        assert_eq!(
            std::fs::read_to_string(dst.join("lib/core.js")).unwrap(),
            "core"
        );
    }

    #[test]
    fn copy_tree_duplicates_contents_with_distinct_inodes() {
        let root = TempDir::new().unwrap();
        let src = store_entry(root.path());
        let dst = root.path().join("dst");

        copy_tree(&src, &dst).unwrap();

        let a = std::fs::metadata(src.join("package.json")).unwrap();
        let b = std::fs::metadata(dst.join("package.json")).unwrap();
        assert_ne!((a.dev(), a.ino()), (b.dev(), b.ino()));
        assert_eq!(
            std::fs::read_to_string(dst.join("package.json")).unwrap(),
            r#"{"name":"lodash"}"#
        );
    }

    #[test]
    fn populate_virtual_store_nests_the_package_under_its_own_node_modules() {
        let root = TempDir::new().unwrap();
        let src = store_entry(root.path());
        let node_modules = root.path().join("node_modules");

        let dir = populate_virtual_store(&src, &node_modules, "lodash@4.17.21", "lodash").unwrap();

        assert_eq!(dir, node_modules.join(".jerky").join("lodash@4.17.21"));
        assert!(dir.join("node_modules/lodash/package.json").is_file());
    }

    #[test]
    fn populate_virtual_store_leaves_no_staging_debris() {
        let root = TempDir::new().unwrap();
        let src = store_entry(root.path());
        let node_modules = root.path().join("node_modules");

        populate_virtual_store(&src, &node_modules, "lodash@4.17.21", "lodash").unwrap();

        let staging = node_modules.join(".jerky").join(".staging");
        if staging.exists() {
            assert_eq!(std::fs::read_dir(&staging).unwrap().count(), 0);
        }
    }

    #[test]
    fn symlink_dependency_creates_a_relative_link_that_resolves() {
        let root = TempDir::new().unwrap();
        let src = store_entry(root.path());
        let node_modules = root.path().join("node_modules");
        populate_virtual_store(&src, &node_modules, "lodash@4.17.21", "lodash").unwrap();

        symlink_dependency(&node_modules, "lodash", "lodash@4.17.21").unwrap();

        let link = node_modules.join("lodash");
        let target = std::fs::read_link(&link).unwrap();
        assert!(
            target.is_relative(),
            "an absolute target breaks as soon as the project moves"
        );
        assert_eq!(
            target,
            Path::new(".jerky/lodash@4.17.21/node_modules/lodash")
        );
        // And it actually resolves.
        assert!(link.join("package.json").is_file());
    }

    #[test]
    fn symlink_dependency_replaces_an_existing_link() {
        let root = TempDir::new().unwrap();
        let src = store_entry(root.path());
        let node_modules = root.path().join("node_modules");
        populate_virtual_store(&src, &node_modules, "lodash@4.17.21", "lodash").unwrap();
        populate_virtual_store(&src, &node_modules, "lodash@3.0.0", "lodash").unwrap();

        symlink_dependency(&node_modules, "lodash", "lodash@3.0.0").unwrap();
        symlink_dependency(&node_modules, "lodash", "lodash@4.17.21").unwrap();

        let target = std::fs::read_link(node_modules.join("lodash")).unwrap();
        assert_eq!(
            target,
            Path::new(".jerky/lodash@4.17.21/node_modules/lodash")
        );
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib linker`
Expected: FAIL — compile error, `hard_link_tree` not found.

- [ ] **Step 3: Write the implementation**

Prepend to `src/linker.rs`:

```rust
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use thiserror::Error;

/// `EXDEV`, "cross-device link". Both Linux and macOS use 18.
/// `io::ErrorKind::CrossesDevices` would be cleaner but is still unstable.
const EXDEV: i32 = 18;

const VIRTUAL_STORE_DIR: &str = ".jerky";
const STAGING_DIR: &str = ".staging";

#[derive(Debug, Error)]
pub enum LinkError {
    #[error("failed to link {from} to {to}")]
    Io {
        from: PathBuf,
        to: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to access {path}")]
    Access {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} already exists and is not a symlink jerky can replace")]
    ConflictingEntry { path: PathBuf },
}

fn unique_suffix() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{}-{}", std::process::id(), nanos, n)
}

/// Hard-link one file, falling back to a copy across filesystems.
///
/// The store and a project can legitimately live on different volumes, where
/// `hard_link` fails with `EXDEV`. Only that specific errno falls back — any
/// other failure is a real error, not something to paper over with a copy.
pub fn link_file(src: &Path, dst: &Path) -> Result<(), LinkError> {
    match std::fs::hard_link(src, dst) {
        Ok(()) => Ok(()),
        Err(source) if source.raw_os_error() == Some(EXDEV) => {
            std::fs::copy(src, dst).map(|_| ()).map_err(|source| LinkError::Io {
                from: src.to_path_buf(),
                to: dst.to_path_buf(),
                source,
            })
        }
        Err(source) => Err(LinkError::Io {
            from: src.to_path_buf(),
            to: dst.to_path_buf(),
            source,
        }),
    }
}

fn walk_tree(
    src: &Path,
    dst: &Path,
    place: &dyn Fn(&Path, &Path) -> Result<(), LinkError>,
) -> Result<(), LinkError> {
    std::fs::create_dir_all(dst).map_err(|source| LinkError::Access {
        path: dst.to_path_buf(),
        source,
    })?;

    let entries = std::fs::read_dir(src).map_err(|source| LinkError::Access {
        path: src.to_path_buf(),
        source,
    })?;

    for entry in entries {
        let entry = entry.map_err(|source| LinkError::Access {
            path: src.to_path_buf(),
            source,
        })?;
        let from = entry.path();
        let to = dst.join(entry.file_name());

        let file_type = entry.file_type().map_err(|source| LinkError::Access {
            path: from.clone(),
            source,
        })?;

        if file_type.is_dir() {
            walk_tree(&from, &to, place)?;
        } else {
            place(&from, &to)?;
        }
    }

    Ok(())
}

/// Recreate `src`'s directory structure at `dst`, hard-linking every file.
///
/// Directories cannot be hard-linked, so they are created and only regular
/// files are linked.
pub fn hard_link_tree(src: &Path, dst: &Path) -> Result<(), LinkError> {
    walk_tree(src, dst, &link_file)
}

/// Recreate `src` at `dst` by copying. The `EXDEV` fallback path, exposed so
/// it can be tested directly — CI cannot practically mount a second
/// filesystem to trigger it naturally.
pub fn copy_tree(src: &Path, dst: &Path) -> Result<(), LinkError> {
    walk_tree(src, dst, &|from: &Path, to: &Path| {
        std::fs::copy(from, to).map(|_| ()).map_err(|source| LinkError::Io {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
            source,
        })
    })
}

/// Build `node_modules/.jerky/<dir_name>/node_modules/<pkg_name>/` from a
/// store entry, and return the `<dir_name>` directory.
///
/// The doubled `node_modules` is the mechanism, not an accident: Node resolves
/// a package's dependencies by walking up from its directory looking for a
/// `node_modules`, so a package nested inside its own sees exactly its
/// declared dependencies as siblings and nothing else.
///
/// Uses the same stage-and-rename discipline as the store, for the same
/// reason: a half-linked tree looks present.
pub fn populate_virtual_store(
    store_entry: &Path,
    node_modules: &Path,
    dir_name: &str,
    pkg_name: &str,
) -> Result<PathBuf, LinkError> {
    let virtual_root = node_modules.join(VIRTUAL_STORE_DIR);
    let target = virtual_root.join(dir_name);
    if target.is_dir() {
        return Ok(target);
    }

    let staging_root = virtual_root.join(STAGING_DIR);
    std::fs::create_dir_all(&staging_root).map_err(|source| LinkError::Access {
        path: staging_root.clone(),
        source,
    })?;

    let staged = staging_root.join(unique_suffix());
    let result = hard_link_tree(store_entry, &staged.join("node_modules").join(pkg_name));

    if let Err(err) = result {
        let _ = std::fs::remove_dir_all(&staged);
        return Err(err);
    }

    match std::fs::rename(&staged, &target) {
        Ok(()) => Ok(target),
        Err(source) => {
            let _ = std::fs::remove_dir_all(&staged);
            if target.is_dir() {
                Ok(target)
            } else {
                Err(LinkError::Io {
                    from: staged,
                    to: target,
                    source,
                })
            }
        }
    }
}

/// Link `node_modules/<pkg_name>` at the package's virtual store directory.
///
/// The target is relative so that moving or copying a project does not break
/// every link. It is resolved relative to `node_modules/` — the directory
/// holding the link — so it carries no leading `../`.
pub fn symlink_dependency(
    node_modules: &Path,
    pkg_name: &str,
    dir_name: &str,
) -> Result<(), LinkError> {
    std::fs::create_dir_all(node_modules).map_err(|source| LinkError::Access {
        path: node_modules.to_path_buf(),
        source,
    })?;

    let link = node_modules.join(pkg_name);
    let target = Path::new(VIRTUAL_STORE_DIR)
        .join(dir_name)
        .join("node_modules")
        .join(pkg_name);

    // symlink_metadata does not follow the link, so a dangling link is still
    // detected and replaced rather than reported as missing.
    match std::fs::symlink_metadata(&link) {
        Ok(meta) if meta.file_type().is_symlink() => {
            std::fs::remove_file(&link).map_err(|source| LinkError::Access {
                path: link.clone(),
                source,
            })?;
        }
        Ok(_) => return Err(LinkError::ConflictingEntry { path: link }),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => return Err(LinkError::Access { path: link, source }),
    }

    std::os::unix::fs::symlink(&target, &link).map_err(|source| LinkError::Io {
        from: target,
        to: link,
        source,
    })
}
```

- [ ] **Step 4: Register the module**

In `src/lib.rs`, add `pub mod linker;`.

In `src/error.rs`, add `use crate::linker::LinkError;` and:

```rust
    #[error(transparent)]
    Link(#[from] LinkError),
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib linker`
Expected: PASS, 7 tests.

- [ ] **Step 6: Correct the spec**

In `docs/superpowers/specs/2026-08-24-package-manager-design.md`, section 5, change the symlink line from:

```
<project>/node_modules/lodash -> ../.jerky/lodash@4.17.21/node_modules/lodash
```

to:

```
<project>/node_modules/lodash -> .jerky/lodash@4.17.21/node_modules/lodash
```

and append to that section's "Symlinks are relative" paragraph:

```markdown
The target carries no leading `../`: it is resolved relative to
`node_modules/`, the directory holding the link. pnpm uses `../` only for
links *inside* the virtual store, which sit one level deeper.
```

- [ ] **Step 7: Commit**

```bash
git add src/ docs/
git commit -m "feat: hard-link and symlink wiring for the virtual store"
```

---

### Task 8: Registry trait and fixture registry

**Files:**
- Create: `src/registry.rs`
- Modify: `src/lib.rs`, `src/testing.rs`, `src/error.rs`
- Test: inline `#[cfg(test)]` module in `src/registry.rs`

**Interfaces:**
- Consumes: `integrity::Integrity`, `integrity::IntegrityError`
- Produces: `registry::RegistryClient` trait with `version_metadata(&self, name: &str, version: &str) -> Result<VersionMetadata, RegistryError>` and `fetch_tarball(&self, url: &str) -> Result<Vec<u8>, RegistryError>`; `registry::VersionMetadata { name, version, dist }`; `registry::Dist { tarball, integrity, shasum }`; `Dist::integrity(&self) -> Result<Integrity, IntegrityError>`; `registry::RegistryError`; `testing::FixtureRegistry` with `new()`, `with_package(name, version, tarball_bytes) -> Self`, `metadata_calls()`, `tarball_calls()`

- [ ] **Step 1: Write the failing test**

Create `src/registry.rs` with:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dist_prefers_the_integrity_field() {
        let dist = Dist {
            tarball: "https://example.test/x.tgz".into(),
            integrity: Some("sha512-3a81oZNherrMQXNJriBBMRLm+k6JqX6iCp7u5ktV05ohkpkqJ0/BqDa6PCOj/uu9RU7XvQFt+DR3B7bLDgnnFQ==".into()),
            shasum: Some("a9993e364706816aba3e25717850c26c9cd0d89d".into()),
        };
        assert_eq!(dist.integrity().unwrap().algo, crate::integrity::Algo::Sha512);
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
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib registry`
Expected: FAIL — compile error, `Dist` not found.

- [ ] **Step 3: Write the implementation**

Prepend to `src/registry.rs`:

```rust
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
```

- [ ] **Step 4: Add `FixtureRegistry` to `src/testing.rs`**

Append to `src/testing.rs`:

```rust
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::integrity::Integrity;
use crate::registry::{Dist, RegistryClient, RegistryError, VersionMetadata};

/// An in-memory registry for tests.
///
/// Counts its calls so tests can prove the store-hit path was taken rather
/// than a redundant download that happened to produce the same result.
#[derive(Default)]
pub struct FixtureRegistry {
    versions: HashMap<(String, String), VersionMetadata>,
    tarballs: HashMap<String, Vec<u8>>,
    metadata_calls: AtomicUsize,
    tarball_calls: AtomicUsize,
}

impl FixtureRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a package version, deriving a self-consistent integrity hash
    /// from the tarball bytes so the happy path verifies by construction.
    /// Also registers it under the `latest` tag.
    pub fn with_package(mut self, name: &str, version: &str, tarball: Vec<u8>) -> Self {
        let url = format!("https://fixture.test/{name}/-/{name}-{version}.tgz");
        let integrity = Integrity {
            algo: crate::integrity::Algo::Sha512,
            digest: <sha2::Sha512 as sha2::Digest>::digest(&tarball).to_vec(),
        };

        let metadata = VersionMetadata {
            name: name.to_string(),
            version: version.to_string(),
            dist: Dist {
                tarball: url.clone(),
                integrity: Some(integrity.to_ssri()),
                shasum: None,
            },
        };

        self.tarballs.insert(url, tarball);
        self.versions
            .insert((name.to_string(), version.to_string()), metadata.clone());
        self.versions
            .insert((name.to_string(), "latest".to_string()), metadata);
        self
    }

    /// Register a package whose advertised integrity does not match its bytes.
    pub fn with_corrupt_package(mut self, name: &str, version: &str, tarball: Vec<u8>) -> Self {
        let url = format!("https://fixture.test/{name}/-/{name}-{version}.tgz");
        let wrong = Integrity {
            algo: crate::integrity::Algo::Sha512,
            digest: <sha2::Sha512 as sha2::Digest>::digest(b"not these bytes").to_vec(),
        };

        let metadata = VersionMetadata {
            name: name.to_string(),
            version: version.to_string(),
            dist: Dist {
                tarball: url.clone(),
                integrity: Some(wrong.to_ssri()),
                shasum: None,
            },
        };

        self.tarballs.insert(url, tarball);
        self.versions
            .insert((name.to_string(), version.to_string()), metadata.clone());
        self.versions
            .insert((name.to_string(), "latest".to_string()), metadata);
        self
    }

    pub fn metadata_calls(&self) -> usize {
        self.metadata_calls.load(Ordering::Relaxed)
    }

    pub fn tarball_calls(&self) -> usize {
        self.tarball_calls.load(Ordering::Relaxed)
    }

    fn knows_package(&self, name: &str) -> bool {
        self.versions.keys().any(|(n, _)| n == name)
    }
}

impl RegistryClient for FixtureRegistry {
    fn version_metadata(
        &self,
        name: &str,
        version: &str,
    ) -> Result<VersionMetadata, RegistryError> {
        self.metadata_calls.fetch_add(1, Ordering::Relaxed);
        match self.versions.get(&(name.to_string(), version.to_string())) {
            Some(metadata) => Ok(metadata.clone()),
            None if self.knows_package(name) => Err(RegistryError::VersionNotFound {
                name: name.to_string(),
                version: version.to_string(),
            }),
            None => Err(RegistryError::PackageNotFound(name.to_string())),
        }
    }

    fn fetch_tarball(&self, url: &str) -> Result<Vec<u8>, RegistryError> {
        self.tarball_calls.fetch_add(1, Ordering::Relaxed);
        self.tarballs
            .get(url)
            .cloned()
            .ok_or_else(|| RegistryError::PackageNotFound(url.to_string()))
    }
}
```

- [ ] **Step 5: Register the module**

In `src/lib.rs`, add `pub mod registry;`.

In `src/error.rs`, add `use crate::registry::RegistryError;` and:

```rust
    #[error(transparent)]
    Registry(#[from] RegistryError),
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --lib registry`
Expected: PASS, 4 tests.

- [ ] **Step 7: Commit**

```bash
git add src/
git commit -m "feat: registry client trait and in-memory fixture"
```

---

### Task 9: HTTP registry client

**Files:**
- Modify: `src/registry.rs`
- Test: `tests/http_registry.rs`

**Interfaces:**
- Consumes: `RegistryClient`, `VersionMetadata`, `RegistryError`
- Produces: `registry::HttpRegistry`, `HttpRegistry::new() -> HttpRegistry`, `HttpRegistry::with_base_url(impl Into<String>) -> HttpRegistry`, `registry::DEFAULT_REGISTRY`

- [ ] **Step 1: Write the failing test**

Create `tests/http_registry.rs`:

```rust
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use jerky::registry::{HttpRegistry, RegistryClient, RegistryError};

/// A canned response for one request path.
struct Route {
    status: u16,
    body: &'static str,
}

/// Spin up a throwaway HTTP server that answers from a routing table and
/// counts requests. Returns the base URL and the counter.
fn serve(routes: Vec<(&'static str, Route)>) -> (String, Arc<AtomicUsize>) {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);

    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            counter.fetch_add(1, Ordering::Relaxed);
            let url = request.url().to_string();
            let matched = routes.iter().find(|(path, _)| *path == url);

            let response = match matched {
                Some((_, route)) => tiny_http::Response::from_string(route.body)
                    .with_status_code(route.status),
                None => tiny_http::Response::from_string("not found").with_status_code(404),
            };
            let _ = request.respond(response);
        }
    });

    (format!("http://127.0.0.1:{port}"), hits)
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
        Route { status: 200, body: LODASH },
    )]);
    let registry = HttpRegistry::with_base_url(base);

    let metadata = registry.version_metadata("lodash", "4.17.21").unwrap();

    assert_eq!(metadata.version, "4.17.21");
}

#[test]
fn resolves_a_dist_tag_through_the_same_endpoint() {
    let (base, _) = serve(vec![("/lodash/latest", Route { status: 200, body: LODASH })]);
    let registry = HttpRegistry::with_base_url(base);

    let metadata = registry.version_metadata("lodash", "latest").unwrap();

    assert_eq!(metadata.version, "4.17.21", "the response is authoritative");
}

#[test]
fn distinguishes_an_unknown_package_from_an_unknown_version() {
    // The package exists, the version does not: the probe of /lodash succeeds.
    let (base, _) = serve(vec![("/lodash", Route { status: 200, body: "{}" })]);
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
        Route { status: 503, body: "down" },
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
        Route { status: 200, body: "{ not json" },
    )]);
    let registry = HttpRegistry::with_base_url(base);

    assert!(matches!(
        registry.version_metadata("lodash", "4.17.21"),
        Err(RegistryError::MalformedResponse { .. })
    ));
}

#[test]
fn fetches_tarball_bytes() {
    let (base, _) = serve(vec![("/x.tgz", Route { status: 200, body: "tarball-bytes" })]);
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
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --test http_registry`
Expected: FAIL — compile error, `HttpRegistry` not found.

- [ ] **Step 3: Write the implementation**

Append to `src/registry.rs`, above the test module:

```rust
pub const DEFAULT_REGISTRY: &str = "https://registry.npmjs.org";

const MAX_ATTEMPTS: u32 = 3;

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

impl HttpRegistry {
    pub fn new() -> Self {
        Self::with_base_url(DEFAULT_REGISTRY)
    }

    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            agent: ureq::AgentBuilder::new()
                .user_agent(concat!("jerky/", env!("CARGO_PKG_VERSION")))
                .build(),
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
                        std::thread::sleep(std::time::Duration::from_millis(
                            100 * 2_u64.pow(n),
                        ));
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

enum Retry {
    Yes,
    No,
}

impl RegistryClient for HttpRegistry {
    fn version_metadata(
        &self,
        name: &str,
        version: &str,
    ) -> Result<VersionMetadata, RegistryError> {
        let url = format!(
            "{}/{}/{}",
            self.base_url,
            Self::encode_name(name),
            version
        );

        let body = Self::with_retries(|| match self.agent.get(&url).call() {
            Ok(response) => response.into_string().map_err(|source| {
                (
                    RegistryError::MalformedResponse {
                        url: url.clone(),
                        source: Box::new(source),
                    },
                    Retry::Yes,
                )
            }),
            Err(ureq::Error::Status(404, _)) => {
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
            Ok(response) => {
                let mut bytes = Vec::new();
                std::io::Read::read_to_end(&mut response.into_reader(), &mut bytes).map_err(
                    |source| {
                        (
                            RegistryError::Network {
                                url: url.to_string(),
                                source: Box::new(source),
                            },
                            Retry::Yes,
                        )
                    },
                )?;
                Ok(bytes)
            }
            Err(ureq::Error::Status(404, _)) => Err((
                RegistryError::PackageNotFound(url.to_string()),
                Retry::No,
            )),
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
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --test http_registry`
Expected: PASS, 8 tests, 1 ignored.

- [ ] **Step 5: Verify the live test works**

Run: `cargo test --test http_registry -- --ignored`
Expected: PASS, 1 test. This requires network access; if it fails with a connection error, note it and move on — it is deliberately excluded from the default run.

- [ ] **Step 6: Commit**

```bash
git add src/ tests/
git commit -m "feat: HTTP registry client with narrow retries"
```

---

### Task 10: `jerky install`

**Files:**
- Create: `src/commands/install.rs`, `tests/install.rs`
- Modify: `src/commands/mod.rs`, `src/main.rs`, `src/error.rs`

**Interfaces:**
- Consumes: `PackageSpec`, `Manifest`, `Store`, `RegistryClient`, `archive::extract`, `linker::populate_virtual_store`, `linker::symlink_dependency`, `Dist::integrity`
- Produces: `commands::install::install(project_dir: &Path, store: &Store, registry: &dyn RegistryClient, spec: &PackageSpec) -> Result<Installed, InstallError>`, `commands::install::Installed { name, version }`, `commands::install::InstallError`

- [ ] **Step 1: Write the failing test**

Create `tests/install.rs`:

```rust
use std::path::Path;

use jerky::cli::{PackageSpec, VersionSpec};
use jerky::commands::install::{InstallError, install};
use jerky::store::Store;
use jerky::testing::{FixtureRegistry, TarEntry, build_tarball};

use std::os::unix::fs::MetadataExt as _;
use tempfile::TempDir;

fn lodash_tarball() -> Vec<u8> {
    build_tarball(&[
        TarEntry::file("package/package.json", r#"{"name":"lodash","version":"4.17.21"}"#),
        TarEntry::file("package/lodash.js", "module.exports = {};"),
    ])
}

fn project(dir: &Path) -> &Path {
    std::fs::write(dir.join("package.json"), "{\n  \"name\": \"demo\"\n}\n").unwrap();
    dir
}

fn spec(name: &str, version: VersionSpec) -> PackageSpec {
    PackageSpec {
        name: name.to_string(),
        version,
    }
}

#[test]
fn installs_a_package_end_to_end() {
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let project_dir = project(work.path());
    let store = Store::new(home.path().join("store"));
    let registry = FixtureRegistry::new().with_package("lodash", "4.17.21", lodash_tarball());

    let installed = install(project_dir, &store, &registry, &spec("lodash", VersionSpec::Latest)).unwrap();

    assert_eq!(installed.version, "4.17.21");

    // The symlink resolves through the virtual store to real contents.
    let link = project_dir.join("node_modules/lodash");
    assert!(link.join("package.json").is_file());
    assert_eq!(
        std::fs::read_to_string(link.join("lodash.js")).unwrap(),
        "module.exports = {};"
    );

    // The manifest records the concrete version, with no caret.
    let raw = std::fs::read_to_string(project_dir.join("package.json")).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(parsed["dependencies"]["lodash"], "4.17.21");
}

#[test]
fn the_recorded_version_comes_from_the_registry_not_the_request() {
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let project_dir = project(work.path());
    let store = Store::new(home.path().join("store"));
    let registry = FixtureRegistry::new().with_package("react", "18.2.0", lodash_tarball());

    // Requesting a dist-tag must pin whatever concrete version came back.
    install(project_dir, &store, &registry, &spec("react", VersionSpec::Exact("latest".into()))).unwrap();

    let raw = std::fs::read_to_string(project_dir.join("package.json")).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(parsed["dependencies"]["react"], "18.2.0");
}

#[test]
fn files_are_hard_linked_from_the_store_not_copied() {
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let project_dir = project(work.path());
    // Store and project share one temp root's filesystem, so the legitimate
    // EXDEV copy fallback cannot fire and make these inodes differ correctly.
    let store = Store::new(home.path().join("store"));
    let registry = FixtureRegistry::new().with_package("lodash", "4.17.21", lodash_tarball());

    install(project_dir, &store, &registry, &spec("lodash", VersionSpec::Latest)).unwrap();

    let linked = project_dir
        .join("node_modules/.jerky/lodash@4.17.21/node_modules/lodash/lodash.js");
    let stored = std::fs::read_dir(store.entry_path_root())
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path().join("lodash.js"))
        .find(|p| p.is_file())
        .expect("a store entry exists");

    let a = std::fs::metadata(&linked).unwrap();
    let b = std::fs::metadata(&stored).unwrap();
    assert_eq!(
        (a.dev(), a.ino()),
        (b.dev(), b.ino()),
        "package was copied, not hard-linked — the store's whole purpose is lost"
    );
}

#[test]
fn a_second_install_reuses_the_store_without_downloading() {
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let project_dir = project(work.path());
    let store = Store::new(home.path().join("store"));
    let registry = FixtureRegistry::new().with_package("lodash", "4.17.21", lodash_tarball());

    install(project_dir, &store, &registry, &spec("lodash", VersionSpec::Latest)).unwrap();
    assert_eq!(registry.tarball_calls(), 1);

    install(project_dir, &store, &registry, &spec("lodash", VersionSpec::Latest)).unwrap();

    // Idempotence has to be *counted*, not just observed: identical output
    // would also result from downloading twice.
    assert_eq!(registry.tarball_calls(), 1, "the store hit was skipped");
    assert!(project_dir.join("node_modules/lodash/package.json").is_file());
}

#[test]
fn an_integrity_mismatch_leaves_the_store_empty() {
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let project_dir = project(work.path());
    let store = Store::new(home.path().join("store"));
    let registry =
        FixtureRegistry::new().with_corrupt_package("evil", "1.0.0", lodash_tarball());

    let result = install(project_dir, &store, &registry, &spec("evil", VersionSpec::Latest));

    assert!(matches!(result, Err(InstallError::Integrity { .. })));

    // Not merely "an error was returned": nothing may reach the store, and
    // the manifest must not claim a dependency that is not on disk.
    let entries = std::fs::read_dir(store.entry_path_root())
        .map(|d| d.filter_map(Result::ok).count())
        .unwrap_or(0);
    assert_eq!(entries, 0, "unverified bytes reached the store");
    assert!(!project_dir.join("node_modules/evil").exists());

    let raw = std::fs::read_to_string(project_dir.join("package.json")).unwrap();
    assert!(!raw.contains("evil"));
}

#[test]
fn reports_an_unknown_package() {
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let project_dir = project(work.path());
    let store = Store::new(home.path().join("store"));
    let registry = FixtureRegistry::new();

    assert!(matches!(
        install(project_dir, &store, &registry, &spec("nope", VersionSpec::Latest)),
        Err(InstallError::Registry(_))
    ));
}

#[test]
fn requires_a_manifest() {
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let store = Store::new(home.path().join("store"));
    let registry = FixtureRegistry::new().with_package("lodash", "4.17.21", lodash_tarball());

    // No package.json was written.
    assert!(matches!(
        install(work.path(), &store, &registry, &spec("lodash", VersionSpec::Latest)),
        Err(InstallError::Manifest(_))
    ));
}
```

- [ ] **Step 2: Add the store helper the tests need**

In `src/store.rs`, add to `impl Store`:

```rust
    /// The directory holding all entries. Exposed so tests can assert the
    /// store is empty after a rejected install.
    pub fn entry_path_root(&self) -> PathBuf {
        self.versioned_root()
    }
```

and make `versioned_root` reachable by leaving it private — `entry_path_root` is its public face.

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test --test install`
Expected: FAIL — compile error, `install` not found.

- [ ] **Step 4: Write the implementation**

Create `src/commands/install.rs`:

```rust
use std::path::Path;

use thiserror::Error;

use crate::archive::{self, ArchiveError};
use crate::cli::PackageSpec;
use crate::integrity::IntegrityError;
use crate::linker::{self, LinkError};
use crate::manifest::{Manifest, ManifestError};
use crate::registry::{RegistryClient, RegistryError};
use crate::store::{Store, StoreError};

#[derive(Debug, Error)]
pub enum InstallError {
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error("integrity check failed for {name}@{version}")]
    Integrity {
        name: String,
        version: String,
        #[source]
        source: IntegrityError,
    },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Link(#[from] LinkError),
    #[error(transparent)]
    Archive(#[from] ArchiveError),
}

#[derive(Debug, Clone)]
pub struct Installed {
    pub name: String,
    pub version: String,
}

/// Install one package into `project_dir`.
///
/// Ordering is deliberate. The manifest write is last, so a failure anywhere
/// leaves at worst an installed-but-unrecorded package — harmless and
/// self-healing on rerun — rather than a `package.json` claiming a dependency
/// that is not on disk.
pub fn install(
    project_dir: &Path,
    store: &Store,
    registry: &dyn RegistryClient,
    spec: &PackageSpec,
) -> Result<Installed, InstallError> {
    // Load the manifest first: failing before any network or disk work is the
    // cheapest possible way to reject a project with no package.json.
    let mut manifest = Manifest::load(project_dir)?;

    let metadata = registry.version_metadata(&spec.name, spec.version.as_request())?;
    let name = metadata.name.clone();
    let version = metadata.version.clone();

    let integrity = metadata
        .dist
        .integrity()
        .map_err(|source| InstallError::Integrity {
            name: name.clone(),
            version: version.clone(),
            source,
        })?;
    let key = integrity.store_key();

    // Populating is skipped entirely when the store already holds these bytes,
    // so a repeat install performs no download.
    let entry = if store.contains(&key) {
        store.entry_path(&key)
    } else {
        let tarball = registry.fetch_tarball(&metadata.dist.tarball)?;

        // Verify the complete buffer before a single byte is extracted. A
        // stream-and-hash design would only detect a mismatch after writing
        // attacker-controlled files to disk.
        integrity
            .verify(&tarball)
            .map_err(|source| InstallError::Integrity {
                name: name.clone(),
                version: version.clone(),
                source,
            })?;

        store.commit(&key, |staging| archive::extract(&tarball, staging))?
    };

    let node_modules = project_dir.join("node_modules");
    let dir_name = format!("{name}@{version}");
    linker::populate_virtual_store(&entry, &node_modules, &dir_name, &name)?;
    linker::symlink_dependency(&node_modules, &name, &dir_name)?;

    // Last: never record something that is not already true on disk. And
    // always the concrete version from the response, never a caret range that
    // spec 1 has no resolver to honour.
    manifest.add_dependency(&name, &version);
    manifest.save()?;

    Ok(Installed { name, version })
}
```

- [ ] **Step 5: Register the module and wire the command**

In `src/commands/mod.rs`, add `pub mod install;`.

In `src/error.rs`, add `use crate::commands::install::InstallError;`, add the variant, and **delete the now-unused `NotImplemented` variant**:

```rust
    #[error(transparent)]
    Install(#[from] InstallError),
```

In `src/main.rs`, replace `run`:

```rust
fn run(cli: Cli) -> Result<(), JerkyError> {
    let project_dir = std::env::current_dir().map_err(JerkyError::Cwd)?;

    match cli.command {
        Command::Init => {
            let path = jerky::commands::init::init(&project_dir)?;
            println!("wrote {}", path.display());
            Ok(())
        }
        Command::Install { spec } => {
            let spec = jerky::cli::parse_package_spec(&spec)?;
            let store = jerky::store::Store::new(store_root()?);
            let registry = jerky::registry::HttpRegistry::new();
            let installed =
                jerky::commands::install::install(&project_dir, &store, &registry, &spec)?;
            println!("added {}@{}", installed.name, installed.version);
            Ok(())
        }
    }
}

/// `main` is the only place allowed to read `$HOME`; everything below it takes
/// paths as parameters so tests never touch a developer's real store.
fn store_root() -> Result<std::path::PathBuf, JerkyError> {
    let home = dirs::home_dir().ok_or(JerkyError::NoHomeDirectory)?;
    Ok(home.join(".jerky").join("store"))
}
```

In `src/error.rs`, add:

```rust
    #[error("could not determine the home directory for the package store")]
    NoHomeDirectory,
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --test install`
Expected: PASS, 7 tests.

- [ ] **Step 7: Run the whole suite**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: all PASS across every module.

- [ ] **Step 8: Verify against the real registry**

```bash
mkdir -p /tmp/jerky-real && cd /tmp/jerky-real
cargo run --manifest-path "$OLDPWD/Cargo.toml" -- init
cargo run --manifest-path "$OLDPWD/Cargo.toml" -- install lodash
node -e "console.log(typeof require('lodash').chunk)"
```
Expected: `added lodash@<version>`, then `function`. Confirm `~/.jerky/store/v1/` holds one entry and that `node_modules/lodash` is a symlink into `.jerky/`.

Running `install lodash` a second time must succeed without re-downloading.

- [ ] **Step 9: Commit**

```bash
git add src/ tests/
git commit -m "feat: jerky install end to end"
```

---

## Self-Review

Run after the plan is written, before execution begins.

**1. Spec coverage.** Every section of the spec maps to a task:

| Spec section | Task |
|---|---|
| 4 — `jerky init` | 3 |
| 4 — `jerky install <spec>` | 1 (parsing), 10 (behaviour) |
| 5 — module layout, lib+bin split | 1 |
| 5 — path scheme, hex store keys | 6, 7 |
| 6 — types and flow, steps (1)–(5) | 4, 5, 8, 9, 10 |
| 7 — store, staging, concurrency | 6 |
| 8 — linking, EXDEV, stage-and-rename | 7 |
| 9 — error taxonomy, failure states, retries | every task; retries in 9 |
| 10 — testing layers, fixtures, isolation, CI | 1 (CI), 5 (fixtures), 8 (fake), 9 (http gap), 10 (assertions) |

**2. Placeholder scan.** No `TBD`, no "add error handling", no "similar to Task N". The one `todo!()`-shaped construct — `JerkyError::NotImplemented` — is real, compiling code introduced in Task 1 and deleted in Task 10.

**3. Type consistency.** Names used across task boundaries were checked against their definitions: `VersionSpec::as_request` (Task 1 → 10), `Manifest::{load, create_default, add_dependency, save, path}` (Task 2 → 3, 10), `Integrity::{parse, from_shasum_hex, store_key, verify, to_ssri}` (Task 4 → 8, 10), `archive::extract` (Task 5 → 10), `Store::{contains, entry_path, commit}` (Task 6 → 10), `linker::{populate_virtual_store, symlink_dependency}` (Task 7 → 10), `RegistryClient::{version_metadata, fetch_tarball}` (Task 8 → 9, 10).

**Two issues found and fixed during review:**

- **`Store::entry_path_root` was missing.** The Task 10 tests assert the store is empty after a rejected install, which needs a public accessor for the versioned root. Added as Task 10 Step 2 rather than being left as an undefined method.
- **The spec's symlink target was wrong.** Section 5 writes `../.jerky/...`, but a symlink target resolves relative to the directory containing the link — `node_modules/` — so the leading `../` would point at the project root and the link would dangle. Corrected to `.jerky/...` in Task 7, which also patches the spec.

**4. Deviations from the spec, recorded deliberately:**

- **Store keys carry an algorithm prefix** (`sha512-<hex>`) rather than bare hex. Still lowercase, so the macOS case-insensitivity constraint holds; the prefix keeps sha1 and sha512 digests in separate namespaces.
- **`tempfile::TempDir` is not used for staging.** Both `store` and `linker` hand-roll a `StagingDir` with a `Drop` impl, because staging directories must be *moved* on success and the tempfile API for that has churned across versions.
- **Link entries in tarballs are rejected outright** rather than resolved and range-checked. Some real packages do contain symlinks; documented in `archive::extract` as a limitation to revisit.
