use std::time::Duration;

/// Wall-clock bound on a single `snapcraft upload` or `snapcraft release`
/// attempt. A Snap Store round-trip that stalls (unreachable store, hung TLS
/// handshake, a snapcraft prompt blocking on stdin) would otherwise hang the
/// entire release or promote forever. Sized generously for a multi-MB snap on
/// a slow link; on expiry the whole snapcraft process subtree is killed.
pub const SNAPCRAFT_UPLOAD_TIMEOUT: Duration = Duration::from_secs(600);

/// Wall-clock bound on a `snapcraft list-revisions` / `snapcraft whoami` probe.
/// A probe only reads a small table, so a much shorter bound suffices.
pub const SNAPCRAFT_PROBE_TIMEOUT: Duration = Duration::from_secs(120);

// ---------------------------------------------------------------------------
// snapcraft_command
// ---------------------------------------------------------------------------

/// Construct the snapcraft pack CLI command arguments.
///
/// `snapcraft pack <prime_dir> --output <snap_file>`.
/// The prime directory is a pre-staged directory containing binaries, extra files,
/// and `meta/snap.yaml`. No `--destructive-mode` is needed because there is no
/// build step — the directory is already assembled.
pub fn snapcraft_command(prime_dir: &str, output_path: &str) -> Vec<String> {
    vec![
        "snapcraft".to_string(),
        "pack".to_string(),
        prime_dir.to_string(),
        "--output".to_string(),
        output_path.to_string(),
    ]
}

/// Construct the `snapcraft list-revisions <name>` command used to probe
/// whether a given version already has a revision in the Snap Store.
///
/// The Snap Store mints a brand-new revision on every `snapcraft upload`, even
/// for an identical `.snap` at the same version — uploads are NOT idempotent.
/// Listing the snap's revisions before uploading lets the publisher detect a
/// re-run at an already-published version and skip the duplicate upload.
pub fn snapcraft_list_revisions_command(snap_name: &str) -> Vec<String> {
    vec![
        "snapcraft".to_string(),
        "list-revisions".to_string(),
        snap_name.to_string(),
    ]
}

/// Construct the snapcraft upload CLI command arguments.
/// When `channels` is non-empty, adds `--release=<comma-separated channels>`.
pub fn snapcraft_upload_command(snap_path: &str, channels: Option<&[String]>) -> Vec<String> {
    let mut args = vec![
        "snapcraft".to_string(),
        "upload".to_string(),
        snap_path.to_string(),
    ];

    if let Some(ch) = channels {
        let non_empty: Vec<&String> = ch.iter().filter(|c| !c.is_empty()).collect();
        if !non_empty.is_empty() {
            let joined: Vec<&str> = non_empty.iter().map(|s| s.as_str()).collect();
            args.push(format!("--release={}", joined.join(",")));
        }
    }

    args
}

// ---------------------------------------------------------------------------
// Promotion — `snapcraft release <name> <revision> <channel>`
// ---------------------------------------------------------------------------

/// Construct the `snapcraft release <name> <revision> <channel>` command used to
/// promote an already-uploaded revision into a channel without rebuilding.
///
/// `snapcraft release` is non-interactive by construction — it takes the target
/// channel as a positional argument and needs no prompt — so no extra
/// `--non-interactive`-style flag is required (unlike `snapcraft register`).
pub fn snapcraft_release_command(snap_name: &str, revision: &str, channel: &str) -> Vec<String> {
    vec![
        "snapcraft".to_string(),
        "release".to_string(),
        snap_name.to_string(),
        revision.to_string(),
        channel.to_string(),
    ]
}

