//! Opt-in post-release verification configuration.
//!
//! The `verify_release` block drives a verification gate that runs LAST in
//! the release pipeline — *after* the release is created and every publisher
//! has run. Because it runs after the irreversible publish, it does NOT
//! block or undo anything: it REPORTS post-publish defects (and exits
//! non-zero so CI surfaces them), but the release is already live.
//!
//! Three independently-toggleable checks:
//!
//! - **asset-existence** ([`Self::assert_assets`]) — every produced artifact
//!   has a matching UPLOADED asset on the published release. Catches the
//!   partial uploads GitHub silently tolerates.
//! - **install smoke-test** ([`Self::install_smoke`]) — installs each Linux
//!   package (`.deb` / `.rpm` / `.apk`) in a pinned container and runs
//!   `<bin> --version`. Skipped with a notice when Docker is unavailable.
//! - **libc ceiling** ([`Self::glibc_ceiling`]) — fails if any glibc-linked
//!   `.deb` requires a glibc newer than the configured floor. musl binaries
//!   have no glibc requirement and are skipped — which is the whole point:
//!   musl hides a glibc-floor regression that this check is meant to surface.
//!
//! The block is off unless [`Self::enabled`] is `true`. Defaults mirror the
//! [`PostPublishPollConfig`](super::PostPublishPollConfig) style:
//! `#[serde(default, deny_unknown_fields)]`, opt-in `enabled`, sane derived
//! defaults for everything else.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Default container image for `.deb` install smoke-tests.
pub const DEFAULT_DEB_IMAGE: &str = "debian:stable-slim";
/// Default container image for `.rpm` install smoke-tests.
pub const DEFAULT_RPM_IMAGE: &str = "fedora:latest";
/// Default container image for `.apk` install smoke-tests.
pub const DEFAULT_APK_IMAGE: &str = "alpine:latest";

/// Top-level `verify_release:` block.
///
/// See the module-level docs for the verification lifecycle. The gate is a
/// no-op unless `enabled: true`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct VerifyReleaseConfig {
    /// Whether to run the post-release verification gate at all. Default
    /// `false` — the gate is opt-in because it needs the published release to
    /// already exist (it runs after publish) and, for install-smoke, a Docker
    /// daemon.
    pub enabled: bool,
    /// Assert that every produced artifact has a matching uploaded asset on
    /// the published release, and that every signature / certificate / SBOM
    /// asset the resolved `signs:` / `sboms:` config demands exists there too
    /// (derived from config + the artifact set, so a sign or SBOM stage that
    /// silently produced nothing still fails the gate with the exact missing
    /// names; intentional skips — `if:` falsy, `skip:` truthy, `--skip=sign` —
    /// create no expectations). Default `true` (no extra config: anodizer
    /// already knows the produced set and can fetch the release's asset list).
    /// Independent of Docker and the network smoke-test.
    pub assert_assets: bool,
    /// Assert that every publisher that succeeded this run actually LANDED:
    /// each published crate version is visible on the crates.io sparse index,
    /// each npm package version is visible on its registry, and each uploaded
    /// blob object exists in its bucket. Default `true` (no extra config: the
    /// run's own publish report already carries every coordinate the probes
    /// need). Publishers that did not run — or did not succeed — are skipped.
    pub assert_landing: bool,
    /// Per-package install smoke-test images. When `None`, smoke-testing is
    /// off. When present, each package type that produced an artifact is
    /// installed in its (configured or default) container and `<bin>
    /// --version` is run.
    pub install_smoke: Option<InstallSmokeConfig>,
    /// glibc version ceiling, e.g. `"2.36"`. When any glibc-linked `.deb`
    /// requires a glibc NEWER than this floor, the gate reports it and exits
    /// non-zero. `None` (the default) disables the libc check entirely. musl
    /// binaries have no glibc requirement and are skipped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub glibc_ceiling: Option<String>,
}

impl Default for VerifyReleaseConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            assert_assets: true,
            assert_landing: true,
            install_smoke: None,
            glibc_ceiling: None,
        }
    }
}

impl VerifyReleaseConfig {
    /// Whether the asset-existence check should run: only when the whole gate
    /// is enabled AND `assert_assets` is set.
    pub fn assert_assets_enabled(&self) -> bool {
        self.enabled && self.assert_assets
    }

    /// Whether the libc-ceiling check should run: only when the whole gate is
    /// enabled AND a ceiling was configured.
    pub fn glibc_check_enabled(&self) -> bool {
        self.enabled && self.glibc_ceiling.is_some()
    }

    /// Whether the per-publisher landing checks should run: only when the
    /// whole gate is enabled AND `assert_landing` is set.
    pub fn landing_checks_enabled(&self) -> bool {
        self.enabled && self.assert_landing
    }
}

/// Per-package install smoke-test image overrides.
///
/// Each field is the container image used to install + version-check that
/// package type. A `None` field falls back to its sane default
/// ([`DEFAULT_DEB_IMAGE`], [`DEFAULT_RPM_IMAGE`], [`DEFAULT_APK_IMAGE`]).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct InstallSmokeConfig {
    /// Image override for `.deb` packages. Default [`DEFAULT_DEB_IMAGE`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deb: Option<SmokeImage>,
    /// Image override for `.rpm` packages. Default [`DEFAULT_RPM_IMAGE`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rpm: Option<SmokeImage>,
    /// Image override for `.apk` packages. Default [`DEFAULT_APK_IMAGE`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub apk: Option<SmokeImage>,
}

