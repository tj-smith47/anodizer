//! Default package formats and filename-format matching for HTTP
//! publishers (Artifactory, Fury, CloudSmith).

/// Default package formats for push-based publishers (Fury, CloudSmith).
pub(crate) fn default_package_formats() -> Vec<&'static str> {
    vec!["apk", "deb", "rpm"]
}

/// Check if a filename matches any of the given format extensions.
///
/// Case-insensitive on both sides: a `.DEB` artifact matches a `deb` filter
/// and vice versa. The fold is applied to the filename and to each format
/// slug so neither a mixed-case artifact name nor a mixed-case config value
/// silently fails to match.
pub(crate) fn format_matches(filename: &str, formats: &[impl AsRef<str>]) -> bool {
    let lower = filename.to_ascii_lowercase();
    formats
        .iter()
        .any(|fmt| lower.ends_with(&format!(".{}", fmt.as_ref().to_ascii_lowercase())))
}

#[cfg(test)]
mod tests {
    use super::format_matches;

    #[test]
    fn matches_plain_extension() {
        assert!(format_matches("myapp_1.0.0_amd64.deb", &["deb"]));
        assert!(format_matches("myapp-1.0.0.rpm", &["deb", "rpm"]));
        assert!(!format_matches(
            "myapp-1.0.0.tar.gz",
            &["deb", "rpm", "apk"]
        ));
    }

    #[test]
    fn folds_case_on_both_sides() {
        // Mixed-case filename extension.
        assert!(format_matches("MyApp.DEB", &["deb"]));
        // Mixed-case format slug.
        assert!(format_matches("myapp.deb", &["DEB"]));
        // Both mixed.
        assert!(format_matches("MyApp.Rpm", &["RpM"]));
    }
}