/// Locate the 0-based index of a named column in a `snapcraft list-revisions`
/// header row. Column matching is case-insensitive. Returns `None` when the
/// header is absent or the column is missing.
fn revision_table_column(output: &str, column: &str) -> Option<(Vec<String>, usize)> {
    let mut lines = output.lines();
    let header = loop {
        let line = lines.next()?;
        if line
            .split_whitespace()
            .any(|c| c.eq_ignore_ascii_case("Rev"))
        {
            break line;
        }
    };
    let idx = header
        .split_whitespace()
        .position(|h| h.eq_ignore_ascii_case(column))?;
    let rows: Vec<String> = lines.map(|l| l.to_string()).collect();
    Some((rows, idx))
}

/// Parse `snapcraft list-revisions` output and return the revision number whose
/// `Version` column equals `version`. When several revisions share the version
/// (each re-upload mints a fresh revision), the numerically highest revision
/// wins so a re-promotion targets the latest upload of that version. Returns
/// `None` when no row matches or the table is unparseable — the caller then
/// reports "nothing to promote" rather than releasing a wrong revision.
///
/// `arch` narrows the search to rows whose `Arches` column matches (each row
/// is minted by one architecture-specific `snapcraft upload`, so a dual-arch
/// snap has one row per arch per version) — pass `Some(arch)` when the
/// caller is scoped to a single build artifact. `None` searches every arch,
/// which is the correct behavior for the store-wide `promote` verb: it has
/// no artifact context to scope to and operates on whichever revision is
/// numerically highest across every architecture.
pub fn snap_revision_for_version(
    output: &str,
    version: &str,
    arch: Option<&str>,
) -> Option<String> {
    let (rows, version_col) = revision_table_column(output, "Version")?;
    let (_, rev_col) = revision_table_column(output, "Rev")?;
    let arch_col = match arch {
        Some(_) => Some(revision_table_column(output, "Arches")?.1),
        None => None,
    };
    rows.iter()
        .filter_map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if !cols.get(version_col).is_some_and(|v| *v == version) {
                return None;
            }
            if let (Some(a), Some(col)) = (arch, arch_col)
                && !row_matches_arch(&cols, col, a)
            {
                return None;
            }
            cols.get(rev_col).and_then(|r| r.parse::<u64>().ok())
        })
        .max()
        .map(|r| r.to_string())
}

/// Parse `snapcraft list-revisions` output and return the numerically highest
/// revision of `version` **for each distinct architecture** present in the
/// table. A dual-arch snap mints one revision per arch per version, so a
/// store-wide `promote --version` must release every arch's revision rather
/// than a single global maximum (which would leave the lower-numbered arch on
/// the source channel). Revisions are returned ascending and de-duplicated.
pub fn snap_revisions_for_version_by_arch(output: &str, version: &str) -> Vec<String> {
    revisions_by_arch(output, |cols, version_col, _chan_col| {
        cols.get(version_col).is_some_and(|v| *v == version)
    })
}

/// Parse `snapcraft list-revisions` output and return the numerically highest
/// revision currently released into `channel` **for each distinct
/// architecture**. The store-wide `promote --newest`/default path releases
/// every arch sitting on the source channel, not just the single highest
/// revision across all arches. Revisions are returned ascending and
/// de-duplicated.
pub fn snap_newest_revisions_in_channel_by_arch(output: &str, channel: &str) -> Vec<String> {
    revisions_by_arch(output, |cols, _version_col, chan_col| {
        row_occupies_channel(cols, chan_col, channel)
    })
}

