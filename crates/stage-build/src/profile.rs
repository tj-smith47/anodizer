use std::collections::HashMap;

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

// ---------------------------------------------------------------------------
// amd64 microarchitecture variant detection from RUSTFLAGS
// ---------------------------------------------------------------------------

pub(crate) fn parse_amd64_variant_from_rustflags(rustflags: &str) -> Option<String> {
    let tokens: Vec<&str> = rustflags.split_whitespace().collect();
    let mut i = 0;
    while i < tokens.len() {
        let cpu = if let Some(val) = tokens[i].strip_prefix("-Ctarget-cpu=") {
            Some(val)
        } else if tokens[i] == "-C"
            && i + 1 < tokens.len()
            && let Some(val) = tokens[i + 1].strip_prefix("target-cpu=")
        {
            i += 1;
            Some(val)
        } else {
            None
        };
        if let Some(cpu) = cpu
            && let Some(level) = cpu.strip_prefix("x86-64-")
        {
            return Some(level.to_string());
        }
        i += 1;
    }
    None
}

pub(crate) fn detect_amd64_variant(target: &str, env: &HashMap<String, String>) -> Option<String> {
    if !target.starts_with("x86_64") {
        return None;
    }
    if let Some(flags) = env.get("RUSTFLAGS")
        && let Some(v) = parse_amd64_variant_from_rustflags(flags)
    {
        return Some(v);
    }
    None
}
