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

/// The complete set of recognized `signs[].artifacts` /
/// `docker_signs[].artifacts` filter strings, in match-arm order.
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
/// the loop: the checksum stage's subject set is
/// `checksummable_subject_kinds()` (PRIMARY only — it never hashes a `.sig`
/// or a `.sha256`), and
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
/// Used by the dry-run `(dry-run) would run:` echo so the password never
/// arrives in logs verbatim. The spawn path relies on `redact::string` (fed the
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
/// for binary_signs (also `{{ .Artifact }}.sig` — anodizer's flat dist
/// layout means binary names already carry the platform suffix; no
/// duplication needed).
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

/// The name template a binary signature falls back to when NO `archives:`
/// entry covers the binary's target — a musl build that only feeds npm, say.
///
/// The full triple is required: `Os`/`Arch` render identically for a gnu and
/// a musl build of one machine, so a `{{ Os }}-{{ Arch }}` name would collapse
/// the two targets' signatures onto one asset. The amd64 micro-architecture
/// level is the dimension the triple itself does not carry — a baseline and a
/// `-Ctarget-cpu=x86-64-v3` build share `x86_64-unknown-linux-gnu` — so the
/// tail every default name template appends
/// ([`anodizer_core::archive_name::INSTALLER_AMD64_VARIANT_SUFFIX`]) is
/// appended here too; `v1` renders nothing, so an ordinary build keeps its
/// historical name.
pub(crate) const UNCOVERED_TARGET_NAME_TEMPLATE: &str = concat!(
    "{{ Binary }}-{{ Version }}-{{ Target }}",
    "{% if Amd64 and Amd64 != \"v1\" %}{{ Amd64 }}{% endif %}"
);

/// The rendered base of a binary signature asset together with the template
/// it came from, so a diagnostic about the name can quote the template the
/// operator has to change.
pub(crate) struct BinarySignNaming {
    /// The rendered asset base.
    pub(crate) base: String,
    /// The template that rendered it.
    pub(crate) template: String,
}