/// Group `list-revisions` rows by their `Arches` column, keep the numerically
/// highest revision per arch whose row satisfies `keep`, and return those
/// revisions ascending + de-duplicated. Rows with an unparseable revision or a
/// missing arch cell are ignored. The `keep` predicate receives the row's
/// columns plus the resolved `Version`/`Channels` column indices so a caller
/// can match on either without re-locating the header.
fn revisions_by_arch(output: &str, keep: impl Fn(&[&str], usize, usize) -> bool) -> Vec<String> {
    let Some((rows, arch_col)) = revision_table_column(output, "Arches") else {
        return Vec::new();
    };
    let Some((_, rev_col)) = revision_table_column(output, "Rev") else {
        return Vec::new();
    };
    // Version/Channels may be absent depending on the caller; default the
    // index to `usize::MAX` so a `keep` closure that does not consult it never
    // indexes a real cell.
    let version_col = revision_table_column(output, "Version")
        .map(|(_, c)| c)
        .unwrap_or(usize::MAX);
    let chan_col = revision_table_column(output, "Channels")
        .map(|(_, c)| c)
        .unwrap_or(usize::MAX);
    let mut best_per_arch: std::collections::BTreeMap<String, u64> =
        std::collections::BTreeMap::new();
    for line in &rows {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if !keep(&cols, version_col, chan_col) {
            continue;
        }
        let Some(arch) = cols.get(arch_col) else {
            continue;
        };
        let Some(rev) = cols.get(rev_col).and_then(|r| r.parse::<u64>().ok()) else {
            continue;
        };
        let entry = best_per_arch.entry(arch.to_string()).or_insert(0);
        if rev > *entry {
            *entry = rev;
        }
    }
    let mut revisions: Vec<u64> = best_per_arch.into_values().collect();
    revisions.sort_unstable();
    revisions.dedup();
    revisions.into_iter().map(|r| r.to_string()).collect()
}

/// Return `true` when a failed `snapcraft list-revisions` (or `release`)
/// invocation's combined output says the snap is simply absent from the store
/// — not yet registered or holding no revisions — as opposed to a genuine
/// authentication / network / server fault. Promotion treats the absent case
/// as "nothing to promote" (a skip) while surfacing every real fault honestly.
///
/// Matching is case-insensitive. Only the store's unambiguous
/// snap-does-not-exist / no-revisions wording qualifies; an auth or
/// connectivity error carries none of these markers and therefore stays a
/// hard error.
pub fn is_snap_absent_from_store(combined_output: &str) -> bool {
    const MARKERS: &[&str] = &[
        "not found in the snap store",
        "could not find snap",
        "has no revisions",
        "no revisions available",
        "no revisions for",
        "is not registered",
        "not registered in the store",
    ];
    let lower = combined_output.to_ascii_lowercase();
    MARKERS.iter().any(|m| lower.contains(m))
}

/// Construct the `snapcraft whoami` command used to probe whether a Snap Store
/// session is available before dispatching a promotion. Non-interactive and
/// side-effect-free — it only prints the logged-in account or errors.
pub fn snapcraft_whoami_command() -> Vec<String> {
    vec!["snapcraft".to_string(), "whoami".to_string()]
}

/// Parse `snapcraft list-revisions` output and return the numerically highest
/// revision currently released into `channel`. The `Channels` column lists the
/// channels a revision occupies (comma- or space-separated, sometimes with a
/// `track/risk` form like `latest/candidate`); a revision counts as "in
/// `channel`" when any listed channel's risk component equals `channel`.
/// Returns `None` when no revision occupies the channel or the table is
/// unparseable.
pub fn snap_newest_revision_in_channel(output: &str, channel: &str) -> Option<String> {
    let (rows, chan_col) = revision_table_column(output, "Channels")?;
    let (_, rev_col) = revision_table_column(output, "Rev")?;
    rows.iter()
        .filter_map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if !row_occupies_channel(&cols, chan_col, channel) {
                return None;
            }
            cols.get(rev_col).and_then(|r| r.parse::<u64>().ok())
        })
        .max()
        .map(|r| r.to_string())
}

/// Snap Store risk levels, in ascending order of stability. A channel is
/// `[track/]risk[/branch]`; the risk component is identified by matching
/// against this fixed vocabulary rather than by position, since a branch
/// suffix (`latest/stable/hotfix-1`) puts the risk in the middle, not last.
const KNOWN_RISKS: &[&str] = &["stable", "candidate", "beta", "edge"];

