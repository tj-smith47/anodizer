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
/// the conventional spelling. The `post:` spelling is accepted
/// as a serde alias on `hooks` for back-compat with the previous
/// anodizer spelling; users with `after: { post: [...] }` keep working
/// and a deprecation warning is logged when both spellings appear in
/// the same block (see [`HooksConfig::merge_hook_aliases`]).
#[derive(Debug, Clone, PartialEq, Default, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HooksConfig {
    /// Commands to run when the block fires. The wire format accepts
    /// either `hooks:` (canonical) or the legacy
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
        #[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
pub struct StructuredHook {
    /// Command to run.
    ///
    /// The entire string is interpreted by `sh -c`, so shell metacharacters
    /// (`|`, `;`, `&&`, backticks, `$()`, redirects, globs) are honoured —
    /// any templated values folded into `cmd` become part of the shell
    /// command and are subject to word-splitting and metacharacter expansion.
    /// Keep templated user-config values out of `cmd` when possible, or quote
    /// them defensively (e.g. `'{{ Env.FOO }}'`). Hooks already run with
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
    /// failure hard-errors (not silent-skip).
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
/// The `before_publish[*].artifacts` enum
/// (`checksum` / `source` / `package` / `installer` / `diskimage` /
/// `archive` / `binary` / `sbom` / `image` / `all`). Maps each variant
/// to a predicate over [`ArtifactKind`]:
///
/// | Variant | Matched [`ArtifactKind`] values |
/// |---|---|
/// | `Checksum` | `Checksum` |
/// | `Source` | `SourceArchive`, `SourcePkgBuild`, `SourceSrcInfo`, `SourceRpm` |
/// | `Package` | `LinuxPackage`, `Snap`, `PublishableSnapcraft`, `Flatpak` |
/// | `Installer` | `Installer`, `MacOsPackage` |
/// | `DiskImage` | `DiskImage` |
/// | `Archive` | `Archive`, `Makeself` |
/// | `Binary` | `Binary`, `UploadableBinary`, `UniversalBinary` |
/// | `Sbom` | `Sbom` |
/// | `Image` | `DockerImage`, `DockerImageV2`, `PublishableDockerImage` (multi-arch `DockerManifest` excluded — covers individual images only) |
/// | `All` | every kind |
///
/// Mapping notes:
/// - `Source` includes RPM-source variants since all source-derived
///   artifacts under one bucket.
/// - `Installer` covers macOS `.pkg` (`MacOsPackage`) alongside Windows
///   MSI/NSIS — "installer" is used without OS qualification.
/// - `Image` deliberately excludes [`ArtifactKind::DockerManifest`] /
///   [`ArtifactKind::DockerDigest`]; those are multi-arch index entries
///   and don't correspond to a scannable image blob. The
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

    #[test]
    fn filter_all_matches_every_kind() {
        let f = BeforePublishArtifactFilter::All;
        assert!(f.matches(ArtifactKind::Checksum));
        assert!(f.matches(ArtifactKind::Binary));
        assert!(f.matches(ArtifactKind::DockerManifest));
        assert!(f.matches(ArtifactKind::Sbom));
    }

    #[test]
    fn filter_default_is_all() {
        assert_eq!(
            BeforePublishArtifactFilter::default(),
            BeforePublishArtifactFilter::All
        );
    }

    #[test]
    fn filter_source_buckets_all_source_kinds() {
        let f = BeforePublishArtifactFilter::Source;
        assert!(f.matches(ArtifactKind::SourceArchive));
        assert!(f.matches(ArtifactKind::SourcePkgBuild));
        assert!(f.matches(ArtifactKind::SourceSrcInfo));
        assert!(f.matches(ArtifactKind::SourceRpm));
        assert!(!f.matches(ArtifactKind::Archive));
        assert!(!f.matches(ArtifactKind::Binary));
    }

    #[test]
    fn filter_package_excludes_archives_and_binaries() {
        let f = BeforePublishArtifactFilter::Package;
        assert!(f.matches(ArtifactKind::LinuxPackage));
        assert!(f.matches(ArtifactKind::Snap));
        assert!(f.matches(ArtifactKind::PublishableSnapcraft));
        assert!(f.matches(ArtifactKind::Flatpak));
        assert!(!f.matches(ArtifactKind::Archive));
        assert!(!f.matches(ArtifactKind::SourceRpm));
    }

    #[test]
    fn filter_installer_covers_msi_and_macos_pkg() {
        let f = BeforePublishArtifactFilter::Installer;
        assert!(f.matches(ArtifactKind::Installer));
        assert!(f.matches(ArtifactKind::MacOsPackage));
        assert!(!f.matches(ArtifactKind::DiskImage));
    }