/// The release-asset BASE name every signature and certificate of one raw
/// binary is built on — unique per (crate, target, binary).
///
/// Derived from CONFIG alone, never from which archives the run has
/// registered, so `anodizer build` (which signs before any archive exists)
/// and `anodizer release --publish-only` (whose registry is the preserved
/// manifest) name the same binary's signature identically:
///
/// | the binary's target | base |
/// |---|---|
/// | covered by the crate's primary `archives:` entry — the first entry in config order whose `ids:` / `binaries:` filters take this binary — and that entry packs this binary alone on the target | that entry's `name_template`, rendered in the archive stage's own per-target scope ([`anodizer_core::archive_name::seed_archive_name_vars`]) |
/// | covered by an entry that packs SEVERAL binaries on the target, or by no entry at all | [`UNCOVERED_TARGET_NAME_TEMPLATE`] |
/// | covered by an entry whose RESOLVED formats include `binary` | that executable's own asset name (the per-binary default template plus the Windows `.exe`), so a `signs:` signature over the uploaded binary and a `binary_signs:` signature over the same bytes resolve to one name |
/// | a lipo-merged universal binary (`darwin-universal`, which no `builds:` entry names) | the covering entry's `name_template` rendered with the binary's OWN name |
///
/// The entry's `if:` is deliberately NOT evaluated: a gate that reads the
/// environment would name one binary's signature differently on the machine
/// that builds it and the machine that publishes it.
///
/// A `binary_signs:` entry's `asset_name_template:` overrides every row.
pub(crate) fn binary_sign_asset_naming(
    ctx: &Context,
    cfg: &SignConfig,
    binary: &anodizer_core::artifact::Artifact,
    target: &str,
) -> Result<BinarySignNaming> {
    use anodizer_core::archive_name;

    let binary_name = binary.binary_name().unwrap_or_default();
    let amd64_variant = binary.metadata.get("amd64_variant").map(String::as_str);
    // The archive stage rebinds `ProjectName` to the per-crate name whenever
    // its work list holds more than one crate, and picks the multi-crate
    // default template on the same condition. Answered from config, since the
    // registry-aware answer differs between a build and a publish-only run.
    let multi_crate = archive_name::config_archives_more_than_one_crate(ctx);
    let seeded = |binary_var: &str| {
        let mut vars = ctx.template_vars().clone();
        if multi_crate {
            vars.set("ProjectName", &binary.crate_name);
        }
        archive_name::seed_archive_name_vars(
            &mut vars,
            target,
            &binary.crate_name,
            binary_var,
            amd64_variant,
        );
        vars
    };
    let render = |template: &str, vars: &anodizer_core::template::TemplateVars| {
        anodizer_core::template::render(template, vars).with_context(|| {
            format!(
                "sign: render binary signature asset name '{template}' for \
                 {}/{target}",
                binary.crate_name
            )
        })
    };

    let named = |base: Result<String>, template: &str| -> Result<BinarySignNaming> {
        Ok(BinarySignNaming {
            base: reject_empty_base(base?)?,
            template: template.to_string(),
        })
    };

    if let Some(template) = cfg.asset_name_template.as_deref() {
        return named(render(template, &seeded(&binary_name)), template);
    }

    let krate = ctx.config.find_crate(&binary.crate_name);
    let entry = krate
        .map(anodizer_core::archive_selection::effective_archive_configs)
        .unwrap_or_default()
        .into_iter()
        .find(|c| {
            // A meta entry packs no binaries, so the archive stage names it
            // with an EMPTY `{{ Binary }}` — a base derived from it names an
            // asset this binary never appears in.
            !c.meta.unwrap_or(false)
                && anodizer_core::artifact::matches_id_filter(binary, c.ids.as_deref())
                && c.binaries
                    .as_deref()
                    .is_none_or(|names| names.contains(&binary_name))
        });

    let Some(entry) = entry else {
        return named(
            render(UNCOVERED_TARGET_NAME_TEMPLATE, &seeded(&binary_name)),
            UNCOVERED_TARGET_NAME_TEMPLATE,
        );
    };

    let formats = archive_name::archive_formats_for_target(
        &entry,
        target,
        &archive_name::global_format_overrides(ctx),
        &archive_name::global_default_archive_format(ctx),
    );
    // An entry may produce several formats at once; whenever `binary` is one
    // of them the executable is itself an uploaded asset, and the signature
    // over its bytes takes that asset's name.
    let is_binary_format = formats
        .iter()
        .any(|f| f == anodizer_core::artifact::FORMAT_BINARY);
    let template = entry.name_template.clone().unwrap_or_else(|| {
        if is_binary_format {
            archive_name::DEFAULT_BINARY_NAME_TEMPLATE.to_string()
        } else if multi_crate {
            // Chosen from the same config-only answer that rebinds
            // `ProjectName` above; the registry-aware resolver would pick one
            // default on a build and another on a publish-only run.
            archive_name::DEFAULT_NAME_TEMPLATE_MULTI_CRATE.to_string()
        } else {
            archive_name::DEFAULT_NAME_TEMPLATE.to_string()
        }
    });
    if is_binary_format {
        // Each executable is published under its own name, so the group holds
        // no ambiguity to resolve. The stem is checked before the extension is
        // appended: a bare `.exe` names no subject either.
        let stem = reject_empty_base(render(&template, &seeded(&binary_name))?)?;
        return Ok(BinarySignNaming {
            base: archive_name::binary_output_name(stem, target),
            template,
        });
    }

    let packed: Vec<String> = krate
        .map(|krate| {
            anodizer_core::build_plan::archive_target_binaries(
                krate,
                entry.ids.as_deref(),
                entry.binaries.as_deref(),
                target,
                &ctx.config.effective_default_targets(),
                |t| ctx.render_template(t),
            )
        })
        .unwrap_or_default();
    // One archive carries the whole group under a single name, so several
    // packed binaries cannot each be named after it — the release would keep
    // one signature and silently drop its siblings.
    if packed.len() > 1 {
        return named(
            render(UNCOVERED_TARGET_NAME_TEMPLATE, &seeded(&binary_name)),
            UNCOVERED_TARGET_NAME_TEMPLATE,
        );
    }
    let binary_var = packed
        .into_iter()
        .next()
        .unwrap_or_else(|| binary_name.clone());
    named(render(&template, &seeded(&binary_var)), &template)
}

/// Reject an empty rendered base before it becomes a `.sig` asset with no
/// stem — a name the release cannot match to its subject and that collides
/// with every other binary's signature.
fn reject_empty_base(base: String) -> Result<String> {
    if base.is_empty() {
        anyhow::bail!(
            "sign: the binary signature asset name rendered empty. An empty \
             base uploads the signature as a bare `.sig`, which every other \
             binary's signature collides with. Verify the templates it is \
             rendered from (`binary_signs[].asset_name_template`, or the \
             covering `archives[].name_template`) reference variables this run \
             populates — `{{{{ Tag }}}}` is unset under `--snapshot`, use \
             `{{{{ Version }}}}`."
        );
    }
    Ok(base)
}

