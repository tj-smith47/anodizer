//! Multi-archive / multi-artifact disambiguation shared by the homebrew and
//! scoop publishers.
//!
//! When the same platform key (`os_arch` for homebrew, `scoop_arch` for scoop)
//! is produced by multiple archive formats (e.g. `.tar.gz` + `.tar.xz` +
//! `.tar.zst`), the tap/manifest renderers cannot accept more than one entry
//! per platform. Two resolution modes:
//!
//! - `ids_was_set = true`: the user already narrowed via an explicit `ids:`
//!   filter; any remaining duplicate is a configuration error.
//! - `ids_was_set = false`: prefer the configured `preferred_formats` list
//!   (highest-priority format first). If exactly one entry matches the
//!   highest-priority bucket the duplicate is resolved; otherwise error.
//!
//! Output is sorted by platform key so identical inputs always produce
//! byte-identical formula/manifest renderings (reproducible builds).
use anodizer_core::log::StageLogger;
use anyhow::Result;
use std::collections::BTreeMap;

/// Per-call configuration for [`disambiguate_by_format`].
///
/// Held separately so the function stays under clippy's
/// `too_many_arguments` ceiling while keeping the closure parameters
/// (which are inherently per-call) on the function signature.
pub(crate) struct DisambiguateConfig<'a> {
    /// Format priority list, highest-priority first (e.g. `["zip", "tar.gz"]`).
    pub preferred_formats: &'a [&'a str],
    /// `true` when the caller already narrowed via an explicit `ids:` filter;
    /// any remaining duplicate is then a hard configuration error.
    pub ids_was_set: bool,
    /// Publisher name used as the error-message prefix (`"homebrew"` / `"scoop"`).
    pub publisher_label: &'a str,
    /// Crate name surfaced in errors so multi-crate workspaces are diagnosable.
    pub crate_name: &'a str,
    /// Logger for warn lines emitted when the fallback drops an archive.
    pub logger: &'a StageLogger,
}

/// Disambiguate a list of entries sharing the same platform key.
///
/// `T` is the per-platform record (e.g. `(target, url, sha256, format)` for
/// homebrew or `(ArchEntry, format)` for scoop). The closures expose the
/// platform key, the archive format, and a human-readable label per entry
/// used in error messages and log lines.
///
/// On ambiguity the returned error names the conflicting entries via
/// `label_fn`. When the fallback drops one or more entries, each is logged
/// at `warn` level so the user can see what was discarded.
///
/// Errors bear the `<publisher_label>:` prefix and include `crate '<name>'`.
pub(crate) fn disambiguate_by_format<T>(
    entries: Vec<T>,
    key_fn: impl Fn(&T) -> String,
    format_fn: impl Fn(&T) -> &str,
    label_fn: impl Fn(&T) -> String,
    cfg: DisambiguateConfig<'_>,
) -> Result<Vec<T>> {
    let DisambiguateConfig {
        preferred_formats,
        ids_was_set,
        publisher_label,
        crate_name,
        logger,
    } = cfg;

    // Group by key — BTreeMap so output order is deterministic across runs.
    let mut by_key: BTreeMap<String, Vec<T>> = BTreeMap::new();
    for entry in entries {
        let key = key_fn(&entry);
        by_key.entry(key).or_default().push(entry);
    }

    let mut result: Vec<T> = Vec::new();
    for (key, mut group) in by_key {
        if group.len() == 1 {
            result.push(group.pop().unwrap());
            continue;
        }
        let labels = group.iter().map(&label_fn).collect::<Vec<_>>().join(", ");
        // Multiple archives for this platform.
        if ids_was_set {
            anyhow::bail!(
                "{publisher_label}: crate '{crate_name}': multiple archives found for {key} \
                 even after applying ids: filter ({labels}); only one archive per platform is \
                 allowed. Narrow `ids:` further."
            );
        }
        // Walk preferred_formats in priority order; first format with exactly
        // one match wins.
        let mut chosen_idx: Option<usize> = None;
        for fmt in preferred_formats {
            let positions: Vec<usize> = group
                .iter()
                .enumerate()
                .filter(|(_, e)| format_fn(e) == *fmt)
                .map(|(i, _)| i)
                .collect();
            match positions.len() {
                0 => continue,
                1 => {
                    chosen_idx = Some(positions[0]);
                    break;
                }
                _ => {
                    // Multiple entries in the highest-priority bucket reached
                    // so far — still ambiguous even after preference.
                    let conflict_labels = positions
                        .iter()
                        .map(|&i| label_fn(&group[i]))
                        .collect::<Vec<_>>()
                        .join(", ");
                    anyhow::bail!(
                        "{publisher_label}: crate '{crate_name}': multiple .{fmt} archives \
                         found for {key} ({conflict_labels}); only one archive per platform \
                         is allowed. Add `ids:` to select one."
                    );
                }
            }
        }
        let Some(idx) = chosen_idx else {
            let actual_formats: Vec<&str> = group.iter().map(&format_fn).collect();
            anyhow::bail!(
                "{publisher_label}: crate '{crate_name}': multiple archives found for {key} \
                 ({labels}) and none matches a preferred format (have: {actual:?}, \
                 prefer: {prefer:?}); only one archive per platform is allowed. Add `ids:` \
                 to select one.",
                actual = actual_formats,
                prefer = preferred_formats,
            );
        };
        let chosen = group.remove(idx);
        // Log the dropped entries so the user knows what we discarded.
        for dropped in &group {
            logger.warn(&format!(
                "{publisher_label}: crate '{crate_name}': platform {key} had multiple \
                 archives; kept '{kept}' (.{kept_fmt}), dropped '{drop}' (.{drop_fmt}). \
                 Set `ids:` in the {publisher_label} config to pick explicitly.",
                kept = label_fn(&chosen),
                kept_fmt = format_fn(&chosen),
                drop = label_fn(dropped),
                drop_fmt = format_fn(dropped),
            ));
        }
        result.push(chosen);
    }

    Ok(result)
}
