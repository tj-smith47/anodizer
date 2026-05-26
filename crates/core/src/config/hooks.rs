use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use crate::artifact::ArtifactKind;

// ---------------------------------------------------------------------------
// HooksConfig
// ---------------------------------------------------------------------------

/// Top-level lifecycle hooks for `before` and `after` blocks.
/// Each block carries a list of hook commands that run around the
/// entire pipeline (not individual stages).
///
/// The canonical key is `hooks:` for both `before:` and `after:` to
/// match GoReleaser Pro (`hooks.md`). The `post:` spelling is accepted
/// as a serde alias on `hooks` for back-compat with the previous
/// anodizer spelling; users with `after: { post: [...] }` keep working
/// and a deprecation warning is logged when both spellings appear in
/// the same block (see [`HooksConfig::merge_hook_aliases`]).
#[derive(Debug, Clone, PartialEq, Default, JsonSchema)]
pub struct HooksConfig {
    /// Commands to run when the block fires. The wire format accepts
    /// either `hooks:` (canonical, GoReleaser-aligned) or the legacy
    /// `post:` spelling; both fold into this field at parse time.
    pub hooks: Option<Vec<HookEntry>>,
    /// Legacy alias for `hooks:` (anodizer pre-v0.4). Always `None`
    /// after parsing — `merge_hook_aliases` collapses it into `hooks`.
    /// Present on the struct only because `Deserialize` writes through
    /// it before the fold step.
    #[doc(hidden)]
    pub post: Option<Vec<HookEntry>>,
}

impl HooksConfig {
    /// Fold the deprecated `post:` spelling into `hooks:` so downstream
    /// readers consult one field. Emits a `tracing::warn!` with one of two
    /// suffixes depending on whether both spellings appeared or only the
    /// legacy one — both messages carry the same "switch to 'hooks:'"
    /// guidance, with the conflict case adding the "and ignoring 'post:'"
    /// note so the user knows which side won.
    fn merge_hook_aliases(&mut self) {
        let has_hooks = self.hooks.as_ref().is_some_and(|v| !v.is_empty());
        let has_post = self.post.as_ref().is_some_and(|v| !v.is_empty());
        match (has_hooks, has_post) {
            (true, true) => {
                tracing::warn!(
                    "DEPRECATION: top-level hooks block has both 'hooks:' and 'post:' \
                     — using 'hooks:' and ignoring 'post:'. The 'post:' spelling is \
                     renamed to 'hooks:' for GoReleaser parity; remove the 'post:' \
                     key from your config."
                );
                self.post = None;
            }
            (false, true) => {
                tracing::warn!(
                    "DEPRECATION: top-level 'after.post:' / 'before.post:' is renamed to \
                     'hooks:' for GoReleaser parity. The 'post:' spelling still works \
                     but will be removed in a future release; switch to 'hooks:'."
                );
                self.hooks = self.post.take();
            }
            // (true, false): canonical shape, nothing to do.
            // (false, false): empty block, nothing to do.
            _ => {}
        }
    }
}

impl Serialize for HooksConfig {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let count = self.hooks.is_some() as usize + self.post.is_some() as usize;
        let mut state = serializer.serialize_struct("HooksConfig", count)?;
        if let Some(ref h) = self.hooks {
            state.serialize_field("hooks", h)?;
        }
        if let Some(ref p) = self.post {
            state.serialize_field("post", p)?;
        }
        state.end()
    }
}

impl<'de> Deserialize<'de> for HooksConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Raw {
            hooks: Option<Vec<HookEntry>>,
            post: Option<Vec<HookEntry>>,
        }
        let raw = Raw::deserialize(deserializer)?;
        let mut out = HooksConfig {
            hooks: raw.hooks,
            post: raw.post,
        };
        out.merge_hook_aliases();
        Ok(out)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct StructuredHook {
    /// Command to run.
    ///
    /// The entire string is interpreted by `sh -c`, so shell metacharacters
    /// (`|`, `;`, `&&`, backticks, `$()`, redirects, globs) are honoured —
    /// any templated values folded into `cmd` become part of the shell
    /// command and are subject to word-splitting and metacharacter expansion.
    /// Keep templated user-config values out of `cmd` when possible, or quote
    /// them defensively (e.g. `'{{ .Env.FOO }}'`). Hooks already run with
    /// `env_clear()` plus an allow-list, so secrets in `$ENV` are not
    /// inherited unless explicitly listed in `env`.
    pub cmd: String,
    /// Working directory for the command (defaults to project root).
    pub dir: Option<String>,
    /// Environment variables for the command.
    #[serde(default)]
    pub env: Option<Vec<String>>,
    /// When true, capture and log stdout/stderr of the command.
    pub output: Option<bool>,
    /// Template-conditional: when set, the hook only runs if the rendered
    /// result is truthy (not `"false"` / `"0"` / `"no"` / empty). Render
    /// failure hard-errors (not silent-skip). Mirrors GoReleaser OSS v2.7+
    /// `before.hooks[].if:` / per-build / per-archive hook `if:` surface.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
    /// Artifact-id allow-list (`before_publish:` only). When `Some`, the
    /// per-artifact iteration only fires for artifacts whose
    /// `metadata["id"]` matches one of these strings. `None` (the default)
    /// imposes no id constraint. Ignored by lifecycle hook sites
    /// (`before:` / `after:` / per-build / per-archive) — those run once
    /// per pipeline tick, not per artifact.
    pub ids: Option<Vec<String>>,
    /// Artifact-kind filter (`before_publish:` only). When `Some`, the
    /// per-artifact iteration only fires for artifacts whose
    /// [`ArtifactKind`] matches the filter. `None` is equivalent to
    /// [`BeforePublishArtifactFilter::All`] (every registered artifact).
    /// Ignored by lifecycle hook sites for the same reason as `ids`.
    pub artifacts: Option<BeforePublishArtifactFilter>,
}

