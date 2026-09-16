use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use jerky::cli::{Cli, Command};
use jerky::commands::install::{Mode, Skipped};
use jerky::error::JerkyError;
use jerky::linker::{BinCollision, Unowned, UnownedReason};
use jerky::platform::Platform;
use jerky::resolver::{ImporterPath, Kind, UnsatisfiedPeer};
use jerky::workspace::{Warning, Workspace};

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
    let project_dir = std::env::current_dir().map_err(JerkyError::Cwd)?;

    match cli.command {
        Command::Init => {
            // Deliberately not workspace-aware: `jerky init` in a
            // subdirectory does not register the new package as a member,
            // because editing `workspaces` is the user's decision and
            // silently rewriting the root manifest would be surprising.
            let path = jerky::commands::init::init(&project_dir)?;
            println!("wrote {}", path.display());
            Ok(())
        }
        Command::Install {
            spec,
            save_dev,
            production,
        } => {
            let spec = spec
                .as_deref()
                .map(jerky::cli::parse_package_spec)
                .transpose()?;

            let root = Workspace::find_root(&project_dir)
                .ok_or_else(|| JerkyError::NoManifestAnywhere(project_dir.clone()))?;
            let workspace = Workspace::discover(&root)?;
            for warning in workspace.warnings() {
                match warning {
                    Warning::PatternMatchedNothing(pattern) => {
                        eprintln!("warning: workspace pattern `{pattern}` matched no package");
                    }
                }
            }

            let store = jerky::store::Store::new(store_root()?);
            let registry = jerky::metadata_cache::CachedRegistry::new(
                registry_client(),
                jerky::metadata_cache::MetadataCache::new(
                    cache_root()?,
                    jerky::metadata_cache::DEFAULT_WINDOW,
                ),
            );

            match spec {
                // Which importer the user is standing in is asked *only* here.
                // A bare install acts on every one of them, so there is no
                // guess for `importer_for` to refuse — see its own doc comment.
                Some(spec) => {
                    let importer = importer_for(&workspace, &project_dir)?;
                    // `None` rather than `Some(Kind::Prod)`: without
                    // `--save-dev` the command names a package and not a
                    // section, and the two are not the same instruction. The
                    // install reads the section off the manifest for the
                    // first, and would overwrite it for the second.
                    let kind = save_dev.then_some(Kind::Dev);
                    let outcome = jerky::commands::install::install(
                        &workspace, &importer, &store, &registry, &spec, kind,
                    )?;
                    report_unowned(&outcome.left_alone);
                    report_unsatisfied_peers(&outcome.unsatisfied_peers);
                    report_bin_collisions(&outcome.bin_collisions);
                    report_skipped(&outcome.skipped, &outcome.platform);
                    let added = outcome
                        .recorded
                        .expect("an install always reports what it recorded");
                    println!("added {}@{} to {importer}", added.name, added.version);
                }
                // `--production` takes this arm too: clap has already refused
                // it alongside a package, so a mode that acts on every importer
                // never has to ask which one the user meant.
                None => {
                    let mode = if production {
                        Mode::Production
                    } else {
                        Mode::Develop
                    };
                    let outcome =
                        jerky::commands::install::sync(&workspace, &store, &registry, None, mode)?;
                    report_unowned(&outcome.left_alone);
                    report_unsatisfied_peers(&outcome.unsatisfied_peers);
                    report_bin_collisions(&outcome.bin_collisions);
                    report_skipped(&outcome.skipped, &outcome.platform);
                    // The packages, not the links: two importers on one version
                    // share a store entry, and reporting the link count would
                    // make the same install read differently in a monorepo.
                    println!(
                        "installed {} across {}",
                        plural(outcome.linked.len(), "package"),
                        plural(workspace.members().len(), "importer")
                    );
                }
            }
            Ok(())
        }
    }
}

