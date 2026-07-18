// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Returns the default formats for CloudSmith uploads: apk, deb, rpm.
pub fn cloudsmith_default_formats() -> Vec<&'static str> {
    crate::util::default_package_formats()
}

/// Check if a filename matches any of the given format extensions.
///
/// The user-facing CloudSmith config (per Pro docs) uses `apk`, `deb`,
/// `rpm`, `src.rpm` as filter slugs. CloudSmith's API path slug for
/// `.apk` files is `alpine`, so users may write either spelling — both
/// are recognized here. `srpm` / `src.rpm` strip the dotted prefix when
/// matched against a `.src.rpm` filename (the dotted slug otherwise
/// won't match through the generic suffix helper).
pub fn cloudsmith_format_matches(filename: &str, formats: &[impl AsRef<str>]) -> bool {
    let lower = filename.to_ascii_lowercase();
    for fmt in formats {
        let raw = fmt.as_ref();
        let suffix = match raw {
            "alpine" => ".apk",
            "srpm" | "src.rpm" => ".src.rpm",
            // Non-aliased slug: defer to the shared case-folding matcher so
            // a mixed-case slug (e.g. `DEB`) still matches a `.deb` file.
            other => {
                if crate::util::format_matches(&lower, &[other]) {
                    return true;
                }
                continue;
            }
        };
        if lower.ends_with(suffix) {
            return true;
        }
    }
    false
}

/// Cloudsmith API base URL (used for files/create and packages/upload/*).
pub(crate) const CLOUDSMITH_API_BASE: &str = "https://api.cloudsmith.io/v1";

/// Resolve the Cloudsmith API base URL from an injected [`EnvSource`].
/// Defaults to [`CLOUDSMITH_API_BASE`]; `ANODIZE_CLOUDSMITH_API_BASE` overrides
/// it so tests can point the 3-step upload flow (files/create → S3 presigned →
/// packages/upload) at a local responder without a real network call. Threading
/// the read through an `EnvSource` (rather than `std::env::var`) lets a test
/// inject the base via a [`MapEnvSource`](anodizer_core::MapEnvSource) without
/// mutating the process env — eliminating the cross-test race where one test's
/// base pointed another test's HTTP call at a torn-down listener. Production
/// never sets the variable.
pub(crate) fn cloudsmith_api_base_from<E: anodizer_core::EnvSource + ?Sized>(env: &E) -> String {
    env.var("ANODIZE_CLOUDSMITH_API_BASE")
        .unwrap_or_else(|| CLOUDSMITH_API_BASE.to_string())
}

/// Build the CloudSmith upload URL for the given org, repo, format, and distribution.
///
/// Retained for dry-run logging parity with prior versions. The live code
/// path uses the canonical 3-step API flow (files/create → S3 presigned
/// upload → packages/upload/{format}/) rather than this URL directly.
pub fn cloudsmith_upload_url(org: &str, repo: &str, format: &str, distribution: &str) -> String {
    format!(
        "{}/packages/{}/{}/upload/{}/ (distribution={})",
        CLOUDSMITH_API_BASE, org, repo, format, distribution
    )
}

/// Detect the package format from a filename extension.
///
/// Returns the CloudSmith API-side format slug (`alpine`, `deb`, `rpm`,
/// `srpm`, or `raw`). `.src.rpm` is matched BEFORE `.rpm` because the
/// suffix overlaps — CloudSmith treats source RPMs as a distinct format
/// at `/packages/<org>/<repo>/upload/srpm/`.
pub(crate) fn detect_format(filename: &str) -> &str {
    let lower = filename.to_ascii_lowercase();
    if lower.ends_with(".src.rpm") {
        "srpm"
    } else if lower.ends_with(".deb") {
        "deb"
    } else if lower.ends_with(".rpm") {
        "rpm"
    } else if lower.ends_with(".apk") {
        "alpine"
    } else {
        "raw"
    }
}

/// CloudSmith API format slugs that accept a Debian `component:` field.
/// Other formats silently ignore `component`; the upload code drops it
/// to avoid noise in the request body.
pub(crate) const COMPONENT_BEARING_FORMATS: &[&str] = &["deb"];

/// The accept-all distribution slug CloudSmith requires for a `format` when
/// the user configured no `distributions.<format>` entry, or `None` for
/// formats that don't require a distribution.
///
/// CloudSmith's `.deb` and `alpine`/`.apk` uploads MUST carry a
/// `distribution`; omitting it leaves the package accepted-but-unindexed and
/// thus not `apt`/`apk` installable. Per CloudSmith's docs the catch-all
/// values are `any-distro/any-version` (deb) and `alpine/any-version`
/// (alpine), which keep the package installable across distro versions while
/// still letting a user pin a real distro (`debian/bookworm`) via config.
/// `rpm`/`srpm`/`raw` do not require a distribution, so they return `None`
/// and continue to upload with the key omitted.
pub(crate) fn cloudsmith_default_distribution(format: &str) -> Option<&'static str> {
    match format {
        "deb" => Some("any-distro/any-version"),
        "alpine" => Some("alpine/any-version"),
        _ => None,
    }
}
