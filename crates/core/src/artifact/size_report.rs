use super::kind::size_reportable_kinds;
use super::registry::ArtifactRegistry;

/// Format a byte count into a human-readable string (e.g. "4.2 MB").
pub fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;

    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{} B", bytes)
    }
}

/// Populate artifact sizes and print a formatted size table.
///
/// Filters artifacts to [`size_reportable_kinds`] (the
/// `reportsizes` pipe), stores the file size in each artifact's `size` field,
/// and prints a human-readable table. Each row is named by the artifact's
/// path relative to `dist`, so seven raw binaries that all sit at
/// `dist/<crate>_<target>/anodizer` print as seven distinct rows instead of
/// seven bare `anodizer` lines; a path outside `dist` prints in full.
pub fn print_size_report(
    registry: &mut ArtifactRegistry,
    dist: &std::path::Path,
    log: &crate::log::StageLogger,
) {
    let reportable = size_reportable_kinds();
    let mut entries: Vec<(String, u64)> = Vec::new();
    let mut total: u64 = 0;

    for artifact in registry.all_mut() {
        if !reportable.contains(&artifact.kind) {
            continue;
        }
        if let Ok(meta) = std::fs::metadata(&artifact.path) {
            let size = meta.len();
            artifact.size = Some(size);
            let name = artifact
                .path
                .strip_prefix(dist)
                .unwrap_or(&artifact.path)
                .to_string_lossy()
                .replace('\\', "/");
            entries.push((name, size));
            total += size;
        }
    }

    if entries.is_empty() {
        return;
    }

    let max_name_len = entries.iter().map(|(n, _)| n.len()).max().unwrap_or(0);

    log.status("");
    log.status("Artifact Sizes:");
    for (name, size) in &entries {
        log.status(&format!(
            "  {:<width$}  {}",
            name,
            format_size(*size),
            width = max_name_len
        ));
    }
    log.status(&format!(
        "  {:<width$}  {}",
        "Total:",
        format_size(total),
        width = max_name_len
    ));
}