/// Artifact-type filter for `before_publish[*].artifacts`.
///
/// Mirrors GoReleaser Pro's `before_publish[*].artifacts` enum
/// (`checksum` / `source` / `package` / `installer` / `diskimage` /
/// `archive` / `binary` / `sbom` / `image` / `all`). Maps each variant
/// to a predicate over [`ArtifactKind`]:
///
/// | Variant | Matched [`ArtifactKind`] values |
/// |---|---|
/// | `Checksum` | `Checksum` |
/// | `Source` | `SourceArchive`, `SourcePkgBuild`, `SourceSrcInfo`, `SourceRpm` |
/// | `Package` | `LinuxPackage`, `Snap`, `PublishableSnapcraft`, `Flatpak` |
/// | `Installer` | `Installer`, `MacOsPackage` (GR Pro's "installer" covers MSI/NSIS/Pkg) |
/// | `DiskImage` | `DiskImage` |
/// | `Archive` | `Archive`, `Makeself` |
/// | `Binary` | `Binary`, `UploadableBinary`, `UniversalBinary` |
/// | `Sbom` | `Sbom` |
/// | `Image` | `DockerImage`, `DockerImageV2`, `PublishableDockerImage` (multi-arch `DockerManifest` excluded — covers individual images only) |
/// | `All` | every kind |
///
/// Mapping notes:
/// - `Source` includes RPM-source variants since GR groups all source-derived
///   artifacts under one bucket.
/// - `Installer` covers macOS `.pkg` (`MacOsPackage`) alongside Windows
///   MSI/NSIS — GR docs say "installer" without OS qualification, so
///   anodizer follows GR.
/// - `Image` deliberately excludes [`ArtifactKind::DockerManifest`] /
///   [`ArtifactKind::DockerDigest`]; those are multi-arch index entries
///   and don't correspond to a scannable image blob. Matches GR's
///   per-image hook semantics (the multi-arch manifest is published
///   separately and isn't a vulnerability scan target).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BeforePublishArtifactFilter {
    Checksum,
    Source,
    Package,
    Installer,
    #[serde(alias = "diskimage")]
    DiskImage,
    Archive,
    Binary,
    Sbom,
    Image,
    #[default]
    All,
}

