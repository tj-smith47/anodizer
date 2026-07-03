// ---------------------------------------------------------------------------
// detect_cargo_profile — parse --release / --profile flags from cargo flags
// ---------------------------------------------------------------------------

/// Detect the effective cargo profile from a flags string.
///
/// Handles `--release`, `--profile release`, and `--profile=release` (or any
/// other profile name like `--profile=bench`).  Falls back to `"debug"` when
/// no profile flag is found.
///
/// Returns a `&str` that borrows from the input flags string for custom
/// profile names, or a static string for well-known profiles.
pub(crate) fn detect_cargo_profile(flags: &[String]) -> &str {
    if flags.is_empty() {
        return "debug";
    }

    // Check for --profile=<name> (equals form)
    for token in flags {
        if let Some(name) = token.strip_prefix("--profile=")
            && !name.is_empty()
        {
            return match name {
                "dev" => "debug",
                _ => name,
            };
        }
    }

    // Check for --profile <name> (space-separated form)
    for i in 0..flags.len() {
        if flags[i] == "--profile"
            && let Some(name) = flags.get(i + 1)
        {
            return match name.as_str() {
                "dev" => "debug",
                _ => name.as_str(),
            };
        }
    }

    // Check for --release flag
    if flags.iter().any(|f| f == "--release") {
        return "release";
    }

    "debug"
}
