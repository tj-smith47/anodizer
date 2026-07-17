use super::*;

/// Rollback policy after the publish stage. `BestEffort` is the default when
/// pre-flight ran clean; `None` is the implicit default otherwise (callers
/// should warn that rollback is disabled). The CLI flag `--rollback=<v>`
/// sets `ContextOptions::rollback_mode` to `Some(v)` to override the
/// default-resolution at the dispatch site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RollbackMode {
    /// Do not attempt rollback. Useful when the operator wants to inspect
    /// half-published state before deciding.
    None,
    /// Run best-effort rollback for every reversible publisher whose
    /// evidence is present in the report. Most irreversible publishers
    /// (chocolatey moderation, winget PRs, AUR) are never rolled back —
    /// the Submitter gate is their only protection. The exception is
    /// cargo: a partial multi-crate publish that left live crates records
    /// them and gets those crates yanked even on a failed run.
    #[default]
    BestEffort,
}

/// Non-publisher `--skip` tokens for the `release` command: the pipeline
/// stage / phase names that are NOT publishers.
///
/// The publisher tokens are NOT listed here — they are derived from
/// [`PublisherKind`] and unioned in by [`VALID_RELEASE_SKIPS`], so the
/// `--skip` publisher vocabulary cannot drift from the registry. Keep ONLY
/// non-publisher stage tokens here.
///
/// Two pairs look like publishers but are stages and belong here:
/// `snapcraft` is the snap *build* stage (its publisher sibling is
/// `snapcraft-publish`), and `release` is the GitHub/GitLab/Gitea release
/// *stage* (its publisher sibling is `github-release`).
pub(super) const NON_PUBLISHER_RELEASE_SKIPS: &[&str] = &[
    "publish",
    "sign",
    "validate",
    "sbom",
    "attest",
    "snapcraft",
    "nfpm",
    "makeself",
    "install-script",
    "appimage",
    "flatpak",
    "srpm",
    "before",
    "before-publish",
    "notarize",
    "archive",
    "source",
    "build",
    "changelog",
    "release",
    "checksum",
    "upx",
    "templatefiles",
    "dmg",
    "msi",
    "nsis",
    "pkg",
    "appbundle",
    "verify-release",
];

/// Valid `--skip` values for the `release` command: every pipeline
/// stage/phase token ([`NON_PUBLISHER_RELEASE_SKIPS`]) PLUS every publisher
/// token (derived from [`PublisherKind`]).
///
/// Skip tokens are stage names plus publisher names. Every publisher's skip
/// token is its canonical [`crate::Publisher::name`] / [`PublisherKind::token`]
/// (the same token `--publishers` keys on and the same one GoReleaser's
/// `--skip` uses), so homebrew is `homebrew` and chocolatey is `chocolatey` —
/// there are no short aliases (`brew`/`choco`). This keeps one denylist
/// vocabulary across the `--skip` and `--publishers` selectors and matches
/// GoReleaser's `--skip` keys, so a single name works on both tools.
///
/// Deriving the publisher half from [`PublisherKind::iter`] is what makes the
/// vocabulary drift-proof: a newly added publisher is automatically a valid
/// `--skip` token. (This closed a real gap — nine publisher tokens
/// — `npm`, `gemfury`, `cloudsmith`, `artifactory`, `uploads`, `dockerhub`,
/// `mcp`, `schemastore`, `upstream-aur` — had silently fallen out of the old
/// hand-maintained literal.)
pub static VALID_RELEASE_SKIPS: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    NON_PUBLISHER_RELEASE_SKIPS
        .iter()
        .copied()
        .chain(PublisherKind::iter().map(PublisherKind::token))
        .collect()
});

/// One entry in anodizer's canonical `--skip` / `--publishers` vocabulary,
/// emitted by `anodizer vocabulary` for machine consumers (the GitHub Action
/// derives its skip / publisher token sets from this instead of re-deriving
/// them in shell).
///
/// `is_publisher` marks the publisher tokens (the half of the vocabulary that
/// `--publishers` also accepts); `is_publish_stage` mirrors
/// [`PublisherKind::is_publish_stage`] for those, and is always `false` for
/// the non-publisher pipeline-stage tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ReleaseToken {
    /// The canonical lowercase token, exactly as `--skip` / `--publishers`
    /// key on it (e.g. `homebrew`, never `homebrew-cask`; `uploads`, never
    /// `upload`).
    pub token: &'static str,
    /// `true` for the publisher half of the vocabulary — the tokens
    /// `--publishers` also accepts. `false` for non-publisher stage tokens.
    pub is_publisher: bool,
    /// `true` when this is a publisher that fires its publish from a pipeline
    /// stage rather than the trait-dispatch chokepoint (see
    /// [`PublisherKind::is_publish_stage`]). Always `false` for non-publisher
    /// stage tokens.
    pub is_publish_stage: bool,
}