    #[test]
    fn filter_archive_includes_makeself_but_not_source_archive() {
        let f = BeforePublishArtifactFilter::Archive;
        assert!(f.matches(ArtifactKind::Archive));
        assert!(f.matches(ArtifactKind::Makeself));
        assert!(!f.matches(ArtifactKind::SourceArchive));
    }

    #[test]
    fn filter_binary_covers_three_binary_kinds() {
        let f = BeforePublishArtifactFilter::Binary;
        assert!(f.matches(ArtifactKind::Binary));
        assert!(f.matches(ArtifactKind::UploadableBinary));
        assert!(f.matches(ArtifactKind::UniversalBinary));
        assert!(!f.matches(ArtifactKind::Library));
    }

    #[test]
    fn filter_image_excludes_multiarch_manifest() {
        let f = BeforePublishArtifactFilter::Image;
        assert!(f.matches(ArtifactKind::DockerImage));
        assert!(f.matches(ArtifactKind::DockerImageV2));
        assert!(f.matches(ArtifactKind::PublishableDockerImage));
        // multi-arch index entries are not scannable image blobs
        assert!(!f.matches(ArtifactKind::DockerManifest));
        assert!(!f.matches(ArtifactKind::DockerDigest));
    }

    #[test]
    fn filter_narrow_variants_match_only_themselves() {
        assert!(BeforePublishArtifactFilter::Checksum.matches(ArtifactKind::Checksum));
        assert!(!BeforePublishArtifactFilter::Checksum.matches(ArtifactKind::Sbom));
        assert!(BeforePublishArtifactFilter::DiskImage.matches(ArtifactKind::DiskImage));
        assert!(!BeforePublishArtifactFilter::DiskImage.matches(ArtifactKind::Installer));
        assert!(BeforePublishArtifactFilter::Sbom.matches(ArtifactKind::Sbom));
        assert!(!BeforePublishArtifactFilter::Sbom.matches(ArtifactKind::Checksum));
    }

    #[test]
    fn filter_deserializes_snake_case_and_diskimage_alias() {
        let f: BeforePublishArtifactFilter = serde_yaml_ng::from_str("disk_image").unwrap();
        assert_eq!(f, BeforePublishArtifactFilter::DiskImage);
        // legacy single-word `diskimage` alias parses to the same variant
        let aliased: BeforePublishArtifactFilter = serde_yaml_ng::from_str("diskimage").unwrap();
        assert_eq!(aliased, BeforePublishArtifactFilter::DiskImage);
    }

    #[test]
    fn hook_entry_string_deserializes_as_simple() {
        let h: HookEntry = serde_yaml_ng::from_str("\"echo hi\"").unwrap();
        assert!(matches!(h, HookEntry::Simple(ref s) if s == "echo hi"));
    }

    #[test]
    fn hook_entry_object_deserializes_as_structured() {
        let h: HookEntry = serde_yaml_ng::from_str("cmd: build.sh\ndir: subdir").unwrap();
        match h {
            HookEntry::Structured(s) => {
                assert_eq!(s.cmd, "build.sh");
                assert_eq!(s.dir.as_deref(), Some("subdir"));
            }
            HookEntry::Simple(_) => panic!("expected structured hook"),
        }
    }

    #[test]
    fn hook_entry_rejects_non_string_non_object() {
        // a bare list is neither a command string nor a structured hook
        let err = serde_yaml_ng::from_str::<HookEntry>("- a\n- b");
        assert!(err.is_err());
    }

    #[test]
    fn hook_entry_if_alias_maps_to_if_condition() {
        let h: HookEntry = serde_yaml_ng::from_str("cmd: x\nif: \"{{ .IsSnapshot }}\"").unwrap();
        match h {
            HookEntry::Structured(s) => {
                assert_eq!(s.if_condition.as_deref(), Some("{{ .IsSnapshot }}"));
            }
            HookEntry::Simple(_) => panic!("expected structured hook"),
        }
    }

    #[test]
    fn hook_entry_partial_eq_str_matches_both_variants() {
        assert!(HookEntry::Simple("go test".to_string()) == "go test");
        assert!(HookEntry::Simple("go test".to_string()) != "go vet");
        let structured = HookEntry::Structured(StructuredHook {
            cmd: "make lint".to_string(),
            ..Default::default()
        });
        assert!(structured == "make lint");
        assert!(structured != "make build");
    }

