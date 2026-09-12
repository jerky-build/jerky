//! Who the members of a workspace are.
//!
//! Membership comes from the root `package.json`'s `workspaces` field, which
//! is what npm and yarn already read: an existing monorepo is a jerky
//! workspace with no new file and no migration step. Absence of the field is
//! not a separate case — it is a workspace of one, keyed `.`, taking the same
//! path as any other — so there is no single-project branch to keep working.
//!
//! Only [`Workspace::find_root`] walks *up*. Everything else takes the root as
//! a parameter, which keeps the implicit "where am I" question in one place
//! with one caller.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use thiserror::Error;

use crate::manifest::{Manifest, ManifestError};
use crate::resolver::{ImporterPath, ImporterPathError};

/// Directories a member glob never descends into.
///
/// `node_modules` is the load-bearing one: it holds thousands of directories
/// that all contain a `package.json`, and `packages/**` would otherwise adopt
/// every one of them as a member.
const PRUNED: &[&str] = &["node_modules", ".git"];

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error("workspace pattern `{pattern}` does not name a directory inside the workspace")]
    EscapesRoot {
        pattern: String,
        #[source]
        source: ImporterPathError,
    },
    #[error("workspace pattern `{pattern}` is not a valid glob")]
    BadPattern {
        pattern: String,
        #[source]
        source: globset::Error,
    },
    #[error("workspace members {first} and {second} are both named `{name}`")]
    DuplicateName {
        name: String,
        first: PathBuf,
        second: PathBuf,
    },
    #[error("failed to access {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Something worth telling the user about that is not worth failing over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Warning {
    /// Almost always a typo in `workspaces`. Not fatal, because a repo may
    /// legitimately keep a pattern for a directory it has not created yet.
    PatternMatchedNothing(String),
}

/// One project in the workspace.
#[derive(Debug)]
pub struct Member {
    /// Workspace-relative, and the same key the lockfile's importers map uses.
    pub importer: ImporterPath,
    /// Absolute, because the linker writes into this directory.
    pub path: PathBuf,
    pub manifest: Manifest,
}

#[derive(Debug)]
pub struct Workspace {
    root: PathBuf,
    members: BTreeMap<ImporterPath, Member>,
    warnings: Vec<Warning>,
}