/// Resolve a `[track/]risk[/branch]` channel string to its risk component by
/// finding the first `/`-separated segment that matches [`KNOWN_RISKS`].
/// Falls back to the whole input when no known risk word is present, so an
/// already-bare, non-standard value is never silently dropped.
fn channel_risk(channel: &str) -> &str {
    channel
        .split('/')
        .find(|segment| KNOWN_RISKS.iter().any(|r| r.eq_ignore_ascii_case(segment)))
        .unwrap_or(channel)
}

/// Return `true` when a `list-revisions` data row's `Arches` column matches
/// `arch` (case-insensitive, exact). Each row is minted by one
/// architecture-specific `snapcraft upload`, so the column holds exactly one
/// token per row — unlike `Channels`, no comma/slash splitting is needed.
fn row_matches_arch(cols: &[&str], arch_col: usize, arch: &str) -> bool {
    cols.get(arch_col)
        .is_some_and(|a| a.eq_ignore_ascii_case(arch))
}

/// Return `true` when a `list-revisions` data row's `Channels` column (and
/// everything after it — a channel list may hold multiple whitespace,
/// comma, or slash-separated tokens) shows the row's revision released to
/// `channel`.
///
/// A progressive/follower channel carries a trailing marker (`candidate*`),
/// stripped before comparing the risk token. `channel` may itself be a bare
/// risk (`stable`) or a full `[track/]risk[/branch]` string (`latest/stable`,
/// `latest/stable/hotfix-1`) — [`channel_risk`] extracts its risk component
/// before comparing against each row token's own risk component, so a
/// track-qualified argument (e.g. `latest/beta`) still matches a row whose
/// `Channels` cell prints the same track-qualified form.
fn row_occupies_channel(cols: &[&str], chan_col: usize, channel: &str) -> bool {
    let target_risk = channel_risk(channel);
    cols.iter().skip(chan_col).any(|tok| {
        tok.split([',', '/']).any(|risk| {
            risk.trim_end_matches(|c: char| !c.is_ascii_alphanumeric())
                .eq_ignore_ascii_case(target_risk)
        })
    })
}

/// For the numerically highest revision whose `Version` column equals
/// `version` AND whose `Arches` column equals `arch`, report which of
/// `channels` it is NOT currently released to.
///
/// `arch` scopes the match to one architecture-specific upload: a dual-arch
/// snap mints one revision per arch per version, so matching on `version`
/// alone would find an unrelated arch's revision and wrongly report the
/// caller's own arch as already published.
///
/// Returns `None` when no revision matches both `version` and `arch` —
/// nothing has been uploaded yet for this artifact, a genuine first
/// publish. Returns `Some((revision, missing))` when a matching revision
/// exists: `missing` is empty when the revision already occupies every
/// requested channel (a true re-run at an already-published version), and
/// non-empty when the revision was uploaded but never released to one or
/// more of the requested channels — the signature of an orphaned upload
/// from an interrupted prior run.
pub fn missing_channels_for_version<'a>(
    output: &str,
    version: &str,
    arch: &str,
    channels: &'a [String],
) -> Option<(String, Vec<&'a str>)> {
    let (rows, version_col) = revision_table_column(output, "Version")?;
    let (_, rev_col) = revision_table_column(output, "Rev")?;
    let (_, chan_col) = revision_table_column(output, "Channels")?;
    let (_, arch_col) = revision_table_column(output, "Arches")?;
    let (rev, cols) = rows
        .iter()
        .filter_map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            let matches_version = cols.get(version_col).is_some_and(|v| *v == version);
            if !matches_version || !row_matches_arch(&cols, arch_col, arch) {
                return None;
            }
            let rev: u64 = cols.get(rev_col)?.parse().ok()?;
            Some((rev, cols))
        })
        .max_by_key(|(rev, _)| *rev)?;
    let missing: Vec<&str> = channels
        .iter()
        .map(|s| s.as_str())
        .filter(|channel| !row_occupies_channel(&cols, chan_col, channel))
        .collect();
    Some((rev.to_string(), missing))
}

// ---------------------------------------------------------------------------
// Channel auto-population based on grade
// ---------------------------------------------------------------------------