/// Say what convergence found in a `node_modules` and did not remove.
///
/// Warnings rather than failures, and written here rather than in the linker
/// for the same reason `Warning::PatternMatchedNothing` is: the module knows
/// what it found, and this is the layer that knows the user is reading a
/// terminal. An install that stopped on a directory a previous `npm install`
/// left behind would make the first jerky run in a half-migrated repository a
/// thing to be afraid of; one that said nothing would leave the user wondering
/// why a package they can still `require` is in no lockfile.
fn report_unowned(entries: &[Unowned]) {
    for entry in entries {
        let path = entry.path.display();
        match entry.reason {
            UnownedReason::NotASymlink => {
                eprintln!("warning: left {path} alone — jerky did not create it");
            }
            UnownedReason::PointsOutside => {
                eprintln!("warning: left {path} alone — it links outside this workspace");
            }
        }
    }
}

/// Say which bin names two dependencies both wanted.
///
/// Beside the others, for the same division: the linker knows which name it
/// had to give away and this is the layer that knows a terminal is reading.
///
/// One line each rather than a count, because the point of saying anything is
/// that the reader can act on it — the fix is to call the loser through its
/// package rather than through `.bin`, and that needs both names.
fn report_bin_collisions(collisions: &[BinCollision]) {
    for collision in collisions {
        let BinCollision {
            bin_dir,
            name,
            winner,
            loser,
        } = collision;

        eprintln!(
            "warning: {winner} and {loser} both publish a `{name}` binary — \
             {} links {winner}'s",
            bin_dir.display()
        );
    }
}

/// Say which required peers the tree does not answer.
///
/// One line each, to stderr, on every install — not behind `--verbose`, which
/// would reproduce the silence this was filed about, and not collapsed into a
/// count, which names a number nobody can act on. Standing beside
/// [`report_unowned`] for the same reason it is here rather than in the
/// resolver: the pass knows what it found, and this is the layer that knows a
/// terminal is reading.
///
/// The install has already succeeded by the time these print. An unsatisfied
/// peer is a warning because peer ranges across the live ecosystem routinely
/// lag a major release, and a package manager that refused those trees would
/// be unable to install most of npm.
fn report_unsatisfied_peers(peers: &[UnsatisfiedPeer]) {
    for peer in peers {
        eprintln!("warning: {}", peer_warning(peer));
    }
}

/// One unmet peer as the sentence to print.
///
/// Missing and out-of-range read differently on purpose. They are different
/// problems with different fixes — install the package, or reconcile two
/// versions of it — and a shared wording would leave the reader to work out
/// which they have. Naming the version that *was* found is the whole of what
/// makes the second one actionable.
///
/// The dependent is rendered by its own `Display`, which for a package with no
/// peer context — which is what a complaint always carries — is exactly
/// `name@version`. That is the spelling a reader can go and find in a
/// `package.json`, where a store directory name is not.
fn peer_warning(peer: &UnsatisfiedPeer) -> String {
    let UnsatisfiedPeer {
        dependent,
        peer: name,
        range,
        found,
    } = peer;

    match found {
        Some(version) => format!(
            "{dependent} wants peer {name}@{range}, \
             but the nearest provider has {name}@{version}"
        ),
        None => format!("{dependent} wants peer {name}@{range}, which nothing provides"),
    }
}

/// Say how many optional dependencies this machine did not take.
///
/// A count and the platform, never the names, which is the one place this
/// deliberately differs from every other report here. A skipped optional
/// dependency is the mechanism working rather than a problem to fix, and it is
/// routine at scale: a project using esbuild, rollup and swc skips some
/// seventy platform variants on every install, forever. Seventy lines of
/// correct behaviour per run is how a tool teaches people to stop reading its
/// output. The names are not lost — the lockfile records every one of them,
/// with the `os` and `cpu` that ruled it out.
///
/// stdout rather than stderr, and no `warning:` prefix, because this is a
/// statement about what the install did rather than a complaint about it.
fn report_skipped(skipped: &[Skipped], platform: &Platform) {
    if skipped.is_empty() {
        return;
    }
    println!(
        "skipped {} unsupported on {platform}",
        plural(skipped.len(), "optional package")
    );
}

