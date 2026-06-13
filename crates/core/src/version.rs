//! Release-version classification.
//!
//! A "release version" is one safe to ship to an external, often
//! irreversible, channel (crates.io, Cloudsmith, Chocolatey, winget, AUR,
//! object-store blobs, announcement broadcasts, …). A snapshot / dirty /
//! `0.0.0`-sentinel version is NOT: shipping it is essentially always a
//! mistake, and several index publishers are one-way doors. This module is
//! the single source of truth for that predicate AND the shared guard so the
//! publish, blob, and announce stages cannot drift on what counts as
//! "non-release" or on how they refuse it.

/// Returns `true` when `version` is safe to publish to an external index —
/// i.e. it is NOT a snapshot / dirty / `0.0.0`-sentinel marker.
///
/// A version is classified **non-release** (returns `false`) when, after
/// trimming, ANY of the following hold:
///
/// - it is empty (no version resolved at all), OR
/// - it matches the `0.0.0` missing-version sentinel — `0.0.0` optionally
///   followed by a `-`, `+`, or `~` pre-release / build / packaging suffix
///   (`0.0.0`, `0.0.0-SNAPSHOT-abc`, `0.0.0~SNAPSHOT_abc`, `0.0.0+dirty`), OR
/// - it carries a snapshot / dirty marker anywhere in the string —
///   `SNAPSHOT` (case-insensitive) or a `dirty` git-state marker.
///
/// Conventional semver pre-releases (`-rc`, `-beta`, `-alpha`, `-dev`) are
/// genuine releases: a `-dev` pre-release is the same release class as `-rc`
/// and is published deliberately, so it is NOT blocked. The real accident this
/// guards (`0.0.0~SNAPSHOT-<sha>`) is already caught by both the `0.0.0`
/// sentinel and the `SNAPSHOT` marker.
///
/// The check is intentionally substring/prefix based rather than strict
/// semver parsing: the synthesized snapshot version
/// (`<base>-SNAPSHOT-<sha>`) and the AUR `~`-normalized form
/// (`0.0.0~SNAPSHOT_<sha>`) are both *valid-enough* strings that a naive
/// `parse_semver` would accept, yet neither must ever reach a real index.
pub fn is_release_version(version: &str) -> bool {
    non_release_reason(version).is_none()
}

/// The human-readable reason `version` is non-release, or `None` when it is a
/// genuine release version. Drives the publish guard's error message so the
/// operator sees *why* the version was rejected, not just that it was.
pub fn non_release_reason(version: &str) -> Option<&'static str> {
    let v = version.trim();
    if v.is_empty() {
        return Some("no version resolved (empty)");
    }
    if is_zero_sentinel(v) {
        return Some("0.0.0 missing-version sentinel");
    }
    let lower = v.to_ascii_lowercase();
    if lower.contains("snapshot") {
        return Some("snapshot marker");
    }
    if lower.contains("dirty") {
        return Some("git-dirty marker");
    }
    None
}

/// `0.0.0` exactly, or `0.0.0` followed by a `-` / `+` / `~` suffix.
fn is_zero_sentinel(v: &str) -> bool {
    let Some(rest) = v.strip_prefix("0.0.0") else {
        return false;
    };
    rest.is_empty() || matches!(rest.as_bytes()[0], b'-' | b'+' | b'~')
}