/// Resolve effective channels for snapcraft upload.
///
/// If `channel_templates` is non-empty, returns it as-is. Otherwise,
/// auto-populates channels based on `grade` and `confinement`:
/// - `grade == "devel"` OR `confinement == "devmode"` -> `["edge", "beta"]`
/// - otherwise (default) -> `["edge", "beta", "candidate", "stable"]`
///
/// The Snap Store rejects `devmode`-confined and `devel`-grade snaps in
/// `candidate`/`stable` (both are explicitly "not ready for general use"
/// markers), so either one alone is enough to restrict the auto-populated
/// default to the pre-release risk levels.
pub fn resolve_effective_channels(
    channel_templates: Option<&[String]>,
    grade: Option<&str>,
    confinement: Option<&str>,
) -> Option<Vec<String>> {
    if channel_templates.is_some_and(|v| !v.is_empty()) {
        return channel_templates.map(|v| v.to_vec());
    }
    let grade = grade.unwrap_or("stable");
    let restricted = grade == "devel" || confinement == Some("devmode");
    Some(if restricted {
        vec!["edge".to_string(), "beta".to_string()]
    } else {
        vec![
            "edge".to_string(),
            "beta".to_string(),
            "candidate".to_string(),
            "stable".to_string(),
        ]
    })
}

/// Channels the Snap Store rejects for a `devmode`-confined or
/// `devel`-grade snap (both mean "not ready for general use").
const RESTRICTED_ONLY_CHANNELS: &[&str] = &["candidate", "stable"];

/// Return the first configured channel (its risk component, identified via
/// [`channel_risk`] rather than positionally — a branch-suffixed channel
/// like `latest/stable/hotfix-1` carries its risk in the middle, not last)
/// that a `devmode`-confined or `devel`-grade snap is not eligible for, so
/// the caller can bail with a concrete channel name before ever invoking
/// `snapcraft upload`.
pub fn first_channel_rejected_for_prerelease_snap(channels: &[String]) -> Option<&str> {
    channels.iter().find_map(|c| {
        let risk = channel_risk(c);
        RESTRICTED_ONLY_CHANNELS
            .iter()
            .find(|r| r.eq_ignore_ascii_case(risk))
            .copied()
    })
}

// ---------------------------------------------------------------------------
// 5xx retry classifier
// ---------------------------------------------------------------------------

/// Return `true` if the combined stdout/stderr of a failed `snapcraft upload`
/// invocation looks like a transient Snap Store failure that should be
/// retried.
///
/// Detects a retriable snap-push failure by scanning the `snapcraft`
/// CLI's combined output for `[500]`, `[502]`, `[503]`, `[504]` bracketed
/// status markers (the format snapcraft itself prints when the Store
/// returns a server error).
///
/// Also accepts the canonical `5xx <Reason>` text forms (`500 Internal
/// Server Error`, `502 Bad Gateway`, `503 Service Unavailable`, `504
/// Gateway Timeout`) so a future change to snapcraft's error formatter that
/// drops the `[NNN]` brackets does not silently regress retry coverage.
///
/// The `binary_sha3_384: Error checking upload uniqueness.` message is
/// classified here, NOT as a content-dedup rejection. It reports that the
/// Store's uniqueness-check step ITSELF errored server-side — it does not
/// assert a duplicate was found. The Store names a genuine duplicate with a
/// distinct, explicit message (see [`is_content_dedup_rejection`]); when it
/// instead says the check "errored", the failure is a transient backend
/// fault and a later attempt can succeed. (Empirically: a `.snap` carrying a
/// freshly-bumped version cannot be byte-identical to any prior release's
/// revision, so the uniqueness step erroring on it is never a real content
/// collision — it is the check service faulting.)
///
/// Matching is case-insensitive (the combined output is lowercased before
/// scanning) so a differently-cased Store response never silently misses
/// this check.
pub fn is_retriable_snap_push(combined_output: &str) -> bool {
    const BRACKETED: &[&str] = &["[500]", "[502]", "[503]", "[504]"];
    const TEXT: &[&str] = &[
        "500 internal server error",
        "502 bad gateway",
        "503 service unavailable",
        "504 gateway timeout",
        // The Store's uniqueness-check step faulted (backend error), which is
        // transient — distinct from a confirmed content duplicate.
        "error checking upload uniqueness",
    ];
    let lower = combined_output.to_ascii_lowercase();
    BRACKETED.iter().any(|m| lower.contains(m)) || TEXT.iter().any(|m| lower.contains(m))
}

