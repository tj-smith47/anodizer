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