/// Which importer the user is standing in.
///
/// `member_for` answers with the nearest enclosing member, which for a
/// directory belonging to no other member is the root. That answer is right
/// when the root is the only member — there is nowhere else it could have
/// meant — and ambiguous when there are others: standing in `tools/scripts`
/// could mean the root, or could mean the user is lost. Ambiguity needs
/// alternatives to exist, so the error is raised only where alternatives do.
fn importer_for(workspace: &Workspace, dir: &Path) -> Result<ImporterPath, JerkyError> {
    let member = workspace
        .member_for(dir)
        .ok_or_else(|| JerkyError::NoManifestAnywhere(dir.to_path_buf()))?;

    let standing_in_its_own_directory = dir
        .canonicalize()
        .is_ok_and(|resolved| resolved == member.path);
    if member.importer.is_root() && !standing_in_its_own_directory && workspace.members().len() > 1
    {
        return Err(JerkyError::NotInAMember {
            directory: dir.to_path_buf(),
            members: workspace
                .members()
                .keys()
                .map(ImporterPath::to_string)
                .collect(),
        });
    }

    Ok(member.importer.clone())
}

/// Which registry to talk to, from `JERKY_REGISTRY_URL` or npm's.
///
/// `HttpRegistry` has taken a base URL since it was written and the
/// integration tests drive it that way, but the binary hardcoded the default
/// here, so nothing built out of `main` could be pointed anywhere else. The
/// benchmark is what noticed: a default run made ~22,600 requests at the live
/// registry, which is both a rate limit and the reason its medians could not
/// be reproduced. It now replays from a local mirror through this variable.
///
/// Read here rather than in `registry.rs` for the same reason `$HOME` is:
/// `main` is the only place allowed to read the environment, and everything
/// below it takes what it needs as a parameter.
///
/// An empty value means the same as an unset one. `JERKY_REGISTRY_URL=`
/// is how a shell says "not this one" when a parent exported it, and the
/// alternative — failing to resolve every package against the empty string —
/// is no reading of that intent at all. This is deliberately *not* the offline
/// mode of #80: pointing jerky at a different HTTP registry is still a
/// registry, and nothing here makes a missing one survivable.
fn registry_client() -> jerky::registry::HttpRegistry {
    match std::env::var("JERKY_REGISTRY_URL") {
        Ok(url) if !url.trim().is_empty() => {
            jerky::registry::HttpRegistry::with_base_url(url.trim())
        }
        _ => jerky::registry::HttpRegistry::new(),
    }
}

/// `main` is the only place allowed to read `$HOME`; everything below it takes
/// paths as parameters so tests never touch a developer's real store.
fn store_root() -> Result<PathBuf, JerkyError> {
    let home = dirs::home_dir().ok_or(JerkyError::NoHomeDirectory)?;
    Ok(home.join(".jerky").join("store"))
}

/// Beside the store, not inside it.
///
/// The store is content-addressed and immutable: an entry's name is the hash
/// of what is in it, and nothing ever rewrites one. The metadata cache is
/// neither — entries are keyed by package name and are overwritten every time
/// the registry answers. Sharing a directory would leave `jerky store prune`
/// unable to say which rule applies to what it finds.
fn cache_root() -> Result<PathBuf, JerkyError> {
    let home = dirs::home_dir().ok_or(JerkyError::NoHomeDirectory)?;
    Ok(home.join(".jerky").join("cache"))
}