// ---------------------------------------------------------------------------
// Content-dedup rejection classifier
// ---------------------------------------------------------------------------

/// Return `true` if the combined stdout/stderr of a failed `snapcraft upload`
/// invocation shows the Snap Store rejecting the push because the uploaded
/// `.snap`'s content hash (`binary_sha3_384`) already matches a prior
/// revision — a *confirmed* duplicate.
///
/// This rejection is permanent for the given bytes: the Store deduplicates
/// on content, not on the caller-supplied version string, so no number of
/// retries changes the outcome — the fix is to PROMOTE the already-landed
/// revision, or (if none is at the current version) ship byte-different
/// `.snap` contents.
///
/// Only the Store's definitive duplicate message counts here. The ambiguous
/// `Error checking upload uniqueness.` message is deliberately NOT matched:
/// it reports the uniqueness *check* faulting, not a duplicate being found,
/// and is classified as a transient retriable failure instead (see
/// [`is_retriable_snap_push`]). Treating that ambiguous message as a
/// permanent dedup previously fast-failed a purely transient Store fault and
/// emitted a false "the .snap contents must change" verdict against a snap
/// whose bytes could not possibly collide with an older version.
///
/// Matching is case-insensitive (the combined output is lowercased before
/// scanning), same reasoning as [`is_retriable_snap_push`]. Callers must
/// check [`is_retriable_snap_push`] FIRST: a response can carry both a
/// transient marker and a genuine dedup marker (observed in the wild — the
/// Store returns a 503 on the attempt that actually landed the bytes, then
/// rejects the client's automatic retry as a duplicate of what it already
/// ingested) and the transient classification must win so the retry ladder
/// gets a chance to resolve it before this permanent classification
/// short-circuits it.
pub fn is_content_dedup_rejection(combined_output: &str) -> bool {
    const MARKERS: &[&str] = &["a file with this exact same content has already been uploaded"];
    let lower = combined_output.to_ascii_lowercase();
    MARKERS.iter().any(|m| lower.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absence_probe_matches_only_genuine_snap_absence() {
        for msg in [
            "Snap 'mysnap' was not found in the Snap Store.",
            "error: could not find snap 'mysnap' in the store",
            "This snap has no revisions available.",
            "snap 'mysnap' is not registered in the store",
        ] {
            assert!(
                is_snap_absent_from_store(msg),
                "genuine snap-absence wording must classify as absent: {msg:?}"
            );
        }
    }

    #[test]
    fn absence_probe_treats_connectivity_failure_as_a_hard_error() {
        // A DNS/network fault ("could not find host …") must NOT masquerade as
        // an unregistered snap — it carries no snap-does-not-exist wording, so
        // the promote probe surfaces it honestly rather than silently skipping.
        let msg = "snapcraft: error: could not find host api.snapcraft.io";
        assert!(
            !is_snap_absent_from_store(msg),
            "a connectivity failure must stay a hard error, not an empty skip"
        );
    }

    #[test]
    fn absence_probe_treats_authorization_failure_as_a_hard_error() {
        // The snap exists but belongs to another account: an authorization
        // fault the operator must see, never a silent "nothing to promote".
        let msg = "error: you are not the publisher or collaborator of this snap";
        assert!(
            !is_snap_absent_from_store(msg),
            "a wrong-account auth failure must stay a hard error, not an empty skip"
        );
    }
}