/// Refuse to release a non-release version from an external-effect stage.
///
/// Shared by the publish, blob, and announce stages: each calls this at its
/// entrypoint — BEFORE any upload / submission / broadcast — so a snapshot /
/// dirty / `0.0.0`-sentinel version can never reach an external channel,
/// regardless of which stage runs (a `--skip=publish` run still guards its
/// blob upload and its announce broadcast).
///
/// `stage` is the user-facing stage label (`"publish"` / `"blob"` /
/// `"announce"`); `targets` are the destination names the stage was about to
/// hit (publisher names, blob provider URLs, announcer names) and are named in
/// the error so the operator sees exactly what was about to leak. Both the
/// global resolved `Version` and every in-scope crate's per-crate resolved
/// version are evaluated, so the guard is correct in all config modes —
/// single-crate, workspace-lockstep, and per-crate.
///
/// No-op in dry-run and snapshot: neither produces an external effect (dry-run
/// stages no-op their side effects; snapshot already short-circuits every one
/// of these stages via `skip_in_snapshot`), so a non-release version there is a
/// preview, not a leak. [`crate::context::ContextOptions::allow_snapshot_publish`]
/// (the `--allow-snapshot-publish` flag) downgrades the bail to a single
/// warning for the deliberate "ship a snapshot to a private channel" case.
pub fn guard_release_version(
    ctx: &crate::context::Context,
    log: &crate::log::StageLogger,
    stage: &str,
    targets: &[String],
) -> anyhow::Result<()> {
    // Real-release only: dry-run and snapshot produce no external effect.
    if ctx.is_dry_run() || ctx.is_snapshot() {
        return Ok(());
    }

    let Some((version, reason)) = first_non_release_version(ctx) else {
        return Ok(());
    };

    let dests = if targets.is_empty() {
        "(none configured)".to_string()
    } else {
        targets.join(", ")
    };

    if ctx.options.allow_snapshot_publish {
        log.warn(&format!(
            "{stage}: releasing non-release version '{version}' ({reason}) to: {dests} \
             — proceeding because --allow-snapshot-publish was set. This version is \
             NOT a real release; only do this for a private/test channel.",
        ));
        return Ok(());
    }

    anyhow::bail!(
        "{stage}: refusing to release non-release version '{version}' ({reason}) to: \
         {dests}. These destinations include one-way-door / external channels; \
         shipping a snapshot / 0.0.0 version is almost always a mistake (e.g. a \
         missing base Version rendered as '0.0.0~SNAPSHOT-<sha>'). Cut a real release \
         with a semver tag, or pass --allow-snapshot-publish to override (intended \
         only for a private/test channel).",
    );
}

