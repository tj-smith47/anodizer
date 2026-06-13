//! Pure helpers for sign-stage decisions: artifact-kind filter resolution,
//! signature-path templating, stdin plumbing, default-cmd discovery, and
//! shell-style variable expansion. Lifted out of the SignStage monolith
//! so the per-decision logic is independently reviewable.

use std::collections::HashMap;
use std::process::Stdio;

use anyhow::{Context as _, Result};

use anodizer_core::artifact::ArtifactKind;
use anodizer_core::config::SignConfig;
use anodizer_core::context::Context;
use anodizer_core::env_expand::expand_with_preserve;

/// Returns `true` if an artifact of `kind` should be signed given the `filter`
/// string from `SignConfig::artifacts` / `DockerSignConfig::artifacts`.
///
/// Filter values:
/// - `"none"`          → nothing is signed
/// - `"all"` / `"any"` → the PRIMARY subject kinds
///   (`signable_subject_kinds()`: Archive, UploadableBinary, SourceArchive,
///   UploadableFile, Makeself, AppImage, LinuxPackage, Flatpak, SourceRpm,
///   Installer, DiskImage, MacOsPackage, Sbom) PLUS every `Checksum`
///   (combined `checksums.txt` AND per-artifact split `.sha256` sidecars) —
///   GoReleaser's `sign.artifacts: all` is `ReleaseUploadableTypes()` minus
///   only `Signature, Certificate`, and `Checksum` is in that set
///   (`internal/pipe/sign/sign.go:103-108`). `Signature` and `Certificate`
///   ARE excluded: signing a signature is never valid. Signing a checksum
///   yields one legitimate `X.sha256.sig` and CANNOT recurse — see below.
/// - `"source"`        → only `ArtifactKind::SourceArchive`
/// - `"archive"`       → only `ArtifactKind::Archive`
/// - `"binary"`        → only `ArtifactKind::Binary`
/// - `"package"`       → only `ArtifactKind::LinuxPackage`
/// - `"installer"`     → only `ArtifactKind::Installer`
/// - `"diskimage"`     → only `ArtifactKind::DiskImage`
/// - `"sbom"`          → only `ArtifactKind::Sbom`
/// - `"snap"`          → only `ArtifactKind::Snap`
/// - `"macos_package"` → only `ArtifactKind::MacOsPackage`
/// - `"checksum"`      → every `ArtifactKind::Checksum`, combined file and
///   per-artifact split sidecars alike (GoReleaser:
///   `artifact.ByType(artifact.Checksum)`,
///   `internal/pipe/sign/sign.go:93-94`). Each yields one `X.sha256.sig`.
///
/// ## Why this cannot recurse into `X.sha256.sig.sha256`
///
/// Signing a checksum produces a `Signature` (`X.sha256.sig`). The
/// anti-recursion guard is NOT in this filter — it is upstream, mirroring
/// GoReleaser's `refreshAll` `Not(Checksum, Signature, Certificate)`
/// (`internal/pipe/checksums/checksums.go:189-190`). Two upstream facts close
/// the loop: the checksum stage's subject set is `checksummable_subject_kinds()`
/// (PRIMARY only — it never hashes a `.sig` or a `.sha256`), and
/// `refresh_combined_checksums` skips every `is_derived_sidecar_kind`. So a
/// freshly-produced `X.sha256.sig` is never re-checksummed (no third level
/// forms) and never re-signed (`Signature` is excluded here). The legit second
/// level `X.sha256.sig` is GoReleaser-parity; the third level is
/// unrepresentable.
///
/// Any other value returns an error.
pub(crate) fn should_sign_artifact(kind: ArtifactKind, filter: &str) -> Result<bool> {
    match filter {
        "none" => Ok(false),
        "all" | "any" => Ok(
            anodizer_core::artifact::signable_subject_kinds().contains(&kind)
                || kind == ArtifactKind::Checksum,
        ),
        "source" => Ok(kind == ArtifactKind::SourceArchive),
        "archive" => Ok(kind == ArtifactKind::Archive),
        "binary" => Ok(kind == ArtifactKind::Binary),
        "package" => Ok(kind == ArtifactKind::LinuxPackage),
        "installer" => Ok(kind == ArtifactKind::Installer),
        "diskimage" => Ok(kind == ArtifactKind::DiskImage),
        "sbom" => Ok(kind == ArtifactKind::Sbom),
        "snap" => Ok(kind == ArtifactKind::Snap),
        "macos_package" => Ok(kind == ArtifactKind::MacOsPackage),
        "checksum" => Ok(kind == ArtifactKind::Checksum),
        other => anyhow::bail!("invalid sign artifacts filter: {other}"),
    }
}

/// Validate a sign-config list's ids: unique, and never colliding with the
/// positional fallback labels (`sign[0]`, `binary-sign[1]`, …) used when
/// `id:` is unset. Skip records and the expected-asset derivation key
/// configs by `id`-or-fallback-label, so an explicit id matching the
/// fallback pattern of its own list could alias another config's skip
/// record; rejecting it up front makes the collision unrepresentable.
///
/// `label` is the stage label embedded in the fallback (`sign` /
/// `binary-sign`); `list_name` is the config key for error messages
/// (`signs` / `binary_signs`).
pub(crate) fn validate_sign_config_ids(
    configs: &[SignConfig],
    label: &str,
    list_name: &str,
) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for cfg in configs {
        let id = cfg.resolved_id();
        if !seen.insert(id.to_string()) {
            anyhow::bail!("found 2 {} with the ID '{}'", list_name, id);
        }
        if cfg
            .id
            .as_deref()
            .is_some_and(|id| is_fallback_label(id, label))
        {
            anyhow::bail!(
                "{} config id '{}' matches the reserved positional label pattern \
                 '{}[N]' (used internally for configs without an id); choose a \
                 different id",
                list_name,
                id,
                label
            );
        }
    }
    Ok(())
}

