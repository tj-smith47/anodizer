//! Nested subcommand trees and their arg structs.
//!
//! The root `Cli` and the top-level `Commands` enum live in the crate root;
//! these are the trees that hang off individual commands.

use clap::Subcommand;
use std::path::PathBuf;

/// Output format for `anodizer changelog`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum ChangelogFormat {
    /// Regenerate the `## [Unreleased]` section(s) of the configured
    /// `CHANGELOG.md` file(s) (the default). Previews to stdout; writes in
    /// place with `--write`.
    #[default]
    #[value(name = "keep-a-changelog", alias = "kac")]
    KeepAChangelog,
    /// GitHub-release-body markdown (grouped bullets) for the resolved range,
    /// to stdout. The historical `anodizer changelog` behavior.
    ReleaseNotes,
    /// Machine-readable JSON array of `{ crate, from, to, groups }` objects,
    /// one per selected crate, sorted by crate name.
    Json,
}

/// `anodizer tag` parent subcommand.
///
/// Bare `anodizer tag` keeps its existing autotag behavior (handled
/// by the `Tag` variant directly). `anodizer tag rollback` opts into
/// the failure-recovery flow described in
/// `commands::tag::rollback` (binary-only module, so not linkable from here).
#[derive(Subcommand)]
pub enum TagSub {
    /// Withdraw a release: unwind the publishers the run recorded, delete
    /// the anodizer-managed tags at a SHA, then revert (or reset past) the
    /// bump commit they point at.
    ///
    /// The publisher unwind reads the run state the release left under
    /// `dist/run-<tag>/`, so the tags being rolled back name the run — there
    /// is no run id to pass. A tag with no recorded state is a tag-only
    /// rollback.
    Rollback {
        #[arg(
            value_name = "sha",
            help = "Commit SHA to roll back from. Defaults to HEAD."
        )]
        sha: Option<String>,
        #[arg(long, help = "Print what would happen without mutating anything")]
        dry_run: bool,
        #[arg(
            long = "no-push",
            help = "Skip remote tag delete and branch push (local-only)"
        )]
        no_push: bool,
        #[arg(
            long,
            help = "Override the published-state guard: roll back even when the tag's run summary shows a one-way-door publisher (crates.io, chocolatey, winget, snapcraft, ...) accepted the version, when the crates.io index shows the tag's crate@version live (GLOBAL state — published by any prior run, not just this one; an unreachable index also refuses), or — when no summary exists — when a published (non-draft) GitHub release exists for the tag. Without it, rollback refuses because those registries never accept the same version twice: the version is burned and the only clean recovery is fixing forward"
        )]
        force: bool,
        #[arg(
            long,
            default_value = "all",
            help = "Tag-shape filter: all | lockstep | per-crate"
        )]
        scope: String,
        #[arg(
            long,
            default_value = "revert",
            help = "Rollback strategy: revert (default; history-preserving) | reset (opt-in; rewrites history, requires --force-with-lease to push)"
        )]
        mode: String,
        #[arg(
            long,
            value_name = "name",
            help = "Branch name to push the revert commit to. Usually unnecessary: the branch is auto-resolved from the bump commit via `git branch -r --contains <sha>`, which covers the ordinary CI tag-push case (detached HEAD, GITHUB_REF_NAME set to the tag). Needed only when that resolution is ambiguous or empty — the bump commit is on two or more remote branches, or on none and HEAD cannot be resolved either. Both cases fail with an error naming this flag. Pass --branch master (or whichever branch the bump commit was created on)."
        )]
        branch: Option<String>,
    },
}

/// The checks `anodizer check` can run: the config validator, the determinism
/// harness, and the `version_files` drift guard.
#[derive(Subcommand)]
pub enum CheckCmd {
    /// Validate the workspace's anodizer config.
    Config {
        #[arg(long, help = "Validate a specific workspace in a monorepo config")]
        workspace: Option<String>,
        #[arg(
            long,
            value_delimiter = ',',
            help = "Validate these skip tokens (stages or publishers) against the known set \
                    without running anything (comma-separated). Unified denylist: a stage name \
                    skips the stage, a publisher name (npm, homebrew, chocolatey, …) skips \
                    that publisher."
        )]
        skip: Vec<String>,
        #[arg(
            long = "publishers",
            value_delimiter = ',',
            help = concat!(
                "Validate-only: check that each name is a publisher the active config \
                 actually enables (a known but unconfigured publisher is rejected). ",
                "Comma-separated publishers to run (default: all configured). \
                 --skip always wins over --publishers.",
            )
        )]
        publishers: Vec<String>,
    },
    /// Run the determinism harness (build pipeline twice, diff artifacts).
    Determinism(CheckDeterminismArgs),
    /// Check that enrolled `version_files` still match each crate's current version.
    VersionFiles,
}

