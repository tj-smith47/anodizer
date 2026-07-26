use super::*;

// ---------------------------------------------------------------------------
// build_reqwest_client
// ---------------------------------------------------------------------------

/// Build a reqwest blocking client with optional mTLS and trusted CA certs.
pub fn build_reqwest_client(
    client_cert_path: Option<&str>,
    client_key_path: Option<&str>,
    trusted_certs_pem: Option<&str>,
) -> Result<reqwest::blocking::Client> {
    // Bound every request so a stalled Artifactory upload (unreachable host,
    // hung TLS, a black-holed route mid-PUT) fails fast instead of hanging the
    // release forever. Matches the 300 s request bound the gitea/gitlab release
    // backends and the bucket clients already carry.
    let mut builder = reqwest::blocking::ClientBuilder::new()
        .user_agent("anodizer/1.0")
        .timeout(std::time::Duration::from_secs(300))
        .connect_timeout(std::time::Duration::from_secs(30));

    // mTLS client certificate
    if let (Some(cert_path), Some(key_path)) = (client_cert_path, client_key_path) {
        let cert_pem = fs::read(cert_path)
            .with_context(|| format!("artifactory: failed to read client cert '{}'", cert_path))?;
        let key_pem = fs::read(key_path)
            .with_context(|| format!("artifactory: failed to read client key '{}'", key_path))?;
        // Identity::from_pem expects a single PEM buffer with both cert and key
        let mut combined_pem = cert_pem;
        combined_pem.push(b'\n');
        combined_pem.extend_from_slice(&key_pem);
        let identity = reqwest::Identity::from_pem(&combined_pem)
            .context("artifactory: failed to load client certificate identity")?;
        builder = builder.identity(identity);
    } else if client_cert_path.is_some() != client_key_path.is_some() {
        bail!(
            "artifactory: client_x509_cert and client_x509_key must both be set (or both omitted)"
        );
    }

    // Trusted CA certificates. A set-but-empty bundle almost always means
    // a copy-paste accident (PEM headers stripped, base64 truncated); bail
    // with a clear message instead of installing an empty trust store.
    if let Some(pem_data) = trusted_certs_pem {
        let trimmed = pem_data.trim();
        if trimmed.is_empty() {
            bail!(
                "artifactory: trusted_certificates is set but empty (remove the field \
                 to use the system trust store, or supply a valid PEM bundle)"
            );
        }
        let certs = reqwest::Certificate::from_pem_bundle(pem_data.as_bytes())
            .context("artifactory: failed to parse trusted_certificates PEM")?;
        if certs.is_empty() {
            bail!(
                "artifactory: trusted_certificates contains no parseable certificates \
                 (check PEM headers and that the bundle is not truncated)"
            );
        }
        for cert in certs {
            builder = builder.add_root_certificate(cert);
        }
    }

    builder
        .build()
        .context("artifactory: failed to build HTTP client")
}

// ---------------------------------------------------------------------------
// render_artifact_url
// ---------------------------------------------------------------------------

/// Render a target URL template with the artifact context bound
/// (Os, Arch, Target, ArtifactName, ArtifactExt).
///
/// When `custom_artifact_name` is false and the template does not already
/// reference `ArtifactName`, the artifact name is appended after the
/// rendered URL — guarding against the `…/foo.tar.gz/foo.tar.gz`
/// double-name when a user writes `target: ".../{{ .ArtifactName }}"`.
pub fn render_artifact_url(
    ctx: &Context,
    template: &str,
    artifact: &Artifact,
    custom_artifact_name: bool,
) -> Result<String> {
    let mut vars = ctx.template_vars().clone();
    let art_name = artifact.name();
    vars.set("ArtifactName", art_name);
    vars.set("ArtifactExt", &artifact.ext());
    if let Some(ref target) = artifact.target {
        let (os, arch) = anodizer_core::target::map_target(target);
        vars.set("Os", &os);
        vars.set("Arch", &arch);
        vars.set("Target", target);
    }

    let mut rendered = anodizer_core::template::render(template, &vars)
        .with_context(|| "artifactory: failed to render target URL template")?;

    // The substring check matches both `ArtifactName` and `.ArtifactName`
    // so the same guard works for Tera and Go-template syntax.
    if !custom_artifact_name && !template.contains("ArtifactName") {
        if !rendered.ends_with('/') {
            rendered.push('/');
        }
        rendered.push_str(art_name);
    }

    Ok(rendered)
}

/// Returns `true` when the artifact is a Debian package (`.deb`) that needs
/// the Artifactory Debian matrix params to be indexed by apt.
fn is_deb_artifact(artifact: &Artifact) -> bool {
    artifact.name().to_ascii_lowercase().ends_with(".deb")
}