/// `true` when `id` has the exact shape of a positional fallback label for
/// `label`: `<label>[<digits>]`.
fn is_fallback_label(id: &str, label: &str) -> bool {
    id.strip_prefix(label)
        .and_then(|rest| rest.strip_prefix('['))
        .and_then(|rest| rest.strip_suffix(']'))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Returns `true` when an artifact passes a sign config's `ids:` filter.
///
/// The sign-stage `ids:` semantic matches either the artifact's `id` metadata
/// (its build id) or its `name` metadata; an absent filter matches everything.
/// Shared by the execution path (`process_sign_configs`) and the
/// expected-asset derivation so the two cannot diverge on which artifacts a
/// sign config selects.
pub(crate) fn sign_ids_match(
    metadata: &HashMap<String, String>,
    ids: Option<&Vec<String>>,
) -> bool {
    let Some(ids) = ids else { return true };
    let matches_id = metadata
        .get("id")
        .map(|id| ids.contains(id))
        .unwrap_or(false);
    let matches_name = metadata
        .get("name")
        .map(|name| ids.contains(name))
        .unwrap_or(false);
    matches_id || matches_name
}

/// Resolve the signature output path from a `SignConfig::signature` template,
/// falling back to `default_template`.
///
/// Caller passes `SignConfig::DEFAULT_SIGNATURE_TEMPLATE` for normal signs
/// (`{{ .Artifact }}.sig`) or `SignConfig::DEFAULT_BINARY_SIGNATURE_TEMPLATE`
/// for binary_signs (also `{{ .Artifact }}.sig` — anodize's flat dist layout
/// means binary names already carry the platform suffix; no duplication needed).
pub(crate) fn resolve_signature_path(
    sign_cfg: &SignConfig,
    artifact_path: &str,
    ctx: &Context,
    default_template: &str,
) -> Result<String> {
    let sig_template = sign_cfg.resolved_signature_template(default_template);
    let preprocessed = sig_template
        .replace("{{ .Artifact }}", artifact_path)
        .replace("{{ Artifact }}", artifact_path);
    ctx.render_template(&preprocessed).with_context(|| {
        format!(
            "sign: render signature template '{}' for artifact {}",
            sig_template, artifact_path
        )
    })
}

/// Pipe `stdin_content` or the contents of `stdin_file` to a child process's
/// stdin. Returns the appropriate `Stdio` and an optional content buffer.
///
/// Shared by both `SignConfig` and `DockerSignConfig` — both expose the same
/// `stdin` / `stdin_file` fields.
pub(crate) fn prepare_stdin_from(
    stdin: Option<&str>,
    stdin_file: Option<&str>,
    label: &str,
) -> Result<(Stdio, Option<Vec<u8>>)> {
    if let Some(content) = stdin {
        Ok((Stdio::piped(), Some(content.as_bytes().to_vec())))
    } else if let Some(path) = stdin_file {
        let data = std::fs::read(path)
            .with_context(|| format!("{}: failed to read stdin_file '{}'", label, path))?;
        Ok((Stdio::piped(), Some(data)))
    } else {
        Ok((Stdio::inherit(), None))
    }
}

/// Determine the default signing command by checking `git config gpg.program`
/// first, falling back to "gpg" if unset or unavailable. Cached for the
/// life of the process — `git config` is shelled out at most once.
pub(crate) fn default_sign_cmd() -> String {
    use std::sync::OnceLock;
    static CACHED: OnceLock<String> = OnceLock::new();
    CACHED
        .get_or_init(|| {
            if let Ok(output) = std::process::Command::new("git")
                .args(["config", "gpg.program"])
                .output()
            {
                let cmd = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !cmd.is_empty() {
                    return cmd;
                }
            }
            "gpg".to_string()
        })
        .clone()
}

/// Expand shell-style variable references (`$var` and `${var}`) in a string
/// against the signing-arg variable map.
///
/// Delegates to `anodizer_core::env_expand::expand_with_preserve` for
/// consistent `$VAR`/`${VAR}` parsing (shell-identifier rules). Unmatched
/// names are preserved literally so paths containing unrelated `$TOKEN`
/// values survive this pass unchanged.
pub(crate) fn expand_shell_vars(s: &str, vars: &HashMap<&str, &str>) -> String {
    expand_with_preserve(s, |name| vars.get(name).map(|v| (*v).to_string()))
}

/// Replace `{{ .Artifact }}`, `{{ .Signature }}`, and `{{ .Certificate }}`
/// placeholders in each arg.
pub(crate) fn resolve_sign_args(
    args: &[String],
    artifact_path: &str,
    signature_path: &str,
    certificate_path: Option<&str>,
) -> Vec<String> {
    args.iter()
        .map(|arg| {
            let mut resolved = arg
                .replace("{{ .Artifact }}", artifact_path)
                .replace("{{ Artifact }}", artifact_path)
                .replace("{{ .Signature }}", signature_path)
                .replace("{{ Signature }}", signature_path);
            // Replace certificate placeholder: with actual path if set, empty string otherwise.
            // This prevents `{{ .Certificate }}` from being fed to Tera and causing spurious warnings.
            let cert = certificate_path.unwrap_or("");
            resolved = resolved
                .replace("{{ .Certificate }}", cert)
                .replace("{{ Certificate }}", cert);
            resolved
        })
        .collect()
}
