use super::*;

pub(crate) fn get_messages_for_bump(
    workspace_root: &Path,
    cfg: &ResolvedConfig,
    prev_tag: Option<&str>,
    path: Option<&str>,
) -> Result<Vec<String>> {
    match cfg.branch_history.as_str() {
        "last" => match path {
            Some(p) => git::get_last_commit_messages_path_in(workspace_root, 1, p),
            None => git::get_last_commit_messages_in(workspace_root, 1),
        },
        "full" | "compare" => match (prev_tag, path) {
            (Some(tag), Some(p)) => {
                git::get_commit_messages_between_path_in(workspace_root, tag, "HEAD", p)
            }
            (Some(tag), None) => git::get_commit_messages_between_in(workspace_root, tag, "HEAD"),
            (None, Some(p)) => git::get_last_commit_messages_path_in(workspace_root, 500, p),
            (None, None) => git::get_last_commit_messages_in(workspace_root, 500),
        },
        other => {
            bail!("unknown branch_history mode: {}", other);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BumpKind {
    Major,
    Minor,
    Patch,
    None,
}

pub(crate) fn detect_bump(messages: &[String], cfg: &ResolvedConfig) -> BumpKind {
    detect_bump_from_tokens(
        messages,
        &cfg.major_string_token,
        &cfg.minor_string_token,
        &cfg.patch_string_token,
        &cfg.none_string_token,
        &cfg.default_bump,
    )
}

/// Detect the bump, then apply pre-major demotion for an inferred bump.
///
/// A bump driven by an explicit `#major`/`#minor`/`#patch` token is operator
/// intent and is returned untouched (see [`has_explicit_bump_token`]). A bump
/// derived from the conventional-commit layer or the `default_bump` fallback is
/// subject to [`demote_pre_major`] when the governing major (from `prev_tag`, or
/// `0` when there is no prior tag) is still `0`. The demotion is computed here,
/// once, so the lockstep-workspace and per-crate tagging paths share it.
pub(crate) fn detect_bump_demoted(
    messages: &[String],
    cfg: &ResolvedConfig,
    prev_tag: Option<&str>,
) -> BumpKind {
    let bump = detect_bump(messages, cfg);
    if has_explicit_bump_token(messages, cfg) {
        return bump;
    }
    let base_major = prev_tag
        .and_then(|t| git::parse_semver_tag(t).ok())
        .map_or(0, |sv| sv.major);
    demote_pre_major(
        bump,
        base_major,
        cfg.bump_minor_pre_major,
        cfg.bump_patch_for_minor_pre_major,
    )
}

/// Whole-word (not substring) token match: a token counts only when it appears
/// as a standalone whitespace-separated word, so a `#none` in prose
/// (`"revert the #none commit"`) or a word like `#handsome` does not trigger it.
/// Shared by [`has_explicit_bump_token`] and [`detect_bump_from_tokens`] so the
/// two never drift on what "a token is present" means — their agreement is a
/// correctness invariant (the explicit-token layer and the gate must see the
/// same tokens).
pub(crate) fn message_has_token(msg: &str, token: &str) -> bool {
    msg.split(|c: char| c.is_whitespace()).any(|w| w == token)
}

/// Whether a message carries a standalone `#major`/`#minor`/`#patch` token that
/// *drives* the bump. Those three are matched ahead of the conventional-commit
/// layer in [`detect_bump_from_tokens`], so their presence always determines the
/// result — the bump is explicit operator intent that pre-major demotion must
/// not touch. `#none` is excluded on purpose: it is the lowest-priority token (a
/// conventional marker overrides it), so a `#none` sharing a range with a
/// `feat!:` does NOT drive the bump and must not block that breaking change's
/// demotion; a `#none` that does win already yields `BumpKind::None`, which
/// demotion passes through untouched. Whole-word match mirrors
/// [`detect_bump_from_tokens`] (a token embedded in prose does not count).
pub(crate) fn has_explicit_bump_token(messages: &[String], cfg: &ResolvedConfig) -> bool {
    let has = |token: &str| messages.iter().any(|m| message_has_token(m, token));
    has(&cfg.major_string_token) || has(&cfg.minor_string_token) || has(&cfg.patch_string_token)
}

/// SemVer "major version zero" demotion (release-please's `bump-minor-pre-major`
/// / `bump-patch-for-minor-pre-major`). While `base_major == 0` the public API
/// is unstable, so an inferred breaking change need not force `1.0.0` and an
/// inferred feature need not force a minor.
///
/// - `bump_minor_pre_major`: [`BumpKind::Major`] → [`BumpKind::Minor`]
/// - `bump_patch_for_minor_pre_major`: [`BumpKind::Minor`] → [`BumpKind::Patch`]
///
/// The two axes are independent (a breaking change is governed by the first,
/// a feature by the second — no cascade). Once `base_major >= 1` the project
/// has committed to a stable API, so both toggles are inert.
pub(crate) fn demote_pre_major(
    bump: BumpKind,
    base_major: u64,
    bump_minor_pre_major: bool,
    bump_patch_for_minor_pre_major: bool,
) -> BumpKind {
    if base_major != 0 {
        return bump;
    }
    match bump {
        BumpKind::Major if bump_minor_pre_major => BumpKind::Minor,
        BumpKind::Minor if bump_patch_for_minor_pre_major => BumpKind::Patch,
        other => other,
    }
}

/// Core bump detection logic, separated for unit testing without needing the full config.
///
/// Resolution order (highest precedence first):
/// 1. Explicit bump tokens `#major` > `#minor` > `#patch` — operator intent,
///    always wins. `#none` is deliberately NOT in this layer.
/// 2. Conventional-commit markers when no `#major`/`#minor`/`#patch` matched —
///    a line containing `BREAKING CHANGE` or a `<type>!:` shorthand → major,
///    `feat:` → minor, `fix:` / `perf:` / `revert:` → patch. A message that
///    starts with `chore:` / `docs:` / `style:` / `refactor:` / `test:` /
///    `build:` / `ci:` is NOT release-worthy, so it contributes nothing. A
///    release-worthy marker beats `#none` (an explicit release signal
///    overrides the veto).
/// 3. `#none` — vetoes the `default_bump` fallback, so a range whose only
///    signal is `#none` skips the release.
/// 4. `default_bump` fallback when nothing above matched (default `none`:
///    chore-only ranges no-op; set `patch`/`minor` to release every range).
pub(crate) fn detect_bump_from_tokens(
    messages: &[String],
    major_token: &str,
    minor_token: &str,
    patch_token: &str,
    none_token: &str,
    default_bump: &str,
) -> BumpKind {
    let mut has_major = false;
    let mut has_minor = false;
    let mut has_patch = false;
    let mut has_none = false;

    for msg in messages {
        if message_has_token(msg, none_token) {
            has_none = true;
        }
        if message_has_token(msg, major_token) {
            has_major = true;
        }
        if message_has_token(msg, minor_token) {
            has_minor = true;
        }
        if message_has_token(msg, patch_token) {
            has_patch = true;
        }
    }

    // Priority: major > minor > patch among explicit tokens.
    if has_major {
        return BumpKind::Major;
    }
    if has_minor {
        return BumpKind::Minor;
    }
    if has_patch {
        return BumpKind::Patch;
    }

    // Conventional-commit layer: fires when no explicit #token matched. A
    // release-worthy conventional marker wins over `#none` because `#none`
    // represents "no default bump intended" — it's a veto over the implicit
    // fallback, not a veto over explicit release signals.
    if let Some(bump) = detect_conventional_bump(messages) {
        return bump;
    }

    // No explicit token, no conventional marker. `#none` now takes effect:
    // ranges where the only "signal" is `#none` explicitly skip, regardless
    // of default_bump.
    if has_none {
        return BumpKind::None;
    }

    // Fall back to default_bump
    match default_bump {
        "major" => BumpKind::Major,
        "minor" => BumpKind::Minor,
        "patch" => BumpKind::Patch,
        "none" | "false" => BumpKind::None,
        // An unrecognized default_bump value fails safe to no release rather
        // than a surprise bump (the unset default is "none" — see line above).
        _ => BumpKind::None,
    }
}

/// Scan messages for Conventional-Commits release-worthy markers.
///
/// Returns `Some(kind)` when at least one message matches a bump-worthy
/// pattern; `None` when the range contains only non-release-worthy commit
/// types (chore, docs, style, refactor, test, build, ci) or unstructured
/// messages. The caller decides how to treat `None` — typically fall back
/// to the configured `default_bump`.
///
/// Per-commit classification delegates to the shared
/// [`anodizer_core::git::classify_commit`] rules (the same classifier
/// `anodizer bump` infers from, so a `bump --dry-run` preview and the
/// auto-tag cut can never disagree); the strongest signal in the range wins.
pub(crate) fn detect_conventional_bump(messages: &[String]) -> Option<BumpKind> {
    use anodizer_core::git::ConventionalLevel;
    messages
        .iter()
        .filter_map(|msg| anodizer_core::git::classify_commit(msg))
        .max()
        .map(|level| match level {
            ConventionalLevel::Major => BumpKind::Major,
            ConventionalLevel::Minor => BumpKind::Minor,
            ConventionalLevel::Patch => BumpKind::Patch,
        })
}
/// Apply a bump to semver components. Returns (major, minor, patch).
pub(crate) fn apply_bump(major: u64, minor: u64, patch: u64, bump: &BumpKind) -> (u64, u64, u64) {
    match bump {
        BumpKind::Major => (major + 1, 0, 0),
        BumpKind::Minor => (major, minor + 1, 0),
        BumpKind::Patch => (major, minor, patch + 1),
        BumpKind::None => (major, minor, patch),
    }
}