impl BeforePublishArtifactFilter {
    /// Returns `true` when `kind` should run the hook under this filter.
    pub fn matches(self, kind: ArtifactKind) -> bool {
        match self {
            Self::All => true,
            Self::Checksum => matches!(kind, ArtifactKind::Checksum),
            Self::Source => matches!(
                kind,
                ArtifactKind::SourceArchive
                    | ArtifactKind::SourcePkgBuild
                    | ArtifactKind::SourceSrcInfo
                    | ArtifactKind::SourceRpm
            ),
            Self::Package => matches!(
                kind,
                ArtifactKind::LinuxPackage
                    | ArtifactKind::Snap
                    | ArtifactKind::PublishableSnapcraft
                    | ArtifactKind::Flatpak
            ),
            Self::Installer => matches!(kind, ArtifactKind::Installer | ArtifactKind::MacOsPackage),
            Self::DiskImage => matches!(kind, ArtifactKind::DiskImage),
            Self::Archive => matches!(kind, ArtifactKind::Archive | ArtifactKind::Makeself),
            Self::Binary => matches!(
                kind,
                ArtifactKind::Binary
                    | ArtifactKind::UploadableBinary
                    | ArtifactKind::UniversalBinary
            ),
            Self::Sbom => matches!(kind, ArtifactKind::Sbom),
            Self::Image => matches!(
                kind,
                ArtifactKind::DockerImage
                    | ArtifactKind::DockerImageV2
                    | ArtifactKind::PublishableDockerImage
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum HookEntry {
    Simple(String),
    Structured(StructuredHook),
}

impl PartialEq<&str> for HookEntry {
    fn eq(&self, other: &&str) -> bool {
        match self {
            HookEntry::Simple(s) => s.as_str() == *other,
            HookEntry::Structured(h) => h.cmd.as_str() == *other,
        }
    }
}

impl<'de> Deserialize<'de> for HookEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        match &value {
            serde_json::Value::String(s) => Ok(HookEntry::Simple(s.clone())),
            serde_json::Value::Object(_) => {
                let hook: StructuredHook =
                    serde_json::from_value(value).map_err(serde::de::Error::custom)?;
                Ok(HookEntry::Structured(hook))
            }
            _ => Err(serde::de::Error::custom(
                "hook entry must be a string or an object with cmd/dir/env/output",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::sync::{Arc, Mutex, MutexGuard};
    use tracing::subscriber::with_default;
    use tracing_subscriber::fmt::MakeWriter;

    /// Shared buffer writer that captures `tracing` output into a `Vec<u8>`.
    #[derive(Clone, Default)]
    struct BufferWriter(Arc<Mutex<Vec<u8>>>);

    impl BufferWriter {
        fn captured(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).to_string()
        }
    }

    impl io::Write for BufferWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// `BufferWriterHandle` matches the `MakeWriter` contract: each
    /// `make_writer` call returns a cheap clone that writes into the
    /// same `Arc<Mutex<Vec<u8>>>` as every other clone.
    impl<'a> MakeWriter<'a> for BufferWriter {
        type Writer = BufferWriterGuard<'a>;
        fn make_writer(&'a self) -> Self::Writer {
            BufferWriterGuard(self.0.lock().unwrap())
        }
    }

    struct BufferWriterGuard<'a>(MutexGuard<'a, Vec<u8>>);
    impl io::Write for BufferWriterGuard<'_> {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Run `body` with a tracing subscriber that captures every event into
    /// the returned buffer; assertions then inspect the captured text.
    fn capture_warnings<F: FnOnce()>(body: F) -> String {
        let buf = BufferWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_max_level(tracing::Level::WARN)
            .without_time()
            .with_ansi(false)
            .finish();
        with_default(subscriber, body);
        buf.captured()
    }

    /// Legacy-only spelling (post: set, hooks: unset) folds into `hooks`
    /// and emits a DEPRECATION warning that points at the canonical name.
    #[test]
    fn legacy_post_only_folds_and_warns() {
        let captured = capture_warnings(|| {
            let mut cfg = HooksConfig {
                hooks: None,
                post: Some(vec![HookEntry::Simple("legacy.sh".to_string())]),
            };
            cfg.merge_hook_aliases();
            assert_eq!(
                cfg.hooks.as_deref().map(|v| v.len()),
                Some(1),
                "post: should have moved into hooks:"
            );
            assert!(cfg.post.is_none(), "post: must be cleared after merge");
        });
        assert!(
            captured.contains("DEPRECATION"),
            "expected DEPRECATION marker in warning: {captured}"
        );
        assert!(
            captured.contains("renamed to 'hooks:'"),
            "legacy-only warning must guide to 'hooks:' rename: {captured}"
        );
    }

    /// Both spellings set: `hooks:` wins, `post:` is dropped, and the
    /// emitted warning explicitly calls out the conflict.
    #[test]
    fn both_present_keeps_hooks_drops_post_and_warns() {
        let captured = capture_warnings(|| {
            let mut cfg = HooksConfig {
                hooks: Some(vec![HookEntry::Simple("modern.sh".to_string())]),
                post: Some(vec![HookEntry::Simple("legacy.sh".to_string())]),
            };
            cfg.merge_hook_aliases();
            assert!(cfg.post.is_none(), "post: must be cleared on conflict");
            let names: Vec<&str> = cfg
                .hooks
                .as_deref()
                .unwrap()
                .iter()
                .map(|h| match h {
                    HookEntry::Simple(s) => s.as_str(),
                    HookEntry::Structured(s) => s.cmd.as_str(),
                })
                .collect();
            assert_eq!(names, vec!["modern.sh"], "hooks: must win on conflict");
        });
        assert!(
            captured.contains("DEPRECATION"),
            "expected DEPRECATION marker: {captured}"
        );
        assert!(
            captured.contains("ignoring 'post:'"),
            "conflict warning must mention 'ignoring post': {captured}"
        );
    }

    /// Canonical `hooks:`-only block emits no warning and stays as-is.
    #[test]
    fn canonical_hooks_only_emits_no_warning() {
        let captured = capture_warnings(|| {
            let mut cfg = HooksConfig {
                hooks: Some(vec![HookEntry::Simple("modern.sh".to_string())]),
                post: None,
            };
            cfg.merge_hook_aliases();
            assert!(cfg.post.is_none());
            assert_eq!(cfg.hooks.as_deref().map(|v| v.len()), Some(1));
        });
        assert!(
            !captured.contains("DEPRECATION"),
            "canonical hooks-only must not warn: {captured}"
        );
    }
}