/// `1 package`, `2 packages`.
///
/// Small, but this line is the first thing a developer sees after cloning a
/// repo, and `installed 1 packages across 1 importers` is not the impression
/// to make there.
fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::TempDir;

    fn write_manifest(dir: &Path, json: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("package.json"), json).unwrap();
    }

    /// A root with two members, plus a `tools/scripts` that is neither.
    fn monorepo(root: &Path) -> Workspace {
        write_manifest(root, r#"{"name":"ws","workspaces":["packages/*"]}"#);
        write_manifest(&root.join("packages/ui"), r#"{"name":"ui"}"#);
        write_manifest(&root.join("packages/api"), r#"{"name":"api"}"#);
        std::fs::create_dir_all(root.join("tools/scripts")).unwrap();
        Workspace::discover(root).unwrap()
    }

    #[test]
    fn a_spec_from_a_directory_belonging_to_no_member_is_refused() {
        // The nearest enclosing member of `tools/scripts` is the root, and
        // taking that answer would install into the root because the user
        // happened to be standing somewhere jerky does not manage. With other
        // members to have meant, that is a guess rather than an answer.
        let work = TempDir::new().unwrap();
        let workspace = monorepo(work.path());

        let err = importer_for(&workspace, &work.path().join("tools/scripts"))
            .expect_err("a directory belonging to no member is ambiguous");

        let JerkyError::NotInAMember { members, .. } = err else {
            panic!("expected NotInAMember, got {err}");
        };
        assert_eq!(members, [".", "packages/api", "packages/ui"]);
    }

    #[test]
    fn a_bare_install_asks_no_such_question() {
        // The asymmetry the CLI depends on: the same directory that cannot
        // name an importer still names a workspace, and a bare install wants
        // nothing else. If this ever stopped resolving, bare install in
        // `tools/scripts` would fail where `jerky install lodash` is merely
        // ambiguous.
        let work = TempDir::new().unwrap();
        monorepo(work.path());

        let root = Workspace::find_root(&work.path().join("tools/scripts"))
            .expect("the root manifest sits above it");
        assert_eq!(root, work.path().canonicalize().unwrap());
    }

    #[test]
    fn a_spec_from_inside_a_member_names_that_member() {
        let work = TempDir::new().unwrap();
        let workspace = monorepo(work.path());

        let importer = importer_for(&workspace, &work.path().join("packages/ui")).unwrap();
        assert_eq!(importer.to_string(), "packages/ui");
    }

    #[test]
    fn a_workspace_of_one_has_nothing_to_be_ambiguous_about() {
        // Ambiguity needs alternatives. In a single-project repo the root is
        // the only thing `tools/scripts` could have meant, so it is the
        // answer rather than an error.
        let work = TempDir::new().unwrap();
        let root = work.path();
        write_manifest(root, r#"{"name":"solo"}"#);
        std::fs::create_dir_all(root.join("tools/scripts")).unwrap();
        let workspace = Workspace::discover(root).unwrap();

        let importer = importer_for(&workspace, &root.join("tools/scripts")).unwrap();
        assert!(importer.is_root());
    }

    #[test]
    fn a_missing_peer_and_an_out_of_range_one_read_differently() {
        // The two are different problems — install the package, or reconcile
        // two versions of it — and one wording for both would leave the reader
        // to work out which they have.
        let unmet = |peer: &str, range: &str, found: Option<&str>| UnsatisfiedPeer {
            dependent: jerky::resolver::PackageId {
                name: "react-dom".to_string(),
                version: "18.2.0".to_string(),
                context: Default::default(),
            },
            peer: peer.to_string(),
            range: range.to_string(),
            found: found.map(str::to_string),
        };

        assert_eq!(
            peer_warning(&unmet("react", "^18.2.0", Some("17.0.2"))),
            "react-dom@18.2.0 wants peer react@^18.2.0, \
             but the nearest provider has react@17.0.2"
        );
        assert_eq!(
            peer_warning(&unmet("react", "^18.2.0", None)),
            "react-dom@18.2.0 wants peer react@^18.2.0, which nothing provides"
        );
    }

    #[test]
    fn the_summary_line_counts_in_the_singular() {
        assert_eq!(plural(0, "package"), "0 packages");
        assert_eq!(plural(1, "importer"), "1 importer");
        assert_eq!(plural(12, "package"), "12 packages");
    }
}
