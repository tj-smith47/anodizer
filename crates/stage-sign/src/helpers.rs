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

/// The complete set of recognized `signs[].artifacts` / `docker_signs[].artifacts`
/// filter strings, in match-arm order.
///
/// This is the single source of truth shared between the runtime resolver
/// (`should_sign_artifact`) and the config-time validator
/// (`anodizer check config`). The `valid_filters_match_resolver` drift-guard
/// test asserts every entry here is accepted by `should_sign_artifact` and
/// that the resolver rejects anything not listed, so the two cannot diverge.
pub const VALID_SIGN_ARTIFACT_FILTERS: &[&str] = &[
    "none",
    "all",
    "any",
    "source",
    "archive",
    "binary",
    "package",
    "installer",
    "diskimage",
    "sbom",
    "snap",
    "macos_package",
    "checksum",
    "windows",
];

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
/// - `"binary"`        → `ArtifactKind::Binary` and
///   `ArtifactKind::UniversalBinary`, so a lipo-merged macOS universal binary
///   is signed under the default filter. `ids` still discriminates, and a
///   universal binary is selected by its own `universal_binaries.id`, not by
///   the source build IDs it was merged from
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
        "binary" => Ok(matches!(
            kind,
            ArtifactKind::Binary | ArtifactKind::UniversalBinary
        )),
        "package" => Ok(kind == ArtifactKind::LinuxPackage),
        "installer" => Ok(kind == ArtifactKind::Installer),
        "diskimage" => Ok(kind == ArtifactKind::DiskImage),
        "sbom" => Ok(kind == ArtifactKind::Sbom),
        "snap" => Ok(kind == ArtifactKind::Snap),
        "macos_package" => Ok(kind == ArtifactKind::MacOsPackage),
        "checksum" => Ok(kind == ArtifactKind::Checksum),
        // The "windows" selector (Authenticode backend) is a two-step filter:
        // the kind pre-filter here admits the container kinds that a Windows
        // PE/MSI/DLL can end up in (.exe → Binary, .msi/NSIS → Installer,
        // .dll → Library), and the EXTENSION refinement happens in
        // `process_sign_configs` where the artifact path is in scope. The kind
        // alone is insufficient (a Linux ELF is also Binary), so this MUST be
        // paired with `windows_artifact_extension_matches` on the path.
        "windows" => Ok(matches!(
            kind,
            ArtifactKind::Binary | ArtifactKind::Installer | ArtifactKind::Library
        )),
        other => anyhow::bail!("invalid sign artifacts filter: {other}"),
    }
}

/// File extensions an Authenticode (`artifacts: windows`) signer can sign.
///
/// The `"windows"` kind pre-filter (`should_sign_artifact`) admits
/// Binary/Installer/Library — but a Linux ELF is also `Binary`, so the
/// final say is this extension check against the artifact's on-disk path.
pub(crate) const WINDOWS_SIGNABLE_EXTENSIONS: &[&str] = &["exe", "msi", "dll"];

/// `true` when `path` ends in a Windows-signable extension
/// (`.exe`, `.msi`, `.dll`, case-insensitive).
///
/// Paired with the `"windows"` kind pre-filter so the Authenticode backend
/// signs a Windows `.exe` Binary but skips a Linux ELF Binary that shares the
/// `ArtifactKind::Binary` kind but carries no signable extension.
pub(crate) fn windows_artifact_extension_matches(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|ext| {
            let lower = ext.to_ascii_lowercase();
            WINDOWS_SIGNABLE_EXTENSIONS.contains(&lower.as_str())
        })
        .unwrap_or(false)
}

