use super::*;

/// Render a `key: value` template map (build_args / annotations / labels)
/// through the engine. Empty rendered keys or values are dropped. Output is
/// sorted by rendered key for deterministic emission.
/// `tplMapFlags`.
pub fn render_v2_kv_map(
    ctx: &mut Context,
    map: Option<&HashMap<String, String>>,
    label: &str,
) -> Result<Vec<(String, String)>> {
    let mut out: Vec<(String, String)> = Vec::new();
    if let Some(entries) = map {
        for (key_tmpl, value_tmpl) in entries {
            let rendered_key = ctx
                .render_template(key_tmpl)
                .with_context(|| format!("dockers_v2: render {} key '{}'", label, key_tmpl))?;
            let rendered_value = ctx.render_template(value_tmpl).with_context(|| {
                format!("dockers_v2: render {} value for '{}'", label, key_tmpl)
            })?;
            if !rendered_key.is_empty() && !rendered_value.is_empty() {
                out.push((rendered_key, rendered_value));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
    }
    Ok(out)
}

/// Whether the standard `org.opencontainers.image.*` labels should be
/// auto-injected for this docker_v2 config. Defaults ON; `oci_labels: false`
/// (or any falsy template value) opts out.
pub(crate) fn oci_labels_enabled(
    oci_labels: &Option<anodizer_core::config::StringOrBool>,
    ctx: &Context,
) -> Result<bool> {
    match oci_labels {
        None => Ok(true),
        Some(v) => v
            .try_evaluates_to_true(|s| ctx.render_template(s))
            .with_context(|| "dockers_v2: render oci_labels template"),
    }
}

/// Build the auto-derived standard `org.opencontainers.image.*` labels for one
/// docker_v2 build, scoped to `krate`. Each label is emitted only when its
/// value is actually derivable from project / git / Cargo context — no empty
/// labels, never a wall-clock `created`.
///
/// Per-crate fields (`title`, `version`, `description`, `licenses`, `url`,
/// `documentation`, `vendor`) resolve from THIS crate so workspace per-crate
/// images carry no cross-crate leakage; run-global fields (`created`, `source`,
/// `revision`) are shared across crates in a single release.
pub(crate) fn auto_oci_labels(
    ctx: &Context,
    krate: &anodizer_core::config::CrateConfig,
) -> Vec<(String, String)> {
    const NS: &str = "org.opencontainers.image";
    let mut out: Vec<(String, String)> = Vec::new();
    let mut push = |key: &str, value: Option<String>| {
        if let Some(v) = value.filter(|v| !v.is_empty()) {
            out.push((format!("{NS}.{key}"), v));
        }
    };

    // created — reproducible build date from the resolved SOURCE_DATE_EPOCH
    // (seeded by the build stage). NEVER wall-clock: a non-deterministic
    // `created` would defeat byte-reproducibility. Omitted when no source
    // date is resolvable.
    let created = ctx
        .determinism
        .as_ref()
        .and_then(|d| anodizer_core::sde::rfc3339_utc_from_epoch(d.sde));
    push("created", created);

    // source / revision — the release repo URL and the released commit SHA,
    // shared across every crate in a single release. The OCI `source`
    // annotation is consumed as a browsable URL (registries, `docker scout`),
    // so an SSH remote (`git@host:owner/repo.git`) is normalized to its
    // `https://host/owner/repo` web base; an unrecognised form is passed
    // through unchanged rather than dropped.
    let tvar = |k: &str| {
        ctx.template_vars()
            .get(k)
            .filter(|v| !v.is_empty())
            .cloned()
    };
    let source = tvar("GitURL").map(|u| anodizer_core::git::parse_remote_web_base(&u).unwrap_or(u));
    push("source", source);
    push("revision", tvar("FullCommit"));

    // version — the released version, read from the `Version` template var so
    // the label always matches the rendered image tag. (When the docker stage
    // re-scopes vars per crate, this var is already crate-scoped, so the label
    // follows without divergence.)
    push("version", tvar("Version"));
    // title — always the crate's own name (per-crate, no template I/O).
    push("title", Some(krate.name.clone()));
    push(
        "description",
        ctx.config
            .meta_description_for(&krate.name)
            .map(str::to_string),
    );
    push(
        "licenses",
        ctx.config.meta_license_for(&krate.name).map(str::to_string),
    );
    // url — the project homepage (falls back to repository inside the accessor).
    push(
        "url",
        ctx.config
            .meta_homepage_for(&krate.name)
            .map(str::to_string),
    );
    push(
        "documentation",
        ctx.config
            .meta_documentation_for(&krate.name)
            .map(str::to_string),
    );
    // vendor — the distributing entity; the crate's first author with any
    // `<email>` suffix stripped.
    push("vendor", ctx.config.meta_vendor_for(&krate.name));

    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Merge auto-derived OCI labels with user-rendered labels, where any key the
/// user supplied wins (the auto value never clobbers an explicit user label).
/// Returns a key-sorted vector for deterministic `--label` emission.
pub(crate) fn merge_oci_labels(
    auto: Vec<(String, String)>,
    user: Vec<(String, String)>,
) -> Vec<(String, String)> {
    let user_keys: HashSet<&str> = user.iter().map(|(k, _)| k.as_str()).collect();
    let mut merged: Vec<(String, String)> = auto
        .into_iter()
        .filter(|(k, _)| !user_keys.contains(k.as_str()))
        .collect();
    merged.extend(user);
    merged.sort_by(|a, b| a.0.cmp(&b.0));
    merged
}

/// Render a list of template strings, dropping any that render empty.
pub(crate) fn render_v2_flag_list(
    ctx: &mut Context,
    flags: Option<&Vec<String>>,
) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    if let Some(flag_list) = flags {
        for flag_tmpl in flag_list {
            let rendered = ctx
                .render_template(flag_tmpl)
                .with_context(|| format!("dockers_v2: render flag '{}'", flag_tmpl))?;
            if !rendered.is_empty() {
                out.push(rendered);
            }
        }
    }
    Ok(out)
}
