use super::*;

// winget PackageIdentifier regex:
// `^[^\.\s\\/:\*\?"<>\|\x01-\x1f]{1,32}(\.[^\.\s\\/:\*\?"<>\|\x01-\x1f]{1,32}){1,7}$`
//
// Two delta points vs. the loose regex this replaced:
//   1. Each segment is bounded to 1..=32 chars (live winget validator
//      enforces this; longer segments fail the upstream PR check).
//   2. ASCII control chars `\x01..=\x1f` are excluded explicitly — winget
//      rejects them, so anodizer must too.
//
// `\x00` (NUL) is also rejected by winget but `regex` interprets `\x00`
// inside `[^...]` as the empty boundary; we strip NULs explicitly below
// before applying the regex to keep the engine happy.
pub(crate) static PACKAGE_IDENTIFIER_RE: LazyLock<Regex> = LazyLock::new(|| {
    static_regex(
        r#"^[^\.\s\\/:\*\?"<>\|\x01-\x1f]{1,32}(\.[^\.\s\\/:\*\?"<>\|\x01-\x1f]{1,32}){1,7}$"#,
    )
});

// ---------------------------------------------------------------------------
// PackageIdentifier validation
// ---------------------------------------------------------------------------

/// Resolve the `winget-pkgs` package identifier for a crate's
/// `publish.winget` block without a template context: the configured
/// `package_identifier`, falling back to the publisher's own auto-derivation
/// (`<publisher>.<name>` with spaces stripped, publisher defaulting to the
/// repository owner). Returns `None` when any contributing value is a
/// template expression or is missing — outside a release run there is
/// nothing to render with, and failure-recovery tooling probing the
/// community repository must skip rather than guess.
///
/// Public for the same reason as [`crate::cargo::targets_crates_io`]: `tag
/// rollback`'s published-state guard must name the same package the
/// publisher would submit.
pub fn static_package_identifier(
    crate_name: &str,
    cfg: &anodizer_core::config::WingetConfig,
) -> Option<String> {
    if let Some(id) = cfg.package_identifier.as_deref() {
        return (!id.contains("{{")).then(|| id.to_string());
    }
    let name = cfg.name.as_deref().unwrap_or(crate_name);
    let publisher = match cfg.publisher.as_deref() {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => crate::util::resolve_repo_owner_name(cfg.repository.as_ref())?.0,
    };
    if name.contains("{{") || publisher.contains("{{") {
        return None;
    }
    Some(auto_package_identifier(&publisher, name))
}

/// Derive a crate's `publish.winget` block for the live run: the configured
/// `package_identifier` template is rendered INTO the returned config, so the
/// struct every later consumer reads already carries the text winget will
/// receive.
///
/// This is the one render of that field. The one-way-door preflight probe, the
/// emission-validate pass, the manifest bodies, the manifest filenames and the
/// publish branch all read the derived field, so a raw `{{ … }}` cannot reach
/// a search URL or a manifest, and a new consumer has no separate helper to
/// forget.
pub(crate) fn derive_winget_config(
    ctx: &anodizer_core::context::Context,
    log: &anodizer_core::log::StageLogger,
    cfg: &anodizer_core::config::WingetConfig,
) -> Result<anodizer_core::config::WingetConfig> {
    let mut derived = cfg.clone();
    if let Some(raw) = cfg.package_identifier.as_deref() {
        derived.package_identifier = Some(crate::util::render_or_warn(
            ctx,
            log,
            "winget.package_identifier",
            raw,
        )?);
    }
    Ok(derived)
}

/// The PackageIdentifier a derived config carries: its `package_identifier`
/// (already rendered by [`derive_winget_config`]) or `auto` when the field is
/// unset — or when the field did not render.
///
/// A residual `{{` can only come from the non-strict render fallback, which
/// warns and keeps the raw template. A template is not an identifier: it names
/// a package that cannot exist, so the auto-derived identifier stands in
/// rather than reaching a manifest, a branch name or the PR search a poll is
/// built from.
pub(crate) fn package_identifier_of(
    cfg: &anodizer_core::config::WingetConfig,
    auto: &str,
) -> String {
    match cfg.package_identifier.as_deref() {
        Some(id) if !id.contains("{{") => id.to_string(),
        _ => auto.to_string(),
    }
}

/// Derive the automatic WinGet PackageIdentifier from a publisher display
/// name and a package name: `<publisher-without-spaces>.<name>` — the single
/// definition of the auto-id rule.
pub(crate) fn auto_package_identifier(publisher: &str, name: &str) -> String {
    format!("{}.{}", publisher.replace(' ', ""), name)
}

/// Validate a WinGet PackageIdentifier against the required pattern.
///
/// The identifier must have 2-8 dot-separated segments, each segment 1-32
/// characters, with no whitespace, ASCII control chars (`\x01-\x1f`), or
/// the characters `\`, `/`, `:`, `*`, `?`, `"`, `<`, `>`, `|`.
///
/// Pattern: `^[^\.\s\\/:\*\?"<>\|\x01-\x1f]{1,32}(\.[^\.\s\\/:\*\?"<>\|\x01-\x1f]{1,32}){1,7}$`
pub fn validate_package_identifier(id: &str) -> Result<()> {
    // NUL (`\x00`) is also forbidden by winget. The regex's character class
    // already excludes `\x01-\x1f` but excluding `\x00` inside an
    // already-negated class is awkward; reject NULs explicitly.
    if !id.contains('\u{0}') && PACKAGE_IDENTIFIER_RE.is_match(id) {
        Ok(())
    } else {
        anyhow::bail!(
            "winget: invalid PackageIdentifier '{}'. Must have 2-8 dot-separated segments, \
             each 1-32 chars, with no whitespace, control chars, or special characters \
             (\\/:*?\"<>|).",
            id
        )
    }
}

// ---------------------------------------------------------------------------
// Winget commit message rendering
// ---------------------------------------------------------------------------

/// Render a commit message for WinGet with PackageIdentifier in the context.
/// `PackageIdentifier` is exposed as an extra template field.
///
/// Strict-aware via [`util::render_or_warn_with_vars`]: a malformed
/// `commit_msg_template` errors under the guard / `--strict`, else warns and
/// falls back to the default-shaped raw message.
pub(crate) fn render_winget_commit_msg(
    template: Option<&str>,
    package_id: &str,
    version: &str,
    log: &StageLogger,
    is_strict: bool,
) -> Result<String> {
    // Default: "New version: {{ .PackageIdentifier }} {{ .Version }}"
    let default_tmpl = "New version: {{ PackageIdentifier }} {{ Version }}";
    let tmpl = template.unwrap_or(default_tmpl);

    let mut vars = TemplateVars::new();
    vars.set("PackageIdentifier", package_id);
    vars.set("ProjectName", package_id);
    vars.set("Tag", version);
    vars.set("Version", version);
    vars.set("name", package_id);
    vars.set("version", version);
    match template::render(tmpl, &vars) {
        Ok(rendered) => Ok(rendered),
        Err(e) => {
            if is_strict {
                anyhow::bail!("failed to render winget.commit_msg_template {tmpl:?}: {e}");
            }
            log.warn(&format!(
                "failed to render winget.commit_msg_template {tmpl:?}: {e}; \
                 falling back to default commit message"
            ));
            Ok(format!("New version: {} {}", package_id, version))
        }
    }
}

// ---------------------------------------------------------------------------
// WingetManifestParams
// ---------------------------------------------------------------------------