/// Build the derived signer argv for an Authenticode sign job.
///
/// Pure function — no I/O, no `Context` — so the exact argv is unit-testable.
/// Two shapes depending on `tool`:
///
/// - **osslsigncode** (Linux/cross): `sign -pkcs12 <cert> [-pass <pw>]
///   [-n <name>] [-i <url>] -ts <ts_url> -in <artifact> -out <out_tmp>`.
///   osslsigncode requires distinct `-in`/`-out`, so the caller writes to a
///   sibling temp then atomically renames it over the original on success.
/// - **signtool** (Windows-native): `sign /f <cert> [/p <pw>] /fd sha256
///   /tr <ts_url> /td sha256 [/d <name>] [/du <url>] <artifact>` — signs in
///   place, so `out_tmp` is unused.
///
/// `password` is passed in argv (`-pass` / `/p`); the caller is responsible
/// for redacting it from any echoed command line and spawn output.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_authenticode_argv(
    tool: &str,
    cert_path: &str,
    password: Option<&str>,
    timestamp_url: &str,
    name: Option<&str>,
    url: Option<&str>,
    artifact_path: &str,
    out_tmp: &str,
) -> Vec<String> {
    let tool_base = std::path::Path::new(tool)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(tool);
    let mut args: Vec<String> = Vec::new();
    if tool_base.starts_with("signtool") {
        args.push("sign".to_string());
        args.push("/f".to_string());
        args.push(cert_path.to_string());
        if let Some(pw) = password {
            args.push("/p".to_string());
            args.push(pw.to_string());
        }
        args.push("/fd".to_string());
        args.push("sha256".to_string());
        args.push("/tr".to_string());
        args.push(timestamp_url.to_string());
        args.push("/td".to_string());
        args.push("sha256".to_string());
        if let Some(n) = name {
            args.push("/d".to_string());
            args.push(n.to_string());
        }
        if let Some(u) = url {
            args.push("/du".to_string());
            args.push(u.to_string());
        }
        args.push(artifact_path.to_string());
    } else {
        // osslsigncode (the cross/Linux default and any other tool name).
        args.push("sign".to_string());
        args.push("-pkcs12".to_string());
        args.push(cert_path.to_string());
        if let Some(pw) = password {
            args.push("-pass".to_string());
            args.push(pw.to_string());
        }
        if let Some(n) = name {
            args.push("-n".to_string());
            args.push(n.to_string());
        }
        if let Some(u) = url {
            args.push("-i".to_string());
            args.push(u.to_string());
        }
        args.push("-ts".to_string());
        args.push(timestamp_url.to_string());
        args.push("-in".to_string());
        args.push(artifact_path.to_string());
        args.push("-out".to_string());
        args.push(out_tmp.to_string());
    }
    args
}

