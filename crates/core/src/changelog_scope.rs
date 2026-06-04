//! The single per-crate path-scope resolver shared by every changelog
//! renderer (`keep-a-changelog`, `json`, and the `release-notes` stage).
//!
//! All three formats once derived their commit path-scope independently and
//! diverged: the `release-notes` stage REPLACED a crate's own path with the
//! global `changelog.paths` whenever it was set, so every track in a
//! multi-track repo resolved to the same paths and rendered identical
//! sections; the `keep-a-changelog` and `json` engines scoped each track to
//! its own directory and ignored `changelog.paths` entirely. Routing all
//! three through [`resolve_changelog_scope`] makes the scope a single
//! computed value they cannot drift on.
//!
//! The model:
//! - a **per-crate track** scopes to that crate's own directory;
//! - an **aggregate** (single-crate, lockstep, or flat-aggregate — the
//!   synthesized root crate whose `path` is empty) scopes to the union of
//!   every crate directory plus the workspace manifests (`Cargo.toml`,
//!   `Cargo.lock`), falling back to the monorepo dir / whole repo when there
//!   are no crate directories;
//! - `changelog.paths`, when set, only ever NARROWS the derived scope by
//!   intersection — it can no longer replace it. A `changelog.paths` that is
//!   a superset of the derived scope (e.g. `crates/**` over every crate dir)
//!   intersects to the derived scope, i.e. becomes a no-op.

/// Workspace manifests that belong to the aggregate scope: a commit touching
/// only `Cargo.toml` / `Cargo.lock` (e.g. a workspace-wide dependency bump)
/// is part of the workspace's combined history even though it touches no
/// single crate directory.
pub const WORKSPACE_MANIFESTS: &[&str] = &["Cargo.toml", "Cargo.lock"];

/// The resolved commit path-scope for one changelog track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangelogScope {
    /// Directory / manifest pathspecs to pass to `git log -- <dirs>`. An empty
    /// vec means "whole repo" (no `--` path filter): the aggregate of a repo
    /// with no declared crate directories and no monorepo dir.
    pub dirs: Vec<String>,
    /// Optional `changelog.paths` glob filter that INTERSECTS [`Self::dirs`].
    ///
    /// `None` means no narrowing is required: either `changelog.paths` was
    /// unset, or it is a superset of the derived `dirs` (so the intersection
    /// equals `dirs` and the git pathspec alone is exact). `Some(globs)` means
    /// `changelog.paths` genuinely narrows the derived scope and the fetched
    /// commits' touched-file lists must additionally be filtered against
    /// `globs` for a precise result.
    pub narrow: Option<Vec<String>>,
}

impl ChangelogScope {
    /// The git pathspec form of [`Self::dirs`] for `git log -- <dirs>`.
    /// Returns an empty slice for the whole-repo aggregate.
    pub fn pathspecs(&self) -> &[String] {
        &self.dirs
    }

    /// Whether a commit touching `touched_files` (workspace-relative paths)
    /// survives the [`Self::narrow`] intersect.
    ///
    /// Returns `true` for every commit when no narrowing is required
    /// ([`Self::narrow`] is `None`). When narrowing is required, the commit is
    /// kept iff at least one of its touched files matches a `changelog.paths`
    /// glob — the precise intersection of the git-pathspec-derived `dirs`
    /// (already applied at fetch time) with the `changelog.paths` filter.
    pub fn commit_survives_narrow(&self, touched_files: &[String]) -> bool {
        let Some(ref globs) = self.narrow else {
            return true;
        };
        touched_files
            .iter()
            .any(|f| globs.iter().any(|g| path_matches_glob(g, f)))
    }
}

/// Match a workspace-relative file path against a `changelog.paths` glob.
///
/// Uses the `glob` crate's `Pattern` for full `**` / `*` / `?` support, with a
/// literal directory-prefix fallback (`crates` matches `crates/core/lib.rs`)
/// so a bare directory entry behaves like a git pathspec. An unparseable glob
/// degrades to the literal-prefix test rather than dropping the commit.
fn path_matches_glob(glob: &str, file: &str) -> bool {
    let glob = glob.trim().trim_start_matches("./");
    let file = file.trim().trim_start_matches("./");
    if glob.is_empty() || glob == "." {
        return true;
    }
    if let Ok(pat) = glob::Pattern::new(glob)
        && pat.matches(file)
    {
        return true;
    }
    // Literal directory-prefix: `crates` matches `crates/core/lib.rs`.
    file == glob || file.starts_with(&format!("{glob}/"))
}

