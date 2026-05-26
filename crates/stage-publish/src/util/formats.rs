//! Default package formats and filename-format matching for HTTP
//! publishers (Artifactory, Fury, CloudSmith).

/// Default package formats for push-based publishers (Fury, CloudSmith).
pub(crate) fn default_package_formats() -> Vec<&'static str> {
    vec!["apk", "deb", "rpm"]
}

/// Check if a filename matches any of the given format extensions.
#[allow(dead_code)]
pub(crate) fn format_matches(filename: &str, formats: &[impl AsRef<str>]) -> bool {
    formats
        .iter()
        .any(|fmt| filename.ends_with(&format!(".{}", fmt.as_ref())))
}