/// Render an Authenticode signer argv to a single space-joined echo line with
/// the cert-password SLOT masked as `***`.
///
/// Masks at the argv-element level: the token immediately following a `-pass`
/// (osslsigncode) or `/p` (signtool) flag is replaced with `***`. A blind
/// `str::replace(password, "***")` would corrupt unrelated tokens whenever the
/// password is a common substring (e.g. a password of `sign` would mangle the
/// `sign` subcommand), so the slot-based approach is both exact and password-
/// value-independent.
///
/// Used by the dry-run `(dry-run) would run:` echo so the password never arrives
/// in logs verbatim. The spawn path relies on `redact::string` (fed the
/// password via `SignJob::redact_extra`) instead; this helper covers the
/// dry-run path where no process runs.
pub(crate) fn redact_password_in_argv(args: &[String]) -> String {
    let mut out: Vec<String> = Vec::with_capacity(args.len());
    let mut mask_next = false;
    for arg in args {
        if mask_next {
            out.push("***".to_string());
            mask_next = false;
            continue;
        }
        out.push(arg.clone());
        if arg == "-pass" || arg == "/p" {
            mask_next = true;
        }
    }
    out.join(" ")
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
/// for binary_signs (also `{{ .Artifact }}.sig` — anodizer's flat dist layout
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
/// The on-disk location a rendered `signature:` / `certificate:` template
/// names. A rendering already under `dist` is kept; any other one is placed
/// under `dist`, the same rule GoReleaser's `relativeToDist` applies. The two
/// are compared as absolute paths so `./dist/x` and `dist/x` agree.
///
/// The signer receives this path AND the stage registers it, so the two can
/// never disagree: a template that rendered outside `dist` used to be handed
/// to the signer unjoined while the artifact was registered joined, leaving a
/// registered signature that pointed at nothing.
pub(crate) fn dist_joined(dist: &std::path::Path, rendered: &str) -> std::path::PathBuf {
    let resolved = std::path::PathBuf::from(rendered);
    let under_dist = match (std::path::absolute(&resolved), std::path::absolute(dist)) {
        (Ok(abs), Ok(abs_dist)) => abs.starts_with(abs_dist),
        _ => resolved.starts_with(dist),
    };
    if under_dist {
        resolved
    } else {
        dist.join(resolved)
    }
}

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

/// Pin a Docker image reference to its content digest, producing the
/// `<repo>:<tag>@sha256:<digest>` form cosign must sign.
///
/// Signing a bare `<repo>:<tag>` is a TOCTOU integrity hole: the tag can
/// move between build and sign, so a tag-signature may certify a different
/// image than the one anodizer built (cosign warns and is removing tag
/// signing). The build stage records the digest it produced; signing the
/// digest-pinned reference certifies exactly that image.
///
/// Idempotent: any existing `@sha256:...` suffix on `image_ref` is stripped
/// before re-pinning, so feeding an already-pinned reference (or a user
/// `args` template that also appends `@{{ .Digest }}`) cannot produce a
/// doubled `@sha256:...@sha256:...` token. When `digest` is empty (no
/// digest was captured) the reference is returned unpinned — the caller
/// warns rather than silently dropping the signature.
pub(crate) fn pin_image_ref_to_digest(image_ref: &str, digest: &str) -> String {
    let base = strip_digest_suffix(image_ref);
    if digest.is_empty() {
        base.to_string()
    } else {
        format!("{base}@{digest}")
    }
}

/// Strip a trailing `@sha256:<hex>` (or any `@<alg>:<value>`) digest suffix
/// from an image reference, returning the bare `<repo>:<tag>` (or `<repo>`)
/// part. A reference with no digest suffix is returned unchanged.
fn strip_digest_suffix(image_ref: &str) -> &str {
    // A digest suffix is the final `@`-delimited segment containing a `:`
    // (the `<algorithm>:<hex>` shape). Splitting on the LAST `@` is safe
    // because a registry/repo/tag cannot contain `@` — only the digest
    // delimiter does.
    match image_ref.rsplit_once('@') {
        Some((base, suffix)) if suffix.contains(':') => base,
        _ => image_ref,
    }
}

/// Collapse a doubled trailing image digest (`@<alg>:<v>@<alg>:<v>`, the two
/// halves identical) down to a single pin.
///
/// `{{ .Artifact }}` now resolves to the already-digest-pinned reference, so
/// an `args` template that ALSO appends `@{{ .Digest }}` (the historical
/// default, and a natural hand-written value) would otherwise produce
/// `<repo>:<tag>@sha256:X@sha256:X`. This keeps exactly one pin so the
/// resulting cosign reference is valid for both args shapes. Only collapses
/// when the two digest segments are byte-identical — a genuinely different
/// trailing token is left untouched.
pub(crate) fn collapse_doubled_digest(arg: &str) -> String {
    if let Some((head, last)) = arg.rsplit_once('@')
        && last.contains(':')
        && let Some((base, prev)) = head.rsplit_once('@')
        && prev == last
    {
        return format!("{base}@{last}");
    }
    arg.to_string()
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

/// Append a target triple to a basename while keeping its extension
/// suffix: `anodizer.sig` + `aarch64-apple-darwin` →
/// `anodizer-aarch64-apple-darwin.sig`, `anodizer.exe.sig` →
/// `anodizer.exe-aarch64-pc-windows-msvc.sig`. A basename with no
/// extension gets a plain `-<target>` suffix.
pub(crate) fn qualify_basename_with_target(name: &str, target: &str) -> String {
    let path = std::path::Path::new(name);
    match (
        path.file_stem().and_then(|s| s.to_str()),
        path.extension().and_then(|e| e.to_str()),
    ) {
        (Some(stem), Some(ext)) => format!("{stem}-{target}.{ext}"),
        _ => format!("{name}-{target}"),
    }
}

/// The asset-name stem the run's other assets for `crate_name`/`target` are
/// built from: the archive stage's rendered `name_template` for that target,
/// read back from the archive it registered
/// (`anodizer-0.26.0-linux-amd64.tar.gz` → `anodizer-0.26.0-linux-amd64`).
///
/// A crate with several `archives:` entries registers several archives per
/// target, and the registry's order is not the config's: a publish-only run
/// loads the preserved manifest, where `app-1.0.0-linux-amd64-extra.tar.xz`
/// sorts ahead of `app-1.0.0-linux-amd64.tar.gz`. The stem is the PRIMARY
/// entry's — the first `archives:` entry in config order, the one chocolatey
/// and scoop bind to with `ids: [default]` — so a signature is named after the
/// archive its consumers install from, whatever order the archives were
/// registered in.
///
/// A `formats: [binary]` entry registers no archive, so the uploadable binary
/// named after `binary_name` stands in — its own name IS the stem, which is
/// what makes a `signs:` signature over that asset and a `binary_signs:`
/// signature over the same bytes resolve to one name.
///
/// `None` when the run produced neither, which is `anodizer build`: it signs
/// binaries before any archive exists and uploads nothing, so the caller keeps
/// the target-qualified basename there.
pub(crate) fn archive_stem_for(
    ctx: &Context,
    crate_name: &str,
    target: &str,
    binary_name: Option<&str>,
) -> Option<String> {
    let for_target = |a: &&anodizer_core::artifact::Artifact| {
        a.crate_name == crate_name && a.target.as_deref() == Some(target)
    };
    let artifacts = ctx.artifacts.all();
    let archives: Vec<_> = artifacts
        .iter()
        .filter(|a| a.kind == ArtifactKind::Archive)
        .filter(for_target)
        .collect();
    let primary_id = primary_archive_id(ctx, crate_name);
    let primary = archives
        .iter()
        .find(|a| a.metadata.get("id") == primary_id.as_ref())
        .or_else(|| archives.first());
    if let Some(archive) = primary
        && let Some(stem) = archive.metadata.get("name")
        && !stem.is_empty()
    {
        return Some(stem.clone());
    }
    artifacts
        .iter()
        .filter(|a| a.kind == ArtifactKind::UploadableBinary)
        .find(|a| for_target(a) && a.binary_name().as_deref() == binary_name)
        .map(|a| a.name.clone())
        .filter(|n| !n.is_empty())
}

/// The id of `crate_name`'s first configured `archives:` entry, the archive
/// the crate's other per-target assets are named after. The archive stage
/// records the same id in each archive's `id` metadata (`default` when the
/// entry names none). `None` when the crate is not in the config or has no
/// archive entry.
fn primary_archive_id(ctx: &Context, crate_name: &str) -> Option<String> {
    match &ctx.config.find_crate(crate_name)?.archives {
        anodizer_core::config::ArchivesConfig::Configs(configs) => configs
            .first()
            .map(|c| c.id.clone().unwrap_or_else(|| "default".to_string())),
        anodizer_core::config::ArchivesConfig::Disabled => None,
    }
}

/// The release-asset name a `binary_signs:` output registers under.
///
/// A binary signature uploads beside the archive built from the same binary,
/// so it is named after that archive's stem: the raw binary's own basename
/// repeats across every target (`anodizer.sig` eight times over). The suffix
/// the `signature:` / `certificate:` template appended to the binary's file
/// name carries over, so `anodizer.exe` → `anodizer.exe.sig` yields
/// `<stem>.sig` and a cosign bundle `anodizer.bundle.sig` yields
/// `<stem>.bundle.sig`.
///
/// Falls back to the target-qualified basename when the run built no archive
/// for the target, or when the template renamed the file rather than suffixing
/// it — both still unique per target.
pub(crate) fn binary_sign_asset_name(
    rendered_basename: &str,
    binary_basename: &str,
    stem: Option<&str>,
    target: &str,
) -> String {
    if binary_basename.is_empty() {
        return qualify_basename_with_target(rendered_basename, target);
    }
    match (stem, rendered_basename.strip_prefix(binary_basename)) {
        (Some(stem), Some(suffix)) if !suffix.is_empty() => format!("{stem}{suffix}"),
        _ => qualify_basename_with_target(rendered_basename, target),
    }
}

#[cfg(test)]
mod archive_stem_for_tests {
    use super::archive_stem_for;
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig, CrateConfig};
    use anodizer_core::context::Context;
    use anodizer_core::test_helpers::TestContextBuilder;

    const TARGET: &str = "x86_64-unknown-linux-gnu";

    fn crate_with_archives(ids: &[&str]) -> CrateConfig {
        CrateConfig {
            name: "app".to_string(),
            path: ".".to_string(),
            archives: ArchivesConfig::Configs(
                ids.iter()
                    .map(|id| ArchiveConfig {
                        id: Some((*id).to_string()),
                        ..Default::default()
                    })
                    .collect(),
            ),
            ..Default::default()
        }
    }

    fn add_archive(ctx: &mut Context, id: &str, stem: &str, file: &str) {
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: std::path::PathBuf::from(format!("dist/{file}")),
            target: Some(TARGET.to_string()),
            crate_name: "app".to_string(),
            metadata: [("id", id), ("name", stem)]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            size: None,
        });
    }

    /// The preserved manifest of a publish-only run lists the `-extra`
    /// archive first (`-` sorts before `.`); the stem is still the primary
    /// entry's.
    #[test]
    fn the_primary_archive_entry_names_the_stem_whatever_the_registry_order() {
        let mut ctx = TestContextBuilder::new()
            .crates(vec![crate_with_archives(&["default", "extra"])])
            .build();
        add_archive(
            &mut ctx,
            "extra",
            "app-1.0.0-linux-amd64-extra",
            "app-1.0.0-linux-amd64-extra.tar.xz",
        );
        add_archive(
            &mut ctx,
            "default",
            "app-1.0.0-linux-amd64",
            "app-1.0.0-linux-amd64.tar.gz",
        );
        assert_eq!(
            archive_stem_for(&ctx, "app", TARGET, Some("app")).as_deref(),
            Some("app-1.0.0-linux-amd64")
        );
    }

    /// An entry without an `id:` is registered as `default`, which is also
    /// what the config fold writes back, so the two spellings agree.
    #[test]
    fn an_unnamed_first_entry_matches_the_default_id() {
        let mut ctx = TestContextBuilder::new()
            .crates(vec![CrateConfig {
                archives: ArchivesConfig::Configs(vec![
                    ArchiveConfig::default(),
                    ArchiveConfig {
                        id: Some("extra".to_string()),
                        ..Default::default()
                    },
                ]),
                ..crate_with_archives(&[])
            }])
            .build();
        add_archive(&mut ctx, "extra", "app-extra", "app-extra.tar.xz");
        add_archive(&mut ctx, "default", "app-main", "app-main.tar.gz");
        assert_eq!(
            archive_stem_for(&ctx, "app", TARGET, Some("app")).as_deref(),
            Some("app-main")
        );
    }

    /// A crate the config does not describe (or whose primary entry produced
    /// no archive for this target) still resolves: the first registered
    /// archive stands in rather than dropping to the target-qualified name.
    #[test]
    fn without_a_primary_match_the_first_registered_archive_stands_in() {
        let mut ctx = TestContextBuilder::new().build();
        add_archive(&mut ctx, "extra", "app-extra", "app-extra.tar.xz");
        assert_eq!(
            archive_stem_for(&ctx, "app", TARGET, Some("app")).as_deref(),
            Some("app-extra")
        );
        let mut ctx = TestContextBuilder::new()
            .crates(vec![crate_with_archives(&["default", "extra"])])
            .build();
        add_archive(&mut ctx, "extra", "app-extra", "app-extra.tar.xz");
        assert_eq!(
            archive_stem_for(&ctx, "app", TARGET, Some("app")).as_deref(),
            Some("app-extra")
        );
    }

    /// A target no archive entry covers (a build that only feeds npm or
    /// docker) has no stem, so the caller keeps the target-qualified name.
    #[test]
    fn a_target_with_no_archive_has_no_stem() {
        let ctx = TestContextBuilder::new()
            .crates(vec![crate_with_archives(&["default"])])
            .build();
        assert_eq!(archive_stem_for(&ctx, "app", TARGET, Some("app")), None);
    }
}