/// Normalize a path for use as a git pathspec / glob subject: trim, and drop a
/// `./` prefix or a single `.` (which would scope to the whole repo and is
/// better expressed as an empty `dirs`). Returns `None` for empty / `.`.
fn normalize_dir(path: &str) -> Option<String> {
    let trimmed = path.trim().trim_start_matches("./");
    if trimmed.is_empty() || trimmed == "." {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Whether `dir` is covered by at least one glob in `paths` — i.e. every file
/// under `dir` would also match `paths`, so intersecting `paths` with `dir`
/// leaves `dir` unchanged.
///
/// A literal prefix match (`crates/core` under `crates`) and a `**` glob
/// (`crates/core` under `crates/**`) both count as coverage. The check is
/// conservative: when in doubt it reports "not covered", which only ever costs
/// a precise post-filter pass (never a silently-wrong widen).
fn dir_covered_by(dir: &str, paths: &[String]) -> bool {
    paths.iter().any(|p| {
        let Some(p) = normalize_dir(p) else {
            // A `.` / empty path in `changelog.paths` means the whole repo,
            // which trivially covers every dir.
            return true;
        };
        if glob_covers_dir(&p, dir) {
            return true;
        }
        // Literal directory-prefix coverage: `crates` covers `crates/core`.
        dir == p || dir.starts_with(&format!("{p}/"))
    })
}

/// Whether a glob pattern `p` (e.g. `crates/**`) covers every path under
/// directory `dir` (e.g. `crates/core`). True when stripping a trailing
/// `/**`, `/*`, or `**` recursion segment leaves a literal prefix of `dir`.
fn glob_covers_dir(p: &str, dir: &str) -> bool {
    for suffix in ["/**", "/**/*", "/*", "**"] {
        if let Some(base) = p.strip_suffix(suffix) {
            let base = base.trim_end_matches('/');
            if base.is_empty() {
                // `**` / `/*` at the root covers everything.
                return true;
            }
            if dir == base || dir.starts_with(&format!("{base}/")) {
                return true;
            }
        }
    }
    false
}

/// Resolve the commit path-scope for one changelog track.
///
/// - `crate_path` is the current track's directory relative to the workspace
///   root; empty / `.` marks the aggregate track.
/// - `all_crate_dirs` are every declared crate's directory (relative to the
///   workspace root); used to build the aggregate union.
/// - `monorepo_dir` is the optional `monorepo.dir`, the aggregate fallback
///   when no crate directories are declared.
/// - `changelog_paths` is the optional, already-template-rendered
///   `changelog.paths`; it can only narrow the derived scope.
pub fn resolve_changelog_scope(
    crate_path: &str,
    all_crate_dirs: &[String],
    monorepo_dir: Option<&str>,
    changelog_paths: &[String],
) -> ChangelogScope {
    // An empty `crate_path` always marks the aggregate. A non-empty path is
    // also the aggregate when it is the SOLE declared crate (single-crate
    // mode): the lone crate's release IS the whole-workspace release, so it
    // must additionally cover the workspace manifests.
    let is_sole_crate = match normalize_dir(crate_path) {
        Some(ref dir) => {
            let distinct: Vec<String> = all_crate_dirs
                .iter()
                .filter_map(|d| normalize_dir(d))
                .collect();
            distinct.len() == 1 && distinct.first() == Some(dir)
        }
        None => false,
    };

    let derived = match normalize_dir(crate_path) {
        // Per-crate track (one of several crates): scope to its own directory.
        Some(dir) if !is_sole_crate => vec![dir],
        // Aggregate track (empty path, or the sole crate): the union of every
        // crate dir plus the workspace manifests, or the monorepo dir / whole
        // repo when no crate directories are declared.
        _ => {
            let mut dirs: Vec<String> = Vec::new();
            for d in all_crate_dirs {
                if let Some(d) = normalize_dir(d)
                    && !dirs.contains(&d)
                {
                    dirs.push(d);
                }
            }
            if dirs.is_empty() {
                match monorepo_dir.and_then(normalize_dir) {
                    Some(dir) => vec![dir],
                    None => Vec::new(),
                }
            } else {
                for m in WORKSPACE_MANIFESTS {
                    dirs.push((*m).to_string());
                }
                dirs
            }
        }
    };

    let narrow = resolve_narrow(&derived, changelog_paths);
    ChangelogScope {
        dirs: derived,
        narrow,
    }
}

/// Decide whether `changelog.paths` requires a precise post-filter narrowing
/// pass over the derived `dirs`.
///
/// Returns `None` (no narrowing) when `changelog.paths` is unset or is a
/// superset of every derived dir; returns `Some(paths)` when it genuinely
/// narrows the scope.
fn resolve_narrow(derived: &[String], changelog_paths: &[String]) -> Option<Vec<String>> {
    let paths: Vec<String> = changelog_paths
        .iter()
        .filter(|p| !p.trim().is_empty())
        .cloned()
        .collect();
    if paths.is_empty() {
        return None;
    }
    // Whole-repo aggregate (no derived dirs): any non-empty `changelog.paths`
    // is a genuine narrowing of "everything".
    if derived.is_empty() {
        return Some(paths);
    }
    // `changelog.paths` is a no-op only when it covers EVERY derived dir; if a
    // single derived dir is not covered, the paths narrow the scope.
    let superset = derived.iter().all(|d| dir_covered_by(d, &paths));
    if superset { None } else { Some(paths) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn per_crate_scopes_to_its_own_dir() {
        let scope =
            resolve_changelog_scope("crates/core", &s(&["crates/core", "crates/cli"]), None, &[]);
        assert_eq!(scope.dirs, s(&["crates/core"]));
        assert_eq!(scope.narrow, None);
    }

    #[test]
    fn per_crate_ignores_global_paths_when_superset() {
        // The cfgd shape: a global `crates/**` over a per-crate track narrows
        // to a no-op (paths ⊇ derived ⇒ derived).
        let scope = resolve_changelog_scope(
            "crates/core",
            &s(&["crates/core", "crates/cli"]),
            None,
            &s(&["crates/**", "Cargo.toml", "Cargo.lock"]),
        );
        assert_eq!(scope.dirs, s(&["crates/core"]));
        assert_eq!(scope.narrow, None, "superset paths must not narrow");
    }

    #[test]
    fn per_crate_narrows_when_paths_exclude_the_dir() {
        // A `changelog.paths` that doesn't cover this crate's dir genuinely
        // narrows: the precise intersect must run. (Two crates ⇒ a genuine
        // per-crate track, not the sole-crate aggregate.)
        let scope = resolve_changelog_scope(
            "crates/core",
            &s(&["crates/core", "crates/cli"]),
            None,
            &s(&["docs/**"]),
        );
        assert_eq!(scope.dirs, s(&["crates/core"]));
        assert_eq!(scope.narrow, Some(s(&["docs/**"])));
    }

    #[test]
    fn sole_crate_is_an_aggregate_with_manifests() {
        // A single-crate repo's lone crate IS the whole-workspace release, so
        // it scopes to its dir + the workspace manifests, not the dir alone.
        let scope = resolve_changelog_scope("crates/app", &s(&["crates/app"]), None, &[]);
        assert_eq!(scope.dirs, s(&["crates/app", "Cargo.toml", "Cargo.lock"]));
        assert_eq!(scope.narrow, None);
    }

    #[test]
    fn aggregate_unions_crate_dirs_plus_manifests() {
        let scope = resolve_changelog_scope("", &s(&["crates/core", "crates/cli"]), None, &[]);
        assert_eq!(
            scope.dirs,
            s(&["crates/core", "crates/cli", "Cargo.toml", "Cargo.lock"])
        );
        assert_eq!(scope.narrow, None);
    }

    #[test]
    fn aggregate_dot_path_is_aggregate() {
        let scope = resolve_changelog_scope(".", &s(&["crates/core"]), None, &[]);
        assert_eq!(scope.dirs, s(&["crates/core", "Cargo.toml", "Cargo.lock"]));
    }

    #[test]
    fn aggregate_no_crate_dirs_falls_back_to_monorepo_dir() {
        let scope = resolve_changelog_scope("", &[], Some("packages/app"), &[]);
        assert_eq!(scope.dirs, s(&["packages/app"]));
    }

    #[test]
    fn aggregate_no_crate_dirs_no_monorepo_is_whole_repo() {
        let scope = resolve_changelog_scope("", &[], None, &[]);
        assert!(scope.dirs.is_empty(), "whole repo = empty pathspec");
        assert_eq!(scope.narrow, None);
    }

    #[test]
    fn aggregate_superset_paths_are_a_noop() {
        let scope = resolve_changelog_scope(
            "",
            &s(&["crates/core", "crates/cli"]),
            None,
            &s(&["crates/**", "Cargo.toml", "Cargo.lock"]),
        );
        assert_eq!(
            scope.dirs,
            s(&["crates/core", "crates/cli", "Cargo.toml", "Cargo.lock"])
        );
        assert_eq!(
            scope.narrow, None,
            "crates/** + manifests ⊇ derived ⇒ no narrowing"
        );
    }

    #[test]
    fn aggregate_partial_paths_narrow() {
        // `crates/**` covers the crate dirs but NOT the manifests in the
        // derived union, so it narrows (drops manifest-only commits).
        let scope = resolve_changelog_scope(
            "",
            &s(&["crates/core", "crates/cli"]),
            None,
            &s(&["crates/**"]),
        );
        assert_eq!(scope.narrow, Some(s(&["crates/**"])));
    }

    #[test]
    fn whole_repo_aggregate_with_paths_narrows() {
        let scope = resolve_changelog_scope("", &[], None, &s(&["src/**"]));
        assert!(scope.dirs.is_empty());
        assert_eq!(scope.narrow, Some(s(&["src/**"])));
    }

    #[test]
    fn glob_covers_dir_handles_recursion_suffixes() {
        assert!(glob_covers_dir("crates/**", "crates/core"));
        assert!(glob_covers_dir("crates/*", "crates/core"));
        assert!(glob_covers_dir("crates/**/*", "crates/core"));
        assert!(glob_covers_dir("**", "anything/here"));
        assert!(!glob_covers_dir("docs/**", "crates/core"));
    }

    #[test]
    fn literal_prefix_covers_nested_dir() {
        assert!(dir_covered_by("crates/core", &s(&["crates"])));
        assert!(dir_covered_by("crates", &s(&["crates"])));
        assert!(!dir_covered_by("cratesx", &s(&["crates"])));
    }

    #[test]
    fn no_narrow_keeps_every_commit() {
        let scope = ChangelogScope {
            dirs: s(&["crates/core"]),
            narrow: None,
        };
        assert!(scope.commit_survives_narrow(&s(&["anything/at/all.rs"])));
        assert!(scope.commit_survives_narrow(&[]));
    }

    #[test]
    fn narrow_keeps_commit_touching_a_matching_file() {
        let scope = ChangelogScope {
            dirs: Vec::new(),
            narrow: Some(s(&["crates/**"])),
        };
        assert!(scope.commit_survives_narrow(&s(&["crates/core/src/lib.rs"])));
        assert!(scope.commit_survives_narrow(&s(&["README.md", "crates/cli/main.rs"])));
        assert!(!scope.commit_survives_narrow(&s(&["docs/guide.md"])));
        assert!(!scope.commit_survives_narrow(&[]));
    }

    #[test]
    fn path_matches_glob_supports_recursion_and_literal_prefix() {
        assert!(path_matches_glob("crates/**", "crates/core/src/lib.rs"));
        assert!(path_matches_glob("crates", "crates/core/src/lib.rs"));
        assert!(path_matches_glob("Cargo.toml", "Cargo.toml"));
        assert!(path_matches_glob("*.toml", "Cargo.toml"));
        assert!(!path_matches_glob("crates/**", "docs/guide.md"));
        assert!(path_matches_glob(".", "anything"));
    }
}