/// Which raw binary claimed each signature asset name in this run.
///
/// One release asset carries one file, so two binaries resolving to one asset
/// name upload two signatures under one name and the release keeps whichever
/// arrived last. The default templates separate every binary by target and
/// micro-architecture level, but a hand-written `archives[].name_template`
/// need not — `{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}` renders
/// one name for a baseline and a `x86-64-v3` build of the same binary.
#[derive(Default)]
pub(crate) struct BinarySignAssetNames {
    claimed: HashMap<String, BinaryIdentity>,
}

/// How the artifact registry tells one raw binary from another.
///
/// Not the file path: two `builds:` entries differing only in
/// `amd64_variant:` compile to ONE path, and `no_unique_dist_dir: true`
/// flattens every binary of a crate onto `dist/<file name>`. The registry
/// separates them by the build entry they came from, the target they were
/// compiled for and the executable they carry.
#[derive(Clone, PartialEq, Eq)]
struct BinaryIdentity {
    crate_name: String,
    build_id: String,
    target: String,
    binary: String,
    amd64_variant: String,
    path: String,
}

impl BinaryIdentity {
    fn of(binary: &anodizer_core::artifact::Artifact) -> Self {
        let meta = |key: &str| binary.metadata.get(key).cloned();
        Self {
            crate_name: binary.crate_name.clone(),
            build_id: meta("id").unwrap_or_else(|| binary.name.clone()),
            target: binary.target.clone().unwrap_or_default(),
            binary: meta("binary").unwrap_or_else(|| binary.name.clone()),
            // An absent level IS the baseline, so a v1 build and a v3 build
            // of one binary are two identities rather than one.
            amd64_variant: meta("amd64_variant").unwrap_or_else(|| "v1".to_string()),
            path: binary.path.display().to_string(),
        }
    }
}

impl std::fmt::Display for BinaryIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} (crate '{}', build id '{}', target {}, amd64 {})",
            self.path, self.crate_name, self.build_id, self.target, self.amd64_variant
        )
    }
}

impl BinarySignAssetNames {
    /// Record `asset_name` as claimed by one binary, or refuse the run when a
    /// different binary already claimed the same name.
    pub(crate) fn claim(
        &mut self,
        asset_name: &str,
        template: &str,
        binary: &anodizer_core::artifact::Artifact,
    ) -> Result<()> {
        let claimant = BinaryIdentity::of(binary);
        match self.claimed.get(asset_name) {
            Some(first) if *first != claimant => anyhow::bail!(
                "sign: the binaries '{first}' and '{claimant}' both resolve to \
                 the signature asset name '{asset_name}', rendered from the \
                 template '{template}'. One release asset cannot carry two \
                 signatures — give the covering `archives[].name_template` a \
                 variable that separates them ({{{{ Target }}}} and \
                 {{{{ Amd64 }}}} are the dimensions {{{{ Os }}}}-{{{{ Arch }}}} \
                 drops), or set `binary_signs[].asset_name_template`."
            ),
            Some(_) => Ok(()),
            None => {
                self.claimed.insert(asset_name.to_string(), claimant);
                Ok(())
            }
        }
    }
}

/// The release-asset name a `binary_signs:` output registers under: the
/// config-derived [`binary_sign_asset_naming`] base plus the suffix the
/// `signature:` / `certificate:` template appended to the binary's own file
/// name.
///
/// The raw binary is called the same thing under every target's directory
/// (`anodizer.sig` eight times over), so the base carries the target.
/// `anodizer.exe` → `anodizer.exe.sig` yields `<base>.sig` and a cosign bundle
/// `anodizer.bundle.sig` yields `<base>.bundle.sig`.
///
/// Falls back to the target-qualified basename when the template renamed the
/// file rather than suffixing it — still unique per target.
pub(crate) fn binary_sign_asset_name(
    rendered_basename: &str,
    binary_basename: &str,
    base: &str,
    target: &str,
) -> String {
    if binary_basename.is_empty() {
        return qualify_basename_with_target(rendered_basename, target);
    }
    match rendered_basename.strip_prefix(binary_basename) {
        Some(suffix) if !suffix.is_empty() => format!("{base}{suffix}"),
        _ => qualify_basename_with_target(rendered_basename, target),
    }
}