#[cfg(test)]
mod binary_sign_asset_name_tests {
    use super::binary_sign_asset_name;

    const TARGET: &str = "x86_64-pc-windows-msvc";

    #[test]
    fn the_suffix_after_the_binary_name_carries_onto_the_archive_stem() {
        for (rendered, binary, expected) in [
            ("app.sig", "app", "app-1.0.0-windows-amd64.sig"),
            ("app.exe.sig", "app.exe", "app-1.0.0-windows-amd64.sig"),
            (
                "app.bundle.sig",
                "app",
                "app-1.0.0-windows-amd64.bundle.sig",
            ),
            ("app.pem", "app", "app-1.0.0-windows-amd64.pem"),
        ] {
            assert_eq!(
                binary_sign_asset_name(rendered, binary, Some("app-1.0.0-windows-amd64"), TARGET),
                expected,
                "{rendered} over {binary}"
            );
        }
    }

    #[test]
    fn no_stem_or_no_suffix_falls_back_to_the_target_qualified_name() {
        // `anodizer build` signs before any archive exists.
        assert_eq!(
            binary_sign_asset_name("app.sig", "app", None, TARGET),
            format!("app-{TARGET}.sig")
        );
        // A `signature:` template that renamed the file rather than suffixing
        // the binary's own name.
        assert_eq!(
            binary_sign_asset_name(
                "detached.sig",
                "app",
                Some("app-1.0.0-windows-amd64"),
                TARGET
            ),
            format!("detached-{TARGET}.sig")
        );
    }
}

