//! `PublisherKind` — the single source of truth for anodizer's publisher
//! vocabulary.
//!
//! Every publisher anodizer knows is one variant here. The whole release
//! tool derives its publisher-keyed sets from this one enum via **exhaustive
//! `match`** (no `_ =>` wildcard), so adding a publisher fails to compile
//! until every derivation has been taught about it:
//!
//! - [`PublisherKind::token`] — the canonical, lowercase selector token
//!   (the same string [`crate::Publisher::name`] returns, the one
//!   `--publishers` and `--skip` key on).
//! - [`PublisherKind::is_publish_stage`] — the explicit per-variant predicate
//!   that distinguishes the trait-dispatched publishers (instantiable as a
//!   `Box<dyn Publisher>` in `stage-publish`'s registry) from the
//!   out-of-dispatch *publish stages* (`blob`, `snapcraft-publish`, `docker`,
//!   `docker-sign`, `announce`) that fire their irreversible publish from a
//!   pipeline stage instead. This replaces the former hand-maintained
//!   `PUBLISH_STAGE_PUBLISHERS` list.
//!
//! The publisher portion of [`crate::context::valid_release_skips`] and
//! `stage-publish`'s `valid_publisher_names` / `all_publishers` are all driven
//! off [`PublisherKind::iter`], so the `--skip` / `--publishers` vocabulary
//! and the trait registry can never drift from this enum again.
//!
//! Lives in `anodizer-core` (not `stage-publish`) because
//! [`crate::context`] needs the publisher tokens to assemble the `--skip`
//! vocabulary, and `core` must not depend on `stage-publish`. The reverse
//! mapping — variant → concrete `Publisher` impl — lives in
//! `stage-publish`'s registry, where the concrete types are visible.

use strum::EnumIter;

/// Every publisher anodizer can run, as an exhaustive enum.
///
/// Variant order matches the historical `all_publishers()` ordering (the 18
/// trait-dispatched publishers first, in registry order) followed by the five
/// out-of-dispatch publish stages, so [`Self::iter`]-derived lists preserve
/// the prior error-message ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumIter)]
pub enum PublisherKind {
    // ----- Trait-dispatched publishers (instantiable in the registry) -----
    /// `cargo publish` to crates.io.
    Cargo,
    /// Docker Hub image description / overview update.
    Dockerhub,
    /// JFrog Artifactory generic-repository upload.
    Artifactory,
    /// Generic HTTP(S) asset upload targets (`uploads:`).
    Uploads,
    /// Cloudsmith package upload.
    Cloudsmith,
    /// GitHub / GitLab / Gitea release + asset upload.
    GithubRelease,
    /// Homebrew formula / cask tap.
    Homebrew,
    /// Scoop bucket manifest.
    Scoop,
    /// Nix overlay / flake.
    Nix,
    /// MCP registry server entry.
    Mcp,
    /// Our-repo AUR `PKGBUILD` (binary).
    Aur,
    /// krew-index plugin manifest.
    Krew,
    /// SchemaStore catalog entry.
    Schemastore,
    /// npm package publish.
    Npm,
    /// Gemfury package push.
    Gemfury,
    /// Chocolatey community-repository push (moderated).
    Chocolatey,
    /// winget-pkgs manifest PR (moderated).
    Winget,
    /// Upstream AUR `PKGBUILD` submission.
    UpstreamAur,
    // ----- Out-of-dispatch publish stages (NOT trait-registered) -----
    /// Object-store upload (`BlobStage`).
    Blob,
    /// Snap Store upload (`SnapcraftPublishStage`).
    SnapcraftPublish,
    /// Container image build + push (`DockerStage`).
    Docker,
    /// cosign image signature push (`DockerSignStage`).
    DockerSign,
    /// Announce broadcast (`AnnounceStage`).
    Announce,
}