/// Reject a configured Debian matrix-param slug that would break the
/// semicolon-delimited Artifactory upload matrix.
///
/// The value lands raw in `;deb.<key>=<value>`, so any `;` injects a rogue
/// matrix param and any whitespace malforms the URL — either way the `.deb`
/// lands at the wrong path and never indexes (the exact failure this feature
/// exists to prevent). Allow only the conservative Debian
/// distribution/component slug charset (ASCII alphanumerics plus `-`, `.`,
/// `_`); `/` is rejected too because an Artifactory deb matrix distribution is
/// a flat codename (`bookworm`), not a path.
///
/// An empty or whitespace-only value is rejected here as well: the list values
/// are joined with `,` into `deb.distribution=a,b`, so a `""` element produces
/// a trailing-comma `bookworm,` that mis-indexes the `.deb` into an
/// empty-named distribution. The charset check alone passes a `""` vacuously,
/// so this explicit emptiness guard runs first.
pub(crate) fn validate_deb_matrix_slug(field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!(
            "artifactory: empty {field} value '{value}' — a Debian repository \
             distribution/component slug must be a non-empty flat codename like \
             'bookworm' or 'stable'. An empty or whitespace-only entry joins into \
             a trailing-comma matrix param (e.g. 'bookworm,') that mis-indexes the \
             .deb into an empty-named slice. Remove the empty entry from the list.",
        );
    }
    let ok = value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'));
    if !ok {
        bail!(
            "artifactory: invalid {field} value '{value}' — Debian repository \
             distribution/component slugs may contain only ASCII letters, digits, \
             '-', '.', and '_' (a ';', '/', or whitespace would corrupt the upload \
             matrix params and leave the .deb unindexed). Use a flat codename like \
             'bookworm' or 'stable', or a comma-separated list of them.",
        );
    }
    Ok(())
}

/// Validate every user-supplied `deb_distributions` / `deb_components` /
/// `deb_architecture` slug on an Artifactory entry up front, so a slug that
/// would corrupt the upload matrix params hard-errors at config-validation
/// time (before any bytes are PUT) rather than silently shipping an
/// unindexable `.deb`. Only the user-supplied overrides are checked here; the
/// *derived* architecture (from each `.deb` artifact's build target) is
/// validated per-artifact at URL-composition time in
/// [`append_deb_matrix_params`], which hard-fails on a triple that has no known
/// Debian arch and re-runs the derived value through [`validate_deb_matrix_slug`].
pub(crate) fn validate_artifactory_deb_slugs(
    entry: &anodizer_core::config::ArtifactoryConfig,
) -> Result<()> {
    for d in entry.deb_distributions.iter().flatten() {
        validate_deb_matrix_slug("deb_distributions", d)?;
    }
    for c in entry.deb_components.iter().flatten() {
        validate_deb_matrix_slug("deb_components", c)?;
    }
    if let Some(arch) = entry.deb_architecture.as_deref() {
        validate_deb_matrix_slug("deb_architecture", arch)?;
    }
    Ok(())
}

/// Append Artifactory's Debian repository matrix params to a `.deb` upload
/// URL so the package is indexed (without them the uploaded `.deb` is a
/// dangling file apt never sees). Per JFrog's Debian-repo upload docs, the
/// params are semicolon-prefixed and appended to the path:
///
/// ```text
/// PUT .../pool/p.deb;deb.distribution=stable;deb.component=main;deb.architecture=amd64
/// ```
///
/// - `distribution` defaults to `["stable"]`, `component` to `["main"]`; both
///   are overridable via config and accept a comma-separated list (Artifactory
///   indexes the same `.deb` into every listed distribution/component).
/// - `architecture` is derived from the artifact's build target
///   ([`debian_arch_from_target`](anodizer_core::target::debian_arch_from_target));
///   an explicit `deb_architecture` override
///   wins. When the target is absent and no override is set, the
///   `deb.architecture` param is omitted (Artifactory then reads it from the
///   package's own control file).
///
/// Fallible: a build target whose architecture has no known Debian spelling
/// (an exotic or user-supplied `prebuilt` triple) hard-errors rather than
/// injecting a raw triple fragment as `deb.architecture=` — a silent wrong
/// value would land the `.deb` in the wrong (or an empty-named) repository
/// slice. The derived value is also re-checked against the matrix-param slug
/// charset as defense-in-depth.
///
/// A no-op for non-`.deb` artifacts: the URL is returned unchanged.
pub(crate) fn append_deb_matrix_params(
    url: &str,
    artifact: &Artifact,
    entry: &anodizer_core::config::ArtifactoryConfig,
) -> Result<String> {
    if !is_deb_artifact(artifact) {
        return Ok(url.to_string());
    }

    let distributions = entry
        .deb_distributions
        .as_deref()
        .filter(|d| !d.is_empty())
        .map(|d| d.join(","))
        .unwrap_or_else(|| "stable".to_string());
    let components = entry
        .deb_components
        .as_deref()
        .filter(|c| !c.is_empty())
        .map(|c| c.join(","))
        .unwrap_or_else(|| "main".to_string());

    let mut out = format!("{url};deb.distribution={distributions};deb.component={components}");

    let architecture = match entry.deb_architecture.clone() {
        Some(explicit) => Some(explicit),
        None => match artifact.target.as_deref() {
            Some(triple) => Some(
                anodizer_core::target::debian_arch_from_target(triple)
                    .map_err(|e| anyhow::anyhow!("artifactory: {e}"))?,
            ),
            None => None,
        },
    };
    if let Some(arch) = architecture.filter(|a| !a.is_empty()) {
        // Defense-in-depth: the explicit override was already validated up
        // front, and the derived value comes from a fixed table, but re-check
        // so neither path can ever inject a matrix-param-breaking char.
        validate_deb_matrix_slug("deb.architecture", &arch)?;
        out.push_str(&format!(";deb.architecture={arch}"));
    }
    Ok(out)
}
