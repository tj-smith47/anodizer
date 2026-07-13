//! Reading crate and workspace `Cargo.toml` version references.

// ---------------------------------------------------------------------------
// publish_to_cargo
// ---------------------------------------------------------------------------

/// Whether a `[<section>]` Cargo.toml block contains a literal
/// `version = "..."` or a `version.workspace = true` reference.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CargoVersionRef {
    /// `version = "X.Y.Z"` — literal version, return as-is.
    Literal(String),
    /// `version.workspace = true` or `version = { workspace = true }` —
    /// walk up to the workspace root and resolve via `[workspace.package]`.
    Workspace,
    /// No version field in the section.
    None,
}

/// Scan a Cargo.toml body for the named section's `version` field.
/// `section_header` is e.g. `"[package]"` or `"[workspace.package]"`.
///
/// Terminates the in-section scan only when the next `[header]` is a
/// SIBLING (not a sub-table of the same logical block). For example,
/// inside `[workspace.package]` the scan continues past
/// `[workspace.package.metadata.X]` because that's a child of the
/// logical block, but stops at `[workspace.dependencies]` because
/// that's a sibling section.
///
/// Lines that begin with `#` are comment-only and skipped. Trailing
/// `# comment` text after `version = "X.Y.Z"` is also stripped before
/// parsing the literal — otherwise the value would include the
/// remainder of the line.
pub(crate) fn scan_section_version(content: &str, section_header: &str) -> CargoVersionRef {
    // The section-prefix is `[section_header[..-1] + '.'` — any header
    // starting with this is a sub-table of the same logical block and
    // does not end the scan.
    let sub_prefix = {
        let trimmed = section_header
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .unwrap_or(section_header);
        format!("[{trimmed}.")
    };
    let mut in_section = false;
    for line in content.lines() {
        let trimmed_full = line.trim();
        // Strip whole-line `#` comments. (Inline `# ...` after a value
        // is handled per-value below to keep the literal-parse honest.)
        if trimmed_full.starts_with('#') {
            continue;
        }
        let trimmed = trimmed_full;
        if trimmed == section_header {
            in_section = true;
            continue;
        }
        if trimmed.starts_with('[') {
            if in_section && !trimmed.starts_with(&sub_prefix) {
                return CargoVersionRef::None;
            }
            // Outside the target section, OR a sub-table of it: skip
            // the header line and keep scanning.
            continue;
        }
        if !in_section {
            continue;
        }
        // `version.workspace = true` — but only when followed by a key
        // boundary char so `versioned-foo` / `versions` / `version-spec`
        // don't get accidentally classified as workspace inherits.
        if let Some(rest) = strip_key_prefix(trimmed, "version.workspace") {
            let rest = rest.trim_start().strip_prefix('=').unwrap_or("").trim();
            if rest.starts_with("true") {
                return CargoVersionRef::Workspace;
            }
        }
        // `version = "X.Y.Z"` (literal) or `version = { workspace = true }`
        // (inline-table form). Same key-boundary check.
        if let Some(rest) = strip_key_prefix(trimmed, "version") {
            let rest = rest.trim_start().strip_prefix('=').unwrap_or("").trim();
            // Literal: take the substring between the first and second `"`
            // so a trailing `# comment` doesn't bleed into the version.
            if let Some(after) = rest.strip_prefix('"')
                && let Some(end) = after.find('"')
            {
                return CargoVersionRef::Literal(after[..end].to_string());
            }
            if rest.starts_with('{')
                && rest
                    .trim_start_matches('{')
                    .trim_end_matches('}')
                    .split(',')
                    .any(|kv| kv.trim().starts_with("workspace") && kv.contains("true"))
            {
                return CargoVersionRef::Workspace;
            }
        }
    }
    CargoVersionRef::None
}

/// `s.strip_prefix(key)` plus a key-boundary check so `version`
/// doesn't match `versioned` / `versions` / `version-spec`. After the
/// prefix the next char must be whitespace, `=`, or `.` (for
/// `version.workspace`). Returns the post-prefix remainder when the
/// boundary holds, else `None`.
pub(crate) fn strip_key_prefix<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(key)?;
    match rest.chars().next() {
        // EOL after the key alone (`version`) is not a valid key=value
        // line; reject so callers don't compute an empty `rest`.
        None => None,
        Some(c) if c.is_whitespace() || c == '=' || c == '.' => Some(rest),
        _ => None,
    }
}

/// Walk parent directories from `start` looking for a Cargo.toml that
/// contains a real `[workspace]` (or exactly `[workspace.package]`)
/// section header. Returns the path to that workspace root manifest.
/// Walks at most 12 levels to bound runtime.
///
/// The header check is anchored to the exact strings — `starts_with`
/// would falsely accept a leaf-crate manifest that contains only a
/// sub-table like `[workspace.package.metadata.docs.rs]` (some crates
/// declare these for workspace-inherited metadata without being a
/// workspace root themselves).
pub(crate) fn find_workspace_root_manifest(start: &std::path::Path) -> Option<std::path::PathBuf> {
    let start_abs = std::fs::canonicalize(start).ok().unwrap_or(start.into());
    let mut dir: &std::path::Path = start_abs.as_ref();
    for _ in 0..12 {
        let candidate = dir.join("Cargo.toml");
        if candidate.is_file()
            && let Ok(content) = std::fs::read_to_string(&candidate)
            && content.lines().any(|l| {
                let t = l.trim();
                t == "[workspace]" || t == "[workspace.package]"
            })
        {
            return Some(candidate);
        }
        dir = match dir.parent() {
            Some(p) => p,
            None => break,
        };
    }
    None
}

/// Read the published version for a crate at `crate_path`.
///
/// Resolves three Cargo.toml shapes:
/// - `version = "X.Y.Z"` in `[package]` → returns `Some("X.Y.Z")`.
/// - `version.workspace = true` (or `version = { workspace = true }`)
///   → walks parent dirs for a Cargo.toml with `[workspace]`, reads
///   `[workspace.package].version`, returns that.
/// - No version anywhere → `None`.
///
/// The workspace-inheritance branch is load-bearing for multi-cadence
/// workspaces (one crate at v0.2.x while siblings are at v0.3.x).
/// Falling back to the release-context version in that case would
/// poll the wrong version on the crates.io index → either a timeout
/// or a false confirmation.
pub(crate) fn read_cargo_toml_version(crate_path: &str) -> Option<String> {
    let manifest = std::path::Path::new(crate_path).join("Cargo.toml");
    let content = std::fs::read_to_string(&manifest).ok()?;
    match scan_section_version(&content, "[package]") {
        CargoVersionRef::Literal(v) => Some(v),
        CargoVersionRef::None => None,
        CargoVersionRef::Workspace => {
            // Walk up from the crate's directory to find the workspace
            // root Cargo.toml. `crate_path` is typically a relative path
            // from the repo root (e.g. `crates/core`), so `.parent()` of
            // its Cargo.toml gives the crate dir; walking up from there
            // finds the workspace manifest.
            let ws_manifest = find_workspace_root_manifest(
                manifest.parent().unwrap_or(std::path::Path::new(".")),
            )?;
            let ws_content = std::fs::read_to_string(&ws_manifest).ok()?;
            match scan_section_version(&ws_content, "[workspace.package]") {
                CargoVersionRef::Literal(v) => Some(v),
                _ => None,
            }
        }
    }
}