impl InstallSmokeConfig {
    /// Resolve the `.deb` smoke image, applying the default when unset.
    pub fn deb_image(&self) -> &str {
        self.deb
            .as_ref()
            .map(|i| i.image.as_str())
            .unwrap_or(DEFAULT_DEB_IMAGE)
    }

    /// Resolve the `.rpm` smoke image, applying the default when unset.
    pub fn rpm_image(&self) -> &str {
        self.rpm
            .as_ref()
            .map(|i| i.image.as_str())
            .unwrap_or(DEFAULT_RPM_IMAGE)
    }

    /// Resolve the `.apk` smoke image, applying the default when unset.
    pub fn apk_image(&self) -> &str {
        self.apk
            .as_ref()
            .map(|i| i.image.as_str())
            .unwrap_or(DEFAULT_APK_IMAGE)
    }
}

/// A single per-package smoke-test image selection.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SmokeImage {
    /// The container image reference (e.g. `debian:12`, `fedora:40`).
    pub image: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_disabled_with_assets_on() {
        let c = VerifyReleaseConfig::default();
        assert!(!c.enabled, "the gate is opt-in");
        assert!(c.assert_assets, "asset-existence defaults on");
        assert!(c.assert_landing, "landing checks default on");
        assert!(c.install_smoke.is_none(), "smoke off by default");
        assert!(c.glibc_ceiling.is_none(), "libc check off by default");
        // The sub-check gates are still off because the whole gate is off.
        assert!(!c.assert_assets_enabled());
        assert!(!c.glibc_check_enabled());
        assert!(!c.landing_checks_enabled());
    }

    #[test]
    fn assert_landing_independently_toggleable() {
        let yaml = "enabled: true\nassert_landing: false\n";
        let c: VerifyReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(!c.landing_checks_enabled(), "landing checks opted out");
        assert!(c.assert_assets_enabled(), "asset check unaffected");
    }

    #[test]
    fn empty_yaml_yields_defaults() {
        let c: VerifyReleaseConfig = serde_yaml_ng::from_str("{}").unwrap();
        assert_eq!(c, VerifyReleaseConfig::default());
    }

    #[test]
    fn parses_full_block() {
        let yaml = r#"
enabled: true
assert_assets: true
install_smoke:
  deb: { image: "debian:12" }
  rpm: { image: "fedora:40" }
  apk: { image: "alpine:3.20" }
glibc_ceiling: "2.36"
"#;
        let c: VerifyReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(c.enabled);
        assert!(c.assert_assets_enabled());
        assert!(c.glibc_check_enabled());
        assert_eq!(c.glibc_ceiling.as_deref(), Some("2.36"));
        let smoke = c.install_smoke.unwrap();
        assert_eq!(smoke.deb_image(), "debian:12");
        assert_eq!(smoke.rpm_image(), "fedora:40");
        assert_eq!(smoke.apk_image(), "alpine:3.20");
    }

    #[test]
    fn smoke_images_fall_back_to_defaults() {
        // Only deb configured; rpm/apk take their sane defaults.
        let yaml = "enabled: true\ninstall_smoke:\n  deb: { image: \"debian:12\" }\n";
        let c: VerifyReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let smoke = c.install_smoke.unwrap();
        assert_eq!(smoke.deb_image(), "debian:12");
        assert_eq!(smoke.rpm_image(), DEFAULT_RPM_IMAGE);
        assert_eq!(smoke.apk_image(), DEFAULT_APK_IMAGE);
    }

    #[test]
    fn empty_install_smoke_uses_all_defaults() {
        let yaml = "enabled: true\ninstall_smoke: {}\n";
        let c: VerifyReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let smoke = c.install_smoke.unwrap();
        assert_eq!(smoke.deb_image(), DEFAULT_DEB_IMAGE);
        assert_eq!(smoke.rpm_image(), DEFAULT_RPM_IMAGE);
        assert_eq!(smoke.apk_image(), DEFAULT_APK_IMAGE);
    }

    #[test]
    fn glibc_ceiling_absent_disables_check() {
        let yaml = "enabled: true\n";
        let c: VerifyReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(!c.glibc_check_enabled(), "no ceiling => libc check off");
    }

    #[test]
    fn assert_assets_independently_toggleable() {
        let yaml = "enabled: true\nassert_assets: false\n";
        let c: VerifyReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(!c.assert_assets_enabled(), "asset check opted out");
        assert!(c.enabled, "gate still enabled for other checks");
    }

    #[test]
    fn unknown_field_rejected() {
        let yaml = "enabled: true\nbogus: 1\n";
        let res: Result<VerifyReleaseConfig, _> = serde_yaml_ng::from_str(yaml);
        assert!(res.is_err(), "deny_unknown_fields must reject typos");
    }

    #[test]
    fn unknown_smoke_field_rejected() {
        let yaml = "enabled: true\ninstall_smoke:\n  deb: { image: \"x\", typo: 1 }\n";
        let res: Result<VerifyReleaseConfig, _> = serde_yaml_ng::from_str(yaml);
        assert!(res.is_err(), "deny_unknown_fields on SmokeImage");
    }
}