#[cfg(test)]
mod binary_sign_asset_name_tests {
    use super::{binary_sign_asset_name, binary_sign_asset_naming};
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::{
        ArchiveConfig, ArchivesConfig, BuildConfig, CrateConfig, SignConfig,
    };
    use anodizer_core::context::Context;
    use anodizer_core::test_helpers::TestContextBuilder;

    const LINUX: &str = "x86_64-unknown-linux-gnu";
    const MUSL: &str = "x86_64-unknown-linux-musl";
    const WINDOWS: &str = "x86_64-pc-windows-msvc";
    const TEMPLATE: &str = "{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}";

    /// The rendered base alone, which every row assertion below reads.
    fn binary_sign_asset_base(
        ctx: &Context,
        cfg: &SignConfig,
        binary: &Artifact,
        target: &str,
    ) -> anyhow::Result<String> {
        Ok(binary_sign_asset_naming(ctx, cfg, binary, target)?.base)
    }

    fn archive(id: &str, name_template: &str) -> ArchiveConfig {
        ArchiveConfig {
            id: Some(id.to_string()),
            name_template: Some(name_template.to_string()),
            ..Default::default()
        }
    }

    fn crate_with(archives: Vec<ArchiveConfig>) -> CrateConfig {
        CrateConfig {
            name: "app".to_string(),
            path: ".".to_string(),
            builds: Some(vec![BuildConfig {
                binary: Some("app".to_string()),
                targets: Some(vec![
                    LINUX.to_string(),
                    MUSL.to_string(),
                    WINDOWS.to_string(),
                ]),
                ..Default::default()
            }]),
            archives: ArchivesConfig::Configs(archives),
            ..Default::default()
        }
    }

    fn ctx_with(archives: Vec<ArchiveConfig>) -> Context {
        let mut ctx = TestContextBuilder::new()
            .project_name("app")
            .crates(vec![crate_with(archives)])
            .build();
        ctx.template_vars_mut().set("ProjectName", "app");
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx
    }

    /// A binary whose own name differs from the project name, so a row that
    /// renders `{{ Binary }}` is told apart from one that renders
    /// `{{ ProjectName }}`.
    fn named_binary(target: &str, binary_name: &str) -> Artifact {
        let mut artifact = binary(target, None);
        artifact
            .metadata
            .insert("binary_name".to_string(), binary_name.to_string());
        artifact.name = binary_name.to_string();
        artifact.path = std::path::PathBuf::from(format!("target/{target}/release/{binary_name}"));
        artifact
    }

    fn binary(target: &str, id: Option<&str>) -> Artifact {
        let mut metadata = std::collections::HashMap::new();
        metadata.insert("binary_name".to_string(), "app".to_string());
        if let Some(id) = id {
            metadata.insert("id".to_string(), id.to_string());
        }
        Artifact {
            kind: ArtifactKind::Binary,
            name: "app".to_string(),
            path: std::path::PathBuf::from(format!("target/{target}/release/app")),
            target: Some(target.to_string()),
            crate_name: "app".to_string(),
            metadata,
            size: None,
        }
    }