/// The first non-release version across the global resolved version and every
/// in-scope crate's per-crate resolved version, with the reason it is
/// non-release. `None` when every resolved version is a genuine release.
///
/// The global `Version` is checked first because it is what the snapshot
/// template stamps (`<base>-SNAPSHOT-<sha>`) and what the `0.0.0` sentinel
/// surfaces as — the exact accident class. Per-crate versions are then checked
/// so per-crate config mode (each crate rendering its own tag-derived version)
/// is covered, not just a single global.
fn first_non_release_version(ctx: &crate::context::Context) -> Option<(String, &'static str)> {
    let global = ctx.version();
    if let Some(reason) = non_release_reason(&global) {
        return Some((global, reason));
    }

    // Per-crate: a crate may resolve its own version from its own tag in
    // per-crate config mode. A crate with no resolvable tag yields `None` here
    // (it would fail loud later at `with_crate_scope`); only an actually
    // resolved, non-release per-crate version trips the guard.
    let selected = &ctx.options.selected_crates;
    for crate_cfg in crate::env_preflight::crate_universe(&ctx.config) {
        if !selected.is_empty() && !selected.contains(&crate_cfg.name) {
            continue;
        }
        if let Some(version) = crate::crate_scope::resolve_crate_tag(ctx, crate_cfg)
            && let Some(reason) = non_release_reason(&version)
        {
            return Some((version, reason));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_semver_is_a_release_version() {
        for v in ["1.0.0", "0.2.1", "10.20.30", "1.2.3-rc.1", "2.0.0+build.5"] {
            assert!(is_release_version(v), "{v} should be a release version");
            assert_eq!(non_release_reason(v), None, "{v}");
        }
    }

    #[test]
    fn empty_version_is_non_release() {
        assert!(!is_release_version(""));
        assert!(!is_release_version("   "));
        assert_eq!(non_release_reason(""), Some("no version resolved (empty)"));
    }

    #[test]
    fn zero_sentinel_is_non_release() {
        for v in [
            "0.0.0",
            "0.0.0-SNAPSHOT-d7813f0",
            "0.0.0~SNAPSHOT_d7813f0",
            "0.0.0+dirty",
        ] {
            assert!(!is_release_version(v), "{v} must be non-release");
        }
        assert_eq!(
            non_release_reason("0.0.0"),
            Some("0.0.0 missing-version sentinel")
        );
        // A non-zero version sharing the 0.0.0 *digits* prefix-by-accident must
        // NOT false-trip on the sentinel (it is caught by length/format).
        assert!(is_release_version("0.0.01")); // 0.0.01 does not strip to a suffix sep
    }

    #[test]
    fn snapshot_marker_is_non_release() {
        for v in ["1.2.3-SNAPSHOT-abc", "1.2.3-snapshot-abc", "9.9.9-SNAPSHOT"] {
            assert!(!is_release_version(v), "{v} must be non-release");
            assert_eq!(non_release_reason(v), Some("snapshot marker"), "{v}");
        }
    }

    #[test]
    fn dirty_marker_is_non_release() {
        assert!(!is_release_version("1.2.3+dirty"));
        assert!(!is_release_version("1.2.3-20240101-dirty"));
        assert_eq!(non_release_reason("1.2.3+dirty"), Some("git-dirty marker"));
    }

    #[test]
    fn dev_pre_release_is_a_release_version() {
        // `-dev` is a conventional semver pre-release, the same class as
        // `-rc` / `-beta`, and is published deliberately. Blocking it while
        // allowing `-rc` would be arbitrary; the real accident
        // (`0.0.0~SNAPSHOT`) is caught by the 0.0.0 sentinel + SNAPSHOT marker.
        for v in ["1.0.0-dev.1", "1.2.3-dev", "1.2.3.dev5", "1.0.0-alpha.dev"] {
            assert!(is_release_version(v), "{v} must be a release version");
            assert_eq!(non_release_reason(v), None, "{v}");
        }
    }

    fn ctx_with_version(version: &str) -> crate::context::Context {
        let mut ctx = crate::context::Context::test_fixture();
        ctx.template_vars_mut().set("Version", version);
        ctx
    }

    #[test]
    fn guard_bails_naming_stage_version_and_targets() {
        let ctx = ctx_with_version("0.0.0~SNAPSHOT-d7813f0");
        let log = ctx.logger("blob-test");
        let err = guard_release_version(&ctx, &log, "blob", &["s3://bucket/key".to_string()])
            .expect_err("non-release version must bail before any external effect");
        let msg = err.to_string();
        assert!(msg.contains("blob"), "names the stage: {msg}");
        assert!(
            msg.contains("0.0.0~SNAPSHOT-d7813f0"),
            "names the version: {msg}"
        );
        assert!(msg.contains("s3://bucket/key"), "names the target: {msg}");
        assert!(
            msg.contains("--allow-snapshot-publish"),
            "tells the operator how to override: {msg}",
        );
    }

    #[test]
    fn guard_allow_snapshot_publish_downgrades_to_warning() {
        let mut ctx = ctx_with_version("0.0.0~SNAPSHOT-d7813f0");
        ctx.options.allow_snapshot_publish = true;
        let log = ctx.logger("announce-test");
        guard_release_version(&ctx, &log, "announce", &["slack".to_string()])
            .expect("--allow-snapshot-publish must downgrade the bail to a warning");
    }

    #[test]
    fn guard_real_semver_passes_silently() {
        let ctx = ctx_with_version("1.4.2");
        let log = ctx.logger("publish-test");
        guard_release_version(&ctx, &log, "publish", &["cargo".to_string()])
            .expect("a real semver version must not trip the guard");
    }

    #[test]
    fn guard_steps_aside_in_dry_run_and_snapshot() {
        for set_mode in [
            (|c: &mut crate::context::Context| c.options.dry_run = true) as fn(&mut _),
            (|c: &mut crate::context::Context| c.options.snapshot = true) as fn(&mut _),
        ] {
            let mut ctx = ctx_with_version("0.0.0~SNAPSHOT-d7813f0");
            set_mode(&mut ctx);
            let log = ctx.logger("blob-test");
            guard_release_version(&ctx, &log, "blob", &["s3://bucket/key".to_string()])
                .expect("dry-run / snapshot must not trip the non-release guard");
        }
    }

    /// Pins the corrected predicate so the deliberate `-dev` decision and the
    /// kept anodizer-specific markers cannot silently regress.
    #[test]
    fn predicate_marker_list_is_pinned() {
        // PASS — genuine releases, including every conventional pre-release.
        for v in [
            "1.0.0-dev.1",
            "1.0.0-rc.1",
            "2.0.0-beta",
            "1.0.0-alpha.dev",
            "1.2.3+build.5",
        ] {
            assert!(is_release_version(v), "{v} must be a release version");
        }
        // FAIL — anodizer's non-release signals: empty, 0.0.0 sentinel,
        // SNAPSHOT marker, dirty.
        for v in [
            "",
            "0.0.0",
            "0.0.0-rc.1",
            "0.9.0-SNAPSHOT-abc123",
            "1.2.3+dirty",
        ] {
            assert!(!is_release_version(v), "{v} must be non-release");
        }
    }
}