impl PublisherKind {
    /// Canonical, lowercase selector token for this publisher.
    ///
    /// This is the exact string [`crate::Publisher::name`] returns for the
    /// trait-dispatched publishers and the stage's skip token for the
    /// out-of-dispatch publish stages — the one `--publishers` and `--skip`
    /// both key on. Exhaustive `match` (no wildcard): a new variant must be
    /// given a token here or the crate fails to compile.
    pub const fn token(self) -> &'static str {
        match self {
            Self::Cargo => "cargo",
            Self::Dockerhub => "dockerhub",
            Self::Artifactory => "artifactory",
            Self::Cloudsmith => "cloudsmith",
            Self::Uploads => "uploads",
            Self::GithubRelease => "github-release",
            Self::Homebrew => "homebrew",
            Self::Scoop => "scoop",
            Self::Nix => "nix",
            Self::Mcp => "mcp",
            Self::Aur => "aur",
            Self::Krew => "krew",
            Self::Schemastore => "schemastore",
            Self::Npm => "npm",
            Self::Gemfury => "gemfury",
            Self::Chocolatey => "chocolatey",
            Self::Winget => "winget",
            Self::UpstreamAur => "upstream-aur",
            Self::Blob => "blob",
            Self::SnapcraftPublish => "snapcraft-publish",
            Self::Docker => "docker",
            Self::DockerSign => "docker-sign",
            Self::Announce => "announce",
        }
    }

    /// Whether this publisher fires its external publish from a pipeline
    /// **stage** rather than the trait-based dispatch chokepoint.
    ///
    /// `true` for `blob` / `snapcraft-publish` / `docker` / `docker-sign` /
    /// `announce`: these own their own stages and record their outcomes
    /// directly, so they are deliberately NOT registered as
    /// `Box<dyn Publisher>` in `all_publishers` (a parallel trait
    /// registration would double-publish). They are still part of the
    /// `--skip` / `--publishers` vocabulary.
    ///
    /// `false` for the 18 trait-dispatched publishers that `stage-publish`'s
    /// registry instantiates and `dispatch` iterates. Exhaustive `match` so a
    /// new variant must declare which side it is on.
    pub const fn is_publish_stage(self) -> bool {
        match self {
            Self::Cargo
            | Self::Dockerhub
            | Self::Artifactory
            | Self::Cloudsmith
            | Self::Uploads
            | Self::GithubRelease
            | Self::Homebrew
            | Self::Scoop
            | Self::Nix
            | Self::Mcp
            | Self::Aur
            | Self::Krew
            | Self::Schemastore
            | Self::Npm
            | Self::Gemfury
            | Self::Chocolatey
            | Self::Winget
            | Self::UpstreamAur => false,
            Self::Blob
            | Self::SnapcraftPublish
            | Self::Docker
            | Self::DockerSign
            | Self::Announce => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use strum::IntoEnumIterator;

    #[test]
    fn tokens_are_unique_and_lowercase() {
        let mut seen = BTreeSet::new();
        for k in PublisherKind::iter() {
            let t = k.token();
            assert!(seen.insert(t), "duplicate publisher token: {t}");
            assert_eq!(
                t,
                t.to_lowercase(),
                "publisher token must be lowercase: {t}"
            );
            assert!(!t.is_empty(), "publisher token must be non-empty");
        }
    }

    #[test]
    fn the_five_publish_stages_are_classified() {
        let stage_tokens: BTreeSet<&str> = PublisherKind::iter()
            .filter(|k| k.is_publish_stage())
            .map(|k| k.token())
            .collect();
        assert_eq!(
            stage_tokens,
            BTreeSet::from([
                "blob",
                "snapcraft-publish",
                "docker",
                "docker-sign",
                "announce"
            ]),
            "publish-stage set drifted from the documented five"
        );
    }

    #[test]
    fn trait_publishers_count_is_eighteen() {
        let trait_count = PublisherKind::iter()
            .filter(|k| !k.is_publish_stage())
            .count();
        assert_eq!(trait_count, 18, "expected 18 trait-dispatched publishers");
    }
}