    /// The already-registered archive of a publish-only run's preserved
    /// manifest. The base must not depend on it.
    fn add_archive(ctx: &mut Context, id: &str, stem: &str, file: &str) {
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: stem.to_string(),
            path: std::path::PathBuf::from(format!("dist/{file}")),
            target: Some(LINUX.to_string()),
            crate_name: "app".to_string(),
            metadata: [("id", id), ("name", stem)]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            size: None,
        });
    }

    /// The primary entry is the first in CONFIG order, and its template is
    /// what names the signature — whatever the registry holds, and whatever
    /// order it holds it in. `anodizer build` (empty registry) therefore
    /// names the asset exactly as `anodizer release` does.
    #[test]
    fn the_primary_entry_template_names_the_base_whatever_the_registry_holds() {
        let archives = vec![
            archive("default", TEMPLATE),
            archive(
                "extra",
                concat!(
                    "{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}",
                    "-extra"
                ),
            ),
        ];
        let empty = ctx_with(archives.clone());

        let mut extra_only = ctx_with(archives.clone());
        add_archive(
            &mut extra_only,
            "extra",
            "app-1.0.0-linux-amd64-extra",
            "app-1.0.0-linux-amd64-extra.tar.xz",
        );

        let mut extra_first = ctx_with(archives.clone());
        add_archive(
            &mut extra_first,
            "extra",
            "app-1.0.0-linux-amd64-extra",
            "app-1.0.0-linux-amd64-extra.tar.xz",
        );
        add_archive(
            &mut extra_first,
            "default",
            "app-1.0.0-linux-amd64",
            "app-1.0.0-linux-amd64.tar.gz",
        );

        let mut default_first = ctx_with(archives);
        add_archive(
            &mut default_first,
            "default",
            "app-1.0.0-linux-amd64",
            "app-1.0.0-linux-amd64.tar.gz",
        );
        add_archive(
            &mut default_first,
            "extra",
            "app-1.0.0-linux-amd64-extra",
            "app-1.0.0-linux-amd64-extra.tar.xz",
        );

        for (label, ctx) in [
            ("no archive registered (anodizer build)", &empty),
            ("only the -extra archive registered", &extra_only),
            ("-extra registered first", &extra_first),
            ("default registered first", &default_first),
        ] {
            assert_eq!(
                binary_sign_asset_base(ctx, &SignConfig::default(), &binary(LINUX, None), LINUX)
                    .unwrap(),
                "app-1.0.0-linux-amd64",
                "{label}"
            );
        }
    }

    /// The suffix the `signature:` / `certificate:` template appended to the
    /// binary's own file name carries onto the base — including the Windows
    /// `.exe` the raw binary carries and the base does not.
    #[test]
    fn the_suffix_after_the_binary_name_carries_onto_the_base() {
        let ctx = ctx_with(vec![archive("default", TEMPLATE)]);
        let base = binary_sign_asset_base(
            &ctx,
            &SignConfig::default(),
            &binary(WINDOWS, None),
            WINDOWS,
        )
        .unwrap();
        assert_eq!(base, "app-1.0.0-windows-amd64");
        for (rendered, binary_file, expected) in [
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
                binary_sign_asset_name(rendered, binary_file, &base, WINDOWS),
                expected,
                "{rendered} over {binary_file}"
            );
        }
    }

    /// A `signature:` template that RENAMED the file rather than suffixing the
    /// binary's own name has no suffix to carry, so the rendered basename is
    /// qualified with the triple instead — still one asset per target.
    #[test]
    fn a_renamed_signature_falls_back_to_the_target_qualified_name() {
        assert_eq!(
            binary_sign_asset_name("detached.sig", "app", "app-1.0.0-windows-amd64", WINDOWS),
            format!("detached-{WINDOWS}.sig")
        );
    }

    /// A target no archive entry covers keeps the full triple: `Os`/`Arch`
    /// render identically for the gnu and musl builds of one machine.
    #[test]
    fn an_uncovered_target_is_named_with_the_whole_triple() {
        let ctx = ctx_with(vec![ArchiveConfig {
            ids: Some(vec!["gnu".to_string()]),
            name_template: Some(TEMPLATE.to_string()),
            ..Default::default()
        }]);
        let base = binary_sign_asset_base(
            &ctx,
            &SignConfig::default(),
            &binary(MUSL, Some("musl")),
            MUSL,
        )
        .unwrap();
        assert_eq!(base, format!("app-1.0.0-{MUSL}"));
        assert_eq!(
            binary_sign_asset_name("app.sig", "app", &base, MUSL),
            format!("app-1.0.0-{MUSL}.sig")
        );
    }

    /// A `formats: [binary]` entry publishes the executable itself, so the
    /// signature is named after that asset — a `signs:` signature over the
    /// uploaded binary and a `binary_signs:` signature over the same bytes
    /// resolve to one name. The Windows `.exe` is part of it.
    #[test]
    fn a_binary_format_entry_names_the_base_after_the_published_executable() {
        let ctx = ctx_with(vec![ArchiveConfig {
            formats: Some(vec!["binary".to_string()]),
            ..Default::default()
        }]);
        assert_eq!(
            binary_sign_asset_base(&ctx, &SignConfig::default(), &binary(LINUX, None), LINUX)
                .unwrap(),
            "app_1.0.0_linux_amd64"
        );
        assert_eq!(
            binary_sign_asset_base(
                &ctx,
                &SignConfig::default(),
                &binary(WINDOWS, None),
                WINDOWS
            )
            .unwrap(),
            "app_1.0.0_windows_amd64.exe"
        );
    }

    /// One archive carries a whole group of binaries under a single name, so
    /// naming both binaries' signatures after it uploads two assets called the
    /// same thing and the release keeps one. Each falls back to the whole
    /// triple instead.
    #[test]
    fn two_binaries_of_one_crate_register_distinct_signature_assets() {
        let mut ctx = TestContextBuilder::new()
            .project_name("app")
            .crates(vec![CrateConfig {
                name: "app".to_string(),
                path: ".".to_string(),
                builds: Some(vec![
                    BuildConfig {
                        binary: Some("app".to_string()),
                        targets: Some(vec![LINUX.to_string()]),
                        ..Default::default()
                    },
                    BuildConfig {
                        binary: Some("helper".to_string()),
                        targets: Some(vec![LINUX.to_string()]),
                        ..Default::default()
                    },
                ]),
                archives: ArchivesConfig::Configs(vec![archive("default", TEMPLATE)]),
                ..Default::default()
            }])
            .build();
        ctx.template_vars_mut().set("ProjectName", "app");
        ctx.template_vars_mut().set("Version", "1.0.0");

        let base = |name: &str, amd64_variant: Option<&str>| {
            let mut artifact = binary(LINUX, None);
            artifact.name = name.to_string();
            artifact.path = std::path::PathBuf::from(format!("target/{LINUX}/release/{name}"));
            if let Some(variant) = amd64_variant {
                artifact
                    .metadata
                    .insert("amd64_variant".to_string(), variant.to_string());
            }
            binary_sign_asset_base(&ctx, &SignConfig::default(), &artifact, LINUX).unwrap()
        };
        assert_eq!(base("app", None), format!("app-1.0.0-{LINUX}"));
        assert_eq!(base("helper", None), format!("helper-1.0.0-{LINUX}"));
        assert_ne!(base("app", None), base("helper", None));

        // The build stage emits one artifact per micro-architecture level on
        // ONE target, so the triple alone does not separate them.
        assert_eq!(base("app", Some("v1")), format!("app-1.0.0-{LINUX}"));
        assert_eq!(base("app", Some("v3")), format!("app-1.0.0-{LINUX}v3"));
        assert_ne!(base("app", Some("v1")), base("app", Some("v3")));
    }

    /// The uncovered-target fallback appends the same amd64 clause every
    /// default name template does, so the two cannot drift apart.
    #[test]
    fn the_uncovered_target_template_ends_with_the_shared_amd64_suffix() {
        assert!(
            super::UNCOVERED_TARGET_NAME_TEMPLATE
                .ends_with(anodizer_core::archive_name::INSTALLER_AMD64_VARIANT_SUFFIX),
            "uncovered-target template must reuse the shared amd64 variant suffix: {}",
            super::UNCOVERED_TARGET_NAME_TEMPLATE
        );
    }

    /// A `binaries:` allow-list that packs this binary alone keeps the entry's
    /// own name: the group is unambiguous again.
    #[test]
    fn a_binaries_filter_that_excludes_this_binary_moves_the_primary_to_the_next_entry() {
        let mut ctx = TestContextBuilder::new()
            .project_name("app")
            .crates(vec![CrateConfig {
                name: "app".to_string(),
                path: ".".to_string(),
                builds: Some(vec![
                    BuildConfig {
                        binary: Some("app".to_string()),
                        targets: Some(vec![LINUX.to_string()]),
                        ..Default::default()
                    },
                    BuildConfig {
                        binary: Some("helper".to_string()),
                        targets: Some(vec![LINUX.to_string()]),
                        ..Default::default()
                    },
                ]),
                archives: ArchivesConfig::Configs(vec![
                    ArchiveConfig {
                        id: Some("app-only".to_string()),
                        binaries: Some(vec!["app".to_string()]),
                        name_template: Some(TEMPLATE.to_string()),
                        ..Default::default()
                    },
                    ArchiveConfig {
                        binaries: Some(vec!["helper".to_string()]),
                        ..archive(
                            "rest",
                            "{{ Binary }}-{{ Version }}-{{ Os }}-{{ Arch }}-rest",
                        )
                    },
                ]),
                ..Default::default()
            }])
            .build();
        ctx.template_vars_mut().set("ProjectName", "app");
        ctx.template_vars_mut().set("Version", "1.0.0");

        let base = |name: &str| {
            let mut artifact = binary(LINUX, None);
            artifact.name = name.to_string();
            artifact.path = std::path::PathBuf::from(format!("target/{LINUX}/release/{name}"));
            binary_sign_asset_base(&ctx, &SignConfig::default(), &artifact, LINUX).unwrap()
        };
        assert_eq!(base("app"), "app-1.0.0-linux-amd64");
        assert_eq!(base("helper"), "helper-1.0.0-linux-amd64-rest");
    }

    /// A meta entry packs no binaries at all, so the archive stage renders its
    /// name with an empty `{{ Binary }}`. The primary is the first entry that
    /// actually holds this binary.
    #[test]
    fn a_meta_first_entry_is_skipped_and_the_next_entry_names_the_base() {
        let ctx = ctx_with(vec![
            ArchiveConfig {
                id: Some("docs".to_string()),
                meta: Some(true),
                name_template: Some("{{ ProjectName }}-docs".to_string()),
                ..Default::default()
            },
            archive("default", TEMPLATE),
        ]);
        assert_eq!(
            binary_sign_asset_base(&ctx, &SignConfig::default(), &binary(LINUX, None), LINUX)
                .unwrap(),
            "app-1.0.0-linux-amd64"
        );
    }

    /// The entry's `if:` is not evaluated: it can read the environment, and a
    /// signature named one way on the build host and another on the publish
    /// host is a release asset the verify gate cannot find.
    #[test]
    fn a_gated_primary_entry_still_names_the_base() {
        let ctx = ctx_with(vec![
            ArchiveConfig {
                if_condition: Some("false".to_string()),
                ..archive("default", TEMPLATE)
            },
            archive(
                "extra",
                "{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}-extra",
            ),
        ]);
        assert_eq!(
            binary_sign_asset_base(&ctx, &SignConfig::default(), &binary(LINUX, None), LINUX)
                .unwrap(),
            "app-1.0.0-linux-amd64"
        );
    }

    /// A sibling crate that configures archives but builds nothing counts
    /// toward the archive stage's work list only once something archivable is
    /// registered for it — which an `anodizer build` run has not done yet.
    /// Basing the multi-crate decision on that would rebind `ProjectName` on
    /// one command and not the other. What this holds is the `ProjectName`
    /// rebinding alone: the multi-crate default template renders the same
    /// string as the single-crate one, so the template half of the decision
    /// is held by the structural pin in `tests.rs` instead.
    #[test]
    fn the_base_is_stable_when_an_artifact_only_sibling_crate_has_nothing_registered_yet() {
        let run = |register_sibling_archive: bool| {
            let mut ctx = TestContextBuilder::new()
                .project_name("proj")
                .crates(vec![
                    crate_with(vec![ArchiveConfig::default()]),
                    CrateConfig {
                        name: "extras".to_string(),
                        path: "extras".to_string(),
                        archives: ArchivesConfig::Configs(vec![ArchiveConfig::default()]),
                        ..Default::default()
                    },
                ])
                .build();
            ctx.template_vars_mut().set("ProjectName", "proj");
            ctx.template_vars_mut().set("Version", "1.0.0");
            if register_sibling_archive {
                ctx.artifacts.add(Artifact {
                    kind: ArtifactKind::Archive,
                    name: "extras-1.0.0".to_string(),
                    path: std::path::PathBuf::from("dist/extras-1.0.0.tar.gz"),
                    target: Some(LINUX.to_string()),
                    crate_name: "extras".to_string(),
                    metadata: Default::default(),
                    size: None,
                });
            }
            binary_sign_asset_base(&ctx, &SignConfig::default(), &binary(LINUX, None), LINUX)
                .unwrap()
        };
        // What differs between the two runs is the `ProjectName` rebinding:
        // the registry-aware answer counts two crates once the sibling's
        // archive is registered and renders `app_…`. The template CHOICE is
        // not covered here — `DEFAULT_NAME_TEMPLATE_MULTI_CRATE` is defined
        // as `DEFAULT_NAME_TEMPLATE`, so both arms render alike; the
        // structural assertion in `tests.rs` holds that half.
        assert_eq!(run(false), "proj_1.0.0_linux_amd64");
        assert_eq!(run(true), run(false));
    }

    /// A lockstep multi-crate run rebinds `ProjectName` to each crate's own
    /// name while the archive stage renders that crate's asset, so the
    /// signature bases must differ by crate.
    #[test]
    fn a_lockstep_multi_crate_run_names_each_crate_signature_under_its_own_project_name() {
        let crate_cfg = |name: &str| CrateConfig {
            name: name.to_string(),
            path: name.to_string(),
            builds: Some(vec![BuildConfig {
                binary: Some(name.to_string()),
                targets: Some(vec![LINUX.to_string()]),
                ..Default::default()
            }]),
            archives: ArchivesConfig::Configs(vec![archive("default", TEMPLATE)]),
            ..Default::default()
        };
        let mut ctx = TestContextBuilder::new()
            .project_name("proj")
            .crates(vec![crate_cfg("app"), crate_cfg("helper")])
            .build();
        ctx.template_vars_mut().set("ProjectName", "proj");
        ctx.template_vars_mut().set("Version", "1.0.0");

        let base = |name: &str| {
            let mut artifact = binary(LINUX, None);
            artifact.name = name.to_string();
            artifact.crate_name = name.to_string();
            artifact.path = std::path::PathBuf::from(format!("target/{LINUX}/release/{name}"));
            binary_sign_asset_base(&ctx, &SignConfig::default(), &artifact, LINUX).unwrap()
        };
        assert_eq!(base("app"), "app-1.0.0-linux-amd64");
        assert_eq!(base("helper"), "helper-1.0.0-linux-amd64");
    }

    /// A v3-tuned group's archive carries the micro-architecture level in its
    /// name, so the signature over its binary carries the same suffix.
    #[test]
    fn a_v3_group_signature_base_carries_the_amd64_suffix() {
        let ctx = ctx_with(vec![archive(
            "default",
            concat!(
                "{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}",
                "{% if Amd64 and Amd64 != \"v1\" %}{{ Amd64 }}{% endif %}"
            ),
        )]);
        let mut artifact = binary(LINUX, None);
        artifact
            .metadata
            .insert("amd64_variant".to_string(), "v3".to_string());
        assert_eq!(
            binary_sign_asset_base(&ctx, &SignConfig::default(), &artifact, LINUX).unwrap(),
            "app-1.0.0-linux-amd64v3"
        );
    }

    /// `defaults.archives.format_overrides` applies to an entry that declares
    /// none of its own — exactly as the archive stage plans its outputs — so a
    /// global override to `binary` names the signature after the published
    /// executable.
    #[test]
    fn a_global_format_override_to_binary_names_the_signature_after_the_executable() {
        let mut ctx = TestContextBuilder::new()
            .project_name("app")
            .crates(vec![crate_with(vec![ArchiveConfig::default()])])
            .defaults(anodizer_core::config::Defaults {
                archives: Some(ArchiveConfig {
                    format_overrides: Some(vec![anodizer_core::config::FormatOverride {
                        os: "windows".to_string(),
                        formats: Some(vec!["binary".to_string()]),
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .build();
        ctx.template_vars_mut().set("ProjectName", "app");
        ctx.template_vars_mut().set("Version", "1.0.0");
        // The binary is named apart from the project, so the assertion holds
        // the template SHAPE — the archive default would render `app_…` — as
        // well as the `.exe` the executable's own asset name carries.
        assert_eq!(
            binary_sign_asset_base(
                &ctx,
                &SignConfig::default(),
                &named_binary(WINDOWS, "helper"),
                WINDOWS
            )
            .unwrap(),
            "helper_1.0.0_windows_amd64.exe"
        );
    }

    /// An entry producing several formats at once publishes the executable
    /// itself whenever `binary` is among them, whatever position it holds in
    /// the list.
    #[test]
    fn an_entry_listing_binary_second_still_names_the_signature_after_the_executable() {
        let ctx = ctx_with(vec![ArchiveConfig {
            formats: Some(vec!["tar.gz".to_string(), "binary".to_string()]),
            ..Default::default()
        }]);
        assert_eq!(
            binary_sign_asset_base(
                &ctx,
                &SignConfig::default(),
                &named_binary(WINDOWS, "helper"),
                WINDOWS
            )
            .unwrap(),
            "helper_1.0.0_windows_amd64.exe"
        );
    }

    /// `asset_name_template:` overrides every derived row, and renders in the
    /// same per-target scope.
    #[test]
    fn the_asset_name_template_override_wins() {
        let ctx = ctx_with(vec![archive("default", TEMPLATE)]);
        let cfg = SignConfig {
            asset_name_template: Some("{{ Binary }}-{{ Target }}-signed".to_string()),
            ..Default::default()
        };
        let base = binary_sign_asset_base(&ctx, &cfg, &binary(LINUX, None), LINUX).unwrap();
        assert_eq!(base, format!("app-{LINUX}-signed"));
        assert_eq!(
            binary_sign_asset_name("app.sig", "app", &base, LINUX),
            format!("app-{LINUX}-signed.sig")
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
