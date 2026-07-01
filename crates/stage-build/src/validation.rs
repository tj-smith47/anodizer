// ---------------------------------------------------------------------------
// strip_glibc_suffix — strip glibc version suffix like ".2.17" from targets
// ---------------------------------------------------------------------------

/// Strip a glibc version suffix from a target triple.
///
/// Targets like `aarch64-unknown-linux-gnu.2.17` carry a `.X.Y` suffix that
/// tells cargo-zigbuild which glibc version to link against. Cargo itself
/// doesn't understand the suffix, so we strip it when constructing the target
/// directory path. The full target (with suffix) is passed to cargo-zigbuild.
///
/// Returns `(cargo_target, has_suffix)` — when there is no suffix the input
/// is returned unchanged.
pub(crate) fn strip_glibc_suffix(target: &str) -> (&str, bool) {
    // Match patterns like "gnu.2.17", "musl.1.1"
    // The suffix starts with a dot followed by a digit after "gnu" or "musl"
    if let Some(idx) = target.rfind("gnu.").or_else(|| target.rfind("musl.")) {
        let suffix_start = target[idx..].find('.').map(|i| idx + i);
        if let Some(start) = suffix_start {
            // Verify the part after the dot looks like a version (starts with digit)
            let after_dot = &target[start + 1..];
            if after_dot.starts_with(|c: char| c.is_ascii_digit()) {
                return (&target[..start], true);
            }
        }
    }
    (target, false)
}

/// Check if a target has a glibc version suffix and should be validated
/// against the known targets list without the suffix.
pub(crate) fn target_for_validation(target: &str) -> &str {
    strip_glibc_suffix(target).0
}