impl Workspace {
    /// The absolute directory holding the workspace's root `package.json`.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn members(&self) -> &BTreeMap<ImporterPath, Member> {
        &self.members
    }

    pub fn warnings(&self) -> &[Warning] {
        &self.warnings
    }

    /// Read the root manifest and expand its `workspaces` patterns.
    pub fn discover(root: &Path) -> Result<Self, WorkspaceError> {
        let manifest = Manifest::load(root)?;
        // Canonicalized once, here, so that `member_for` can compare paths by
        // prefix without every caller having to hand in a resolved path.
        let root = root.canonicalize().map_err(|source| WorkspaceError::Io {
            path: root.to_path_buf(),
            source,
        })?;

        let patterns = manifest.workspaces();
        let globs = MemberGlobs::compile(&patterns)?;

        // The root is a member whether or not it declares dependencies:
        // tooling-only dependencies commonly live there, and treating it as an
        // ordinary member keyed `.` is what removes the special case.
        let mut members = BTreeMap::new();
        members.insert(
            ImporterPath::root(),
            Member {
                importer: ImporterPath::root(),
                path: root.clone(),
                manifest,
            },
        );

        let mut matched = vec![false; patterns.len()];
        collect(&root, Path::new(""), &globs, &mut |relative, hits| {
            let path = root.join(relative);
            let manifest = match Manifest::load(&path) {
                Ok(manifest) => manifest,
                // A match without a `package.json` is a stray directory, not a
                // member: `packages/*` in a repo with a `packages/.cache`
                // should not fail an install.
                Err(ManifestError::NotFound(_)) => return Ok(()),
                Err(source) => return Err(source.into()),
            };

            // A pattern counts as matched once it yields a *member*. A pattern
            // that finds only manifest-less directories has told the user
            // nothing they wanted, so it is still worth the warning.
            for hit in hits {
                matched[*hit] = true;
            }

            let importer = ImporterPath::new(relative.to_string_lossy().as_ref())
                .expect("built from the Normal components of a directory below the root");
            members.insert(
                importer.clone(),
                Member {
                    importer,
                    path,
                    manifest,
                },
            );
            Ok(())
        })?;

        let warnings = patterns
            .iter()
            .zip(&matched)
            .filter(|(_, hit)| !**hit)
            .map(|(pattern, _)| Warning::PatternMatchedNothing(pattern.clone()))
            .collect();

        check_for_duplicate_names(&members)?;

        Ok(Workspace {
            root,
            members,
            warnings,
        })
    }

    /// The member whose directory most closely encloses `dir`.
    ///
    /// Nearest rather than any, because `packages/ui/src` sits inside both
    /// `packages/ui` and the root, and only one of them is the project the
    /// user is standing in — §7 of the workspace design settles that as "the
    /// nearest enclosing member". Returns `None` outside the workspace
    /// entirely, where naming a member would be a guess rather than an answer.
    ///
    /// A directory inside the workspace but inside no other member therefore
    /// answers the root, which is an ordinary member and not a fallback.
    /// Whether *standing* there should be an error is §10's open question, and
    /// it belongs to the command rather than here: `Member::path` is the
    /// member's own directory, so a caller that cares can still tell "at the
    /// root" from "lost somewhere under it".
    pub fn member_for(&self, dir: &Path) -> Option<&Member> {
        // A caller's `dir` is whatever the shell handed them, so it is
        // resolved to compare against the canonical root. Falling back to the
        // path as given covers a directory that does not exist yet; it simply
        // fails the prefix test below, which is the honest answer.
        let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        if !dir.starts_with(&self.root) {
            return None;
        }

        // Longest matching prefix is the nearest enclosing member. `Path`
        // compares component-wise, so `packages/ui-kit` does not match a
        // `packages/ui` member.
        self.members
            .values()
            .filter(|member| dir.starts_with(&member.path))
            .max_by_key(|member| member.path.components().count())
    }

    /// The workspace root governing `from`.
    ///
    /// Prefers an ancestor that declares `workspaces` *and actually claims
    /// `from` as a member*, because a member's own `package.json` must not
    /// stop the walk — running `jerky install` inside `packages/ui` installs
    /// the workspace. Checking membership rather than stopping at the first
    /// `workspaces` field is what keeps an unrelated project that merely sits
    /// underneath a monorepo from being installed into that monorepo.
    ///
    /// Falls back to the nearest manifest of any kind, which is what makes a
    /// plain single project work without declaring anything.
    pub fn find_root(from: &Path) -> Option<PathBuf> {
        let from = from.canonicalize().unwrap_or_else(|_| from.to_path_buf());
        let mut nearest_manifest = None;

        for dir in from.ancestors() {
            let Ok(manifest) = Manifest::load(dir) else {
                continue;
            };
            if !manifest.workspaces().is_empty() && claims(dir, &from) {
                return Some(dir.to_path_buf());
            }
            nearest_manifest.get_or_insert_with(|| dir.to_path_buf());
        }

        nearest_manifest
    }
}

/// Does the workspace rooted at `root` contain `dir` as one of its members?
///
/// A root that cannot be discovered at all still counts as claiming what it
/// encloses. Returning it hands the caller a workspace that will fail loudly
/// with the real error — a duplicate member name, say — which is better than
/// stepping quietly past it and installing somewhere else.
fn claims(root: &Path, dir: &Path) -> bool {
    match Workspace::discover(root) {
        Err(_) => true,
        Ok(workspace) => workspace
            .member_for(dir)
            .is_some_and(|member| !member.importer.is_root()),
    }
}

/// The compiled `workspaces` patterns, and how deep they can reach.
///
/// The depth limit travels with the matcher because it is derived from it and
/// is meaningless apart from it: `packages/*` cannot match anything below two
/// levels, so nothing below two levels needs to be read, and that is what
/// keeps discovery from walking an entire repository.
struct MemberGlobs {
    set: GlobSet,
    /// `None` once a `**` gives up the bound, which is the price of asking
    /// for an unbounded pattern.
    depth_limit: Option<usize>,
}