    #[test]
    fn deserialize_then_serialize_drops_post_field() {
        // post-only input folds into hooks: and serialization shows no post:
        let cfg: HooksConfig = serde_yaml_ng::from_str("post:\n  - legacy.sh").unwrap();
        assert!(cfg.post.is_none());
        let out = serde_yaml_ng::to_string(&cfg).unwrap();
        assert!(out.contains("hooks"), "serialized: {out}");
        assert!(
            !out.contains("post"),
            "serialized must not carry post: {out}"
        );
    }

    #[test]
    fn empty_block_neither_spelling_stays_empty_and_silent() {
        // (false, false) arm: empty block, nothing folds and nothing warns.
        let captured = capture_warnings(|| {
            let mut cfg = HooksConfig {
                hooks: None,
                post: None,
            };
            cfg.merge_hook_aliases();
            assert!(cfg.hooks.is_none());
            assert!(cfg.post.is_none());
        });
        assert!(
            !captured.contains("DEPRECATION"),
            "empty block must not warn: {captured}"
        );
    }

    #[test]
    fn empty_post_vec_does_not_trigger_fold_or_warn() {
        // is_some_and(|v| !v.is_empty()) means an empty post vec is treated as
        // absent — it must not fold into hooks nor warn.
        let captured = capture_warnings(|| {
            let mut cfg = HooksConfig {
                hooks: None,
                post: Some(vec![]),
            };
            cfg.merge_hook_aliases();
            // empty post was NOT folded; it stays as the (now still-empty) post
            assert!(cfg.hooks.is_none(), "empty post must not become hooks");
        });
        assert!(
            !captured.contains("DEPRECATION"),
            "empty post must not warn: {captured}"
        );
    }

    #[test]
    fn default_hooks_config_is_all_none() {
        let cfg = HooksConfig::default();
        assert!(cfg.hooks.is_none());
        assert!(cfg.post.is_none());
    }

    #[test]
    fn raw_deserialize_rejects_unknown_key() {
        let err = serde_yaml_ng::from_str::<HooksConfig>("befor:\n  - x");
        assert!(err.is_err(), "deny_unknown_fields on Raw must reject typos");
    }

    #[test]
    fn structured_hook_full_fields_parse() {
        let h: HookEntry = serde_yaml_ng::from_str(
            "cmd: build.sh\nenv: [FOO=1, BAR=2]\noutput: true\nids: [linux-bin]\nartifacts: binary\n",
        )
        .unwrap();
        match h {
            HookEntry::Structured(s) => {
                assert_eq!(s.cmd, "build.sh");
                assert_eq!(
                    s.env.as_deref(),
                    Some(&["FOO=1".to_string(), "BAR=2".to_string()][..])
                );
                assert_eq!(s.output, Some(true));
                assert_eq!(s.ids.as_deref(), Some(&["linux-bin".to_string()][..]));
                assert_eq!(s.artifacts, Some(BeforePublishArtifactFilter::Binary));
            }
            HookEntry::Simple(_) => panic!("expected structured hook"),
        }
    }

    #[test]
    fn structured_hook_rejects_unknown_field() {
        // StructuredHook is deny_unknown_fields; deserialize routes objects here.
        let err = serde_yaml_ng::from_str::<HookEntry>("cmd: x\nbogus: 1");
        assert!(
            err.is_err(),
            "unknown structured-hook field must be rejected"
        );
    }

    #[test]
    fn hook_entry_simple_serializes_untagged_as_bare_string() {
        let h = HookEntry::Simple("echo hi".to_string());
        let out = serde_yaml_ng::to_string(&h).unwrap();
        // untagged: a Simple serializes to a bare scalar, not a tagged map
        assert_eq!(out.trim(), "echo hi");
    }

    #[test]
    fn hook_entry_structured_serializes_to_mapping() {
        let h = HookEntry::Structured(StructuredHook {
            cmd: "make".to_string(),
            output: Some(true),
            ..Default::default()
        });
        let out = serde_yaml_ng::to_string(&h).unwrap();
        assert!(out.contains("cmd: make"), "serialized: {out}");
        assert!(out.contains("output: true"), "serialized: {out}");
    }

    #[test]
    fn filter_installer_distinct_from_package_and_diskimage() {
        // Installer must NOT swallow package/diskimage kinds — guards the
        // most error-prone overlap in the matches() table.
        let f = BeforePublishArtifactFilter::Installer;
        assert!(!f.matches(ArtifactKind::LinuxPackage));
        assert!(!f.matches(ArtifactKind::DiskImage));
        assert!(!f.matches(ArtifactKind::Archive));
    }
}