#[cfg(test)]
mod dist_joined_tests {
    use super::dist_joined;
    use std::path::{Path, PathBuf};

    #[test]
    fn a_relative_rendering_outside_dist_is_placed_under_dist() {
        assert_eq!(
            dist_joined(Path::new("./dist"), ".det-tmp/target/x/release/app.sig"),
            PathBuf::from("./dist/.det-tmp/target/x/release/app.sig")
        );
    }

    #[test]
    fn a_rendering_under_dist_is_kept_whatever_its_spelling() {
        assert_eq!(
            dist_joined(Path::new("./dist"), "dist/app.tar.gz.sig"),
            PathBuf::from("dist/app.tar.gz.sig")
        );
        assert_eq!(
            dist_joined(Path::new("dist"), "./dist/app.tar.gz.sig"),
            PathBuf::from("./dist/app.tar.gz.sig")
        );
    }

    #[test]
    fn an_absolute_rendering_stays_put() {
        let abs = std::env::temp_dir().join("app.sig");
        assert_eq!(
            dist_joined(Path::new("./dist"), &abs.to_string_lossy()),
            abs
        );
    }
}

#[cfg(test)]
mod filter_drift_tests {
    use super::*;

    /// The published `VALID_SIGN_ARTIFACT_FILTERS` constant and the runtime
    /// `should_sign_artifact` resolver must stay in lockstep: every listed
    /// value resolves without error, and any value NOT listed is rejected.
    /// This is what lets `anodizer check config` validate against the same
    /// vocabulary the sign stage actually honors.
    #[test]
    fn valid_filters_match_resolver() {
        // Any kind works as the probe — `should_sign_artifact` only errors on
        // an unrecognized filter, never on the kind.
        let probe = ArtifactKind::Archive;

        for filter in VALID_SIGN_ARTIFACT_FILTERS {
            assert!(
                should_sign_artifact(probe, filter).is_ok(),
                "filter '{filter}' is listed in VALID_SIGN_ARTIFACT_FILTERS but \
                 the resolver rejects it",
            );
        }

        // Values not in the list must be rejected — proves the const is the
        // complete accepted set, not merely a subset.
        for bogus in ["", "bogus", "All", "ANY", "sboms", "macos-package"] {
            assert!(
                should_sign_artifact(probe, bogus).is_err(),
                "filter '{bogus}' is not listed but the resolver accepted it",
            );
        }
    }
}