impl MemberGlobs {
    fn compile(patterns: &[String]) -> Result<Self, WorkspaceError> {
        let mut builder = GlobSetBuilder::new();
        let mut depth_limit = Some(0);

        for pattern in patterns {
            // npm permits `../siblings/*`. Silently adopting a directory
            // outside the repo is the tar-slip shape `archive` already guards
            // against, so it is refused rather than resolved — and it is
            // refused by the type that already owns this question for
            // hand-edited lockfiles, rather than by a second rule that would
            // have to be kept in agreement with it. The normalized path is
            // discarded; only the verdict is wanted.
            //
            // A brace alternation hiding an escape, `{a,../b}/*`, is not seen
            // here, since it is one path component until globset expands it.
            // That costs nothing: `collect` only ever matches directories it
            // walked down to from the root, so an escaping alternative matches
            // nothing and earns the "matched nothing" warning.
            ImporterPath::new(pattern.as_str()).map_err(|source| WorkspaceError::EscapesRoot {
                pattern: pattern.clone(),
                source,
            })?;

            let glob = GlobBuilder::new(pattern)
                // Without this, `*` matches across `/` and `packages/*` would
                // adopt every descendant of `packages`, not its children.
                .literal_separator(true)
                .build()
                .map_err(|source| WorkspaceError::BadPattern {
                    pattern: pattern.clone(),
                    source,
                })?;
            builder.add(glob);

            depth_limit = match depth_limit {
                Some(limit) if !pattern.contains("**") => {
                    Some(limit.max(Path::new(pattern).components().count()))
                }
                _ => None,
            };
        }

        let set = builder
            .build()
            .expect("every glob in the set was built individually");
        Ok(MemberGlobs { set, depth_limit })
    }

    /// Could any pattern match something below a directory at `depth`?
    fn can_reach_below(&self, depth: usize) -> bool {
        self.depth_limit.is_none_or(|limit| depth < limit)
    }

    fn matches(&self, relative: &Path) -> Vec<usize> {
        self.set.matches(relative)
    }
}

/// Walk directories below `root`, handing each glob match to `found`.
///
/// Symlinked directories are skipped. A pattern is forbidden from naming a
/// directory outside the workspace, and following a link would be a second
/// door to the same place.
fn collect(
    root: &Path,
    relative: &Path,
    globs: &MemberGlobs,
    found: &mut impl FnMut(&Path, &[usize]) -> Result<(), WorkspaceError>,
) -> Result<(), WorkspaceError> {
    if !globs.can_reach_below(relative.components().count()) {
        return Ok(());
    }

    let dir = root.join(relative);
    let entries = std::fs::read_dir(&dir).map_err(|source| WorkspaceError::Io {
        path: dir.clone(),
        source,
    })?;

    for entry in entries {
        let entry = entry.map_err(|source| WorkspaceError::Io {
            path: dir.clone(),
            source,
        })?;
        let name = entry.file_name();
        if PRUNED.iter().any(|pruned| *pruned == name) {
            continue;
        }
        let metadata = entry
            .path()
            .symlink_metadata()
            .map_err(|source| WorkspaceError::Io {
                path: entry.path(),
                source,
            })?;
        if !metadata.is_dir() {
            continue;
        }

        let child = relative.join(&name);
        let hits = globs.matches(&child);
        if !hits.is_empty() {
            found(&child, &hits)?;
        }
        collect(root, &child, globs, found)?;
    }

    Ok(())
}

/// Two members claiming one package name is invalid.
///
/// The message names both directories rather than the duplicated key, because
/// membership is a set of directories: a name collision is found by looking at
/// paths, so paths are what the user needs to go fix it.
fn check_for_duplicate_names(
    members: &BTreeMap<ImporterPath, Member>,
) -> Result<(), WorkspaceError> {
    let mut seen: BTreeMap<&str, &ImporterPath> = BTreeMap::new();

    for (importer, member) in members {
        // An unnamed package cannot collide and cannot be depended on by name
        // either, so it is left alone rather than refused.
        let Some(name) = member.manifest.name() else {
            continue;
        };
        if let Some(first) = seen.insert(name, importer) {
            return Err(WorkspaceError::DuplicateName {
                name: name.to_string(),
                first: PathBuf::from(first.as_str()),
                second: PathBuf::from(importer.as_str()),
            });
        }
    }

    Ok(())
}
