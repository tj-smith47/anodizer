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

/// Parse `snapcraft list-revisions` tabular output and report whether any
/// listed revision carries `version`.
///
/// The command prints a header row (`Rev  Uploaded  Arches  Version  ...`)
/// followed by one row per revision. We locate the `Version` column index
/// from the header row, then compare only that column in each data row so
/// tokens in other columns (Rev, Arches, Channels) cannot cause a false
/// positive for versions that happen to look like revision numbers or arch
/// strings (e.g. version `3` matching revision `3`). An empty / unparseable
/// body or a missing `Version` header yields `false` — the caller treats
/// "couldn't prove the revision exists" as "upload" so a genuine first
/// publish is never skipped.
pub fn snap_revision_exists_in_output(output: &str, version: &str) -> bool {
    let mut lines = output.lines();
    // Advance to the header row (contains "Rev").
    let header = loop {
        let Some(line) = lines.next() else {
            return false;
        };
        if line
            .split_whitespace()
            .any(|c| c.eq_ignore_ascii_case("Rev"))
        {
            break line;
        }
    };
    // Determine the 0-based column index for "Version".
    let Some(version_col) = header
        .split_whitespace()
        .position(|h| h.eq_ignore_ascii_case("Version"))
    else {
        return false;
    };
    // Check data rows: only the Version column must equal the target version.
    lines.any(|line| {
        line.split_whitespace()
            .nth(version_col)
            .map(|v| v == version)
            .unwrap_or(false)
    })
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
pub fn snap_revision_for_version(output: &str, version: &str) -> Option<String> {
    let (rows, version_col) = revision_table_column(output, "Version")?;
    let (_, rev_col) = revision_table_column(output, "Rev")?;
    rows.iter()
        .filter_map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            let matches_version = cols.get(version_col).is_some_and(|v| *v == version);
            if !matches_version {
                return None;
            }
            cols.get(rev_col).and_then(|r| r.parse::<u64>().ok())
        })
        .max()
        .map(|r| r.to_string())
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
            // The Channels column and everything after it may hold multiple
            // channel tokens; scan from the Channels column to end of row.
            let in_channel = cols.iter().skip(chan_col).any(|tok| {
                tok.split([',', '/']).any(|risk| {
                    // A progressive/follower channel carries a trailing marker
                    // (`candidate*`); strip it before comparing the risk token.
                    risk.trim_end_matches(|c: char| !c.is_ascii_alphanumeric())
                        .eq_ignore_ascii_case(channel)
                })
            });
            if !in_channel {
                return None;
            }
            cols.get(rev_col).and_then(|r| r.parse::<u64>().ok())
        })
        .max()
        .map(|r| r.to_string())
}

// ---------------------------------------------------------------------------
// Channel auto-population based on grade
// ---------------------------------------------------------------------------

/// Resolve effective channels for snapcraft upload.
///
/// If `channel_templates` is non-empty, returns it as-is. Otherwise,
/// auto-populates channels based on the `grade` setting:
/// - `"devel"` -> `["edge", "beta"]`
/// - `"stable"` (default) -> `["edge", "beta", "candidate", "stable"]`
///
/// transient push failures are retried.
pub fn resolve_effective_channels(
    channel_templates: Option<&[String]>,
    grade: Option<&str>,
) -> Option<Vec<String>> {
    if channel_templates.is_some_and(|v| !v.is_empty()) {
        return channel_templates.map(|v| v.to_vec());
    }
    let grade = grade.unwrap_or("stable");
    Some(if grade == "devel" {
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

// ---------------------------------------------------------------------------
// 5xx retry classifier — Q8.1
// ---------------------------------------------------------------------------

/// Return `true` if the combined stdout/stderr of a failed `snapcraft upload`
/// invocation looks like a transient Snap Store 5xx response that should be
/// retried.
///
/// Detects a retriable snap-push failure by scanning the `snapcraft`
/// CLI's combined output for `[500]`, `[502]`, `[503]`, `[504]` bracketed
/// status markers (the format snapcraft itself prints when the Store
/// returns a server error).
///
/// We additionally accept the canonical `5xx <Reason>` text forms
/// (`500 Internal Server Error`, `502 Bad Gateway`, `503 Service
/// Unavailable`, `504 Gateway Timeout`) so a future change to snapcraft's
/// error formatter that drops the `[NNN]` brackets does not silently
/// regress retry coverage.
pub fn is_retriable_snap_push(combined_output: &str) -> bool {
    const BRACKETED: &[&str] = &["[500]", "[502]", "[503]", "[504]"];
    const TEXT: &[&str] = &[
        "500 Internal Server Error",
        "502 Bad Gateway",
        "503 Service Unavailable",
        "504 Gateway Timeout",
    ];
    BRACKETED.iter().any(|m| combined_output.contains(m))
        || TEXT.iter().any(|m| combined_output.contains(m))
}