/// The full canonical `--skip` / `--publishers` vocabulary as structured
/// entries, derived entirely from [`NON_PUBLISHER_RELEASE_SKIPS`] and
/// [`PublisherKind::iter`] — no hand-maintained list. Adding a publisher
/// variant or a non-publisher stage token updates this automatically.
///
/// The set of [`ReleaseToken::token`] values equals [`VALID_RELEASE_SKIPS`]
/// exactly (enforced by a by-construction test), so anodizer and its
/// consumers can never disagree on the legal token set.
pub fn release_skip_vocabulary() -> Vec<ReleaseToken> {
    NON_PUBLISHER_RELEASE_SKIPS
        .iter()
        .map(|&token| ReleaseToken {
            token,
            is_publisher: false,
            is_publish_stage: false,
        })
        .chain(PublisherKind::iter().map(|k| ReleaseToken {
            token: k.token(),
            is_publisher: true,
            is_publish_stage: k.is_publish_stage(),
        }))
        .collect()
}

/// Valid --skip values for the `build` command.
pub const VALID_BUILD_SKIPS: &[&str] = &["pre-hooks", "post-hooks", "validate", "before"];

/// Validate that all skip values are in the allowed set.
///
/// Returns `Ok(())` if all values are valid, or `Err` with a descriptive
/// message listing the invalid value(s) and the full set of valid options.
pub fn validate_skip_values(skip: &[String], valid: &[&str]) -> Result<(), String> {
    let invalid: Vec<&str> = dedup_preserving_order(
        skip.iter()
            .map(|s| s.as_str())
            .filter(|s| !valid.contains(s)),
    );
    if invalid.is_empty() {
        Ok(())
    } else {
        // The combined skip vocabulary is `VALID_RELEASE_SKIPS ++ publisher
        // names`, which overlap (e.g. `homebrew`, `cargo` appear in both), so a
        // raw join prints each shared token twice. De-dup the hint — a consumer
        // (or the action's skip-token generator) reading "Valid options" should
        // see one clean vocabulary, not a confusing list with repeats.
        Err(format!(
            "invalid --skip value(s): {}. Valid options: {}",
            invalid.join(", "),
            dedup_preserving_order(valid.iter().copied()).join(", "),
        ))
    }
}

/// Collect an iterator of string slices, dropping later duplicates while keeping
/// first-seen order — used so the `--skip` error hint lists each valid token
/// once even though its source set unions overlapping vocabularies.
fn dedup_preserving_order<'a>(items: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    let mut seen = std::collections::HashSet::new();
    items.filter(|s| seen.insert(*s)).collect()
}

impl Context {
    /// Whether `stage_name` (or a publisher name — the skip list is unified) is
    /// in the operator's `--skip` denylist.
    pub fn should_skip(&self, stage_name: &str) -> bool {
        self.options.skip_stages.iter().any(|s| s == stage_name)
    }

    /// Whether the named publisher is excluded from this run by operator
    /// selection. Combines the two selectors the publish dispatch consults
    /// before running any publisher:
    ///
    /// - `--skip` (`skip_stages`, the UNIFIED denylist holding stage names
    ///   AND publisher names) ALWAYS wins: a publisher named there is
    ///   deselected regardless of any allowlist.
    /// - `--publishers` (`publisher_allowlist`): an EMPTY allowlist deselects
    ///   nothing (every publisher runs); a NON-EMPTY allowlist deselects every
    ///   publisher not listed in it.
    ///
    /// Returns `true` when the publisher should be reported
    /// [`crate::publish_report::SkipReason::Deselected`] instead of dispatched.
    pub fn publisher_deselected(&self, name: &str) -> bool {
        self.should_skip(name)
            || (!self.options.publisher_allowlist.is_empty()
                && !self.options.publisher_allowlist.iter().any(|s| s == name))
    }

    /// Whether ANY of the named publishers survives the operator-selection
    /// filter — the positive dual of [`Self::publisher_deselected`] over a
    /// set. One helper for both registers ("is any consumer selected?" and
    /// its negation "are all consumers deselected?") so callers never
    /// hand-roll De Morgan twins that can drift apart.
    pub fn any_publisher_selected(&self, names: &[&str]) -> bool {
        names.iter().any(|n| !self.publisher_deselected(n))
    }

    /// A distinguished, operator-facing summary line for a deselected
    /// publisher, naming WHICH selector excluded it so the operator can fix
    /// their command. `--skip` always wins, so it is tested first: a publisher
    /// named in both selectors reports the denylist cause.
    ///
    /// Shared by the dispatch chokepoint and the out-of-dispatch publish
    /// stages (blob / snapcraft-publish / docker / docker-sign / announce) so the
    /// "skipped X — excluded via --skip" / "… — not in --publishers allowlist"
    /// wording is identical everywhere a publisher is deselected. Call only
    /// when [`Self::publisher_deselected`] is `true`.
    pub fn deselected_reason(&self, name: &str) -> String {
        let reason = if self.should_skip(name) {
            "excluded via --skip"
        } else {
            "not in --publishers allowlist"
        };
        format!("skipped {name} — {reason}")
    }

    /// Check whether "validate" is in the skip list.
    pub fn skip_validate(&self) -> bool {
        self.should_skip("validate")
    }
}