/// Arguments of `anodizer check determinism`: the run count, the stage and
/// target filters, preserved-dist reuse, and where the report is written.
#[derive(clap::Args)]
pub struct CheckDeterminismArgs {
    #[arg(
        long,
        default_value = "2",
        help = "Number of from-clean rebuilds to diff"
    )]
    pub runs: u32,
    #[arg(
        long,
        value_name = "stages",
        help = "Optional stage subset (build,source,upx,archive,nfpm,makeself,snapcraft,sbom,sign,checksum,cargo-package,docker,msi,nsis,dmg,pkg,srpm,appbundle,appimage,flatpak, plus the `installers` family selector expanding to nfpm,makeself,srpm,msi,nsis,dmg,pkg). Omit the flag to byte-verify the full OS-native partition for this host (Linux adds nfpm/makeself/snapcraft/srpm/docker/appimage/flatpak; macOS adds appbundle/dmg/pkg; Windows adds msi/nsis). The list is also the build filter: stages NOT named here are added to the child release's `--skip=` set, so a stage must be requested (or in the host default) to be byte-verified. `cargo-package` is harness-only — drives `cargo package --no-verify --allow-dirty` per workspace member to probe `.crate` byte-stability without hitting a registry; it is NOT in the host default and stays opt-in. `docker` is harness-only — drives `docker buildx build --output=type=oci,rewrite-timestamp=true,dest=…` against each configured `dockers_v2` entry's rendered dockerfile (with its `extra_files` and `build_args`, mirroring the production `docker` stage) to probe OCI image byte-stability without pushing to a registry; skipped when `docker buildx` is unavailable or the crate configures no `dockers_v2`. Installer stages (msi/nsis/dmg/pkg/srpm) plus appimage (needs `linuxdeploy`) and flatpak (needs `flatpak-builder`) are skipped at the gate when their backing tool is absent — a host-default stage warn-skips, an explicitly typed one hard-fails; `appbundle` is pure file assembly and always runs when requested."
    )]
    pub stages: Option<String>,
    #[arg(
        long,
        value_name = "csv",
        help = "Restrict the harness to a comma-separated subset of configured target triples. Used by the sharded release workflow so each runner only validates targets it can natively build (Linux runner skips macOS targets, etc.). Forwarded to the child `anodizer release --snapshot` subprocess."
    )]
    pub targets: Option<String>,
    #[arg(
        long,
        value_name = "path",
        help = "JSON report path; default dist/run-<id>/determinism.json"
    )]
    pub report: Option<PathBuf>,
    #[arg(
        long,
        conflicts_with = "no_snapshot",
        help = "Force snapshot mode on the child release subprocess (artifacts get a `-SNAPSHOT-<sha>` suffix). Default: auto — snapshot off when HEAD is at a tag, on otherwise."
    )]
    pub snapshot: bool,
    #[arg(
        long = "no-snapshot",
        conflicts_with = "snapshot",
        help = "Force snapshot mode OFF on the child release subprocess (artifacts emit the actual release version). Default: auto — see --snapshot."
    )]
    pub no_snapshot: bool,
    #[arg(
        long = "inject-drift",
        value_name = "stage",
        hide = true,
        help = "(TEST HARNESS) Append 1 random byte to the first artifact emitted by <stage>. Gated by ANODIZE_TEST_HARNESS=1."
    )]
    pub inject_drift: Option<String>,
    #[arg(
        long = "preserve-dist",
        value_name = "path",
        help = "When the harness greens, copy run-0's `<worktree>/dist/**` to <path> and emit `<path>/context.json` describing the artifact set. The release workflow's publish-only path consumes this to ship the determinism step's output directly (eliminates the redundant `build:` recompilation). Local operators can pass this too — useful for inspecting a hermetic dist tree without re-running the release pipeline."
    )]
    pub preserve_dist: Option<PathBuf>,
    #[arg(
        long = "crate",
        value_name = "name",
        help = "When --preserve-dist is set, write the preserved dist tree to \
                <dest>/<name>/ instead of directly into <dest>/. Used by the \
                sharded matrix to produce per-crate subdirectories so a \
                `release --publish-only` job can merge all crates into a single \
                dist/ without context.json collision."
    )]
    pub crate_name: Option<String>,
    /// Fail (not warn-skip) if any selected stage's backing tool is missing —
    /// used by CI so a default host-OS run cannot silently skip an OS-native
    /// producer.
    ///
    /// Without `--stages`, the harness builds the full host-OS partition
    /// (`commands::check::determinism`'s `default_stages_for_host`),
    /// and a host-default stage whose tool is absent normally warn-skips so dev
    /// boxes stay usable. CI provisions every OS-native tool and must treat a
    /// missing one as a hard failure: a silent skip is the exact false coverage
    /// that once hid the installer formats from every release. This flag
    /// promotes the WHOLE resolved stage set to the hard-fail contract that
    /// explicitly typed stages already get.
    #[arg(
        long = "require-tools",
        help = "Fail (not warn-skip) if any selected stage's backing tool is missing — used by CI so a default host-OS run cannot silently skip an OS-native producer."
    )]
    pub require_tools: bool,
}
