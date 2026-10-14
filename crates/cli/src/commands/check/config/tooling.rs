use super::*;

/// Environment / tool availability checks (warnings only). Probes
/// cross-compilation tools, docker + buildx, the release forge's token, nfpm, and
/// the signing binaries.
pub(super) fn check_environment(config: &Config, warnings: &mut Vec<String>) {
    check_cross_tooling(config, warnings);
    check_docker_tooling(config, warnings);
    check_forge_token(config, &anodizer_core::ProcessEnvSource, warnings);
    check_nfpm_tool(config, warnings);
    // Blob storage cloud credentials are validated at upload time by the
    // SDK — object_store handles S3/GCS/Azure natively. Config correctness
    // (provider, bucket) is already validated in check_blob_configs.
    check_signing_tools(config, warnings);
}

pub(super) fn check_cross_tooling(config: &Config, warnings: &mut Vec<String>) {
    let universe = config.crate_universe();
    let needs_cross = universe.iter().any(|c| {
        use anodizer_core::config::CrossStrategy;
        matches!(
            &c.cross,
            Some(CrossStrategy::Zigbuild) | Some(CrossStrategy::Auto)
        ) || config
            .defaults
            .as_ref()
            .and_then(|d| d.cross.as_ref())
            .is_some_and(|cs| matches!(cs, CrossStrategy::Zigbuild | CrossStrategy::Auto))
    });

    if needs_cross || universe.iter().any(|c| c.builds.is_some()) {
        if !anodizer_core::tool_detect::on_path("cargo-zigbuild") {
            warnings.push(
                "cargo-zigbuild is not installed (needed for cross-compilation via zigbuild)"
                    .to_string(),
            );
        }
        if !anodizer_core::tool_detect::on_path("cross") {
            warnings.push(
                "cross is not installed (needed for cross-compilation via cross)".to_string(),
            );
        }
    }
}

pub(super) fn check_docker_tooling(config: &Config, warnings: &mut Vec<String>) {
    if !config_needs_docker(config) {
        return;
    }
    if !anodizer_core::tool_detect::on_path("docker") {
        warnings.push("docker is not installed but docker sections are configured".to_string());
        return;
    }
    // The buildx probe reports three states:
    //   Ok(true)  → buildx subcommand exists.
    //   Ok(false) → docker present but buildx subcommand missing.
    //   Err(_)    → spawn failed (typically: docker disappeared between
    //               the on_path check above and now, e.g. PATH race).
    //               Collapse Err to "buildx unavailable" and trace-log the
    //               io::Error so verbose runs can see the underlying cause.
    let buildx_ok = match anodizer_core::docker_detect::buildx_available() {
        Ok(b) => b,
        Err(e) => {
            tracing::trace!(error = %e, "buildx probe failed");
            false
        }
    };
    if !buildx_ok {
        warnings
            .push("docker buildx is not available but docker sections are configured".to_string());
    }
}

/// Warn when release sections are configured and the forge the run would
/// release to has no token in any source the run reads: its variables, then
/// its token file or env-file line. A blank variable counts as unset. A file
/// that cannot be read is reported by the release itself; here it counts as
/// no token.
pub(super) fn check_forge_token<E: anodizer_core::env_source::EnvSource + ?Sized>(
    config: &Config,
    env: &E,
    warnings: &mut Vec<String>,
) {
    if !config_needs_release(config) {
        return;
    }
    let file_tokens = anodizer_core::config::token_file_tokens(config.env_files.as_ref(), env)
        .unwrap_or_default();
    let forge = crate::commands::helpers::detect_forge(config, env, &file_tokens);
    let vars = anodizer_core::config::forge_token_vars(forge);
    let has_var = vars.iter().any(|name| {
        env.var(name)
            .as_deref()
            .and_then(anodizer_core::git::non_blank_token)
            .is_some()
    });
    if has_var || file_tokens.contains_key(anodizer_core::config::forge_file_var(forge)) {
        return;
    }
    let vars = vars.join(" or ");
    let hint = match anodizer_core::config::token_file_clause(config.env_files.as_ref(), forge) {
        Some(file) => format!("set {vars} or {file}"),
        None => format!("set {vars}"),
    };
    warnings.push(format!(
        "no {} token found but release sections are configured; {hint}",
        forge.name()
    ));
}

pub(super) fn check_nfpm_tool(config: &Config, warnings: &mut Vec<String>) {
    if config_needs_nfpm(config) && !anodizer_core::tool_detect::on_path("nfpm") {
        warnings.push("nfpm is not installed but nfpm sections are configured".to_string());
    }
}

/// `true` when any crate in the universe configures `dockers_v2` — docker +
/// buildx are then release-time requirements.
pub(super) fn config_needs_docker(config: &Config) -> bool {
    config
        .crate_universe()
        .iter()
        .any(|c| c.dockers_v2.is_some())
}

/// `true` when any crate in the universe configures a `release:` block — a
/// forge token is then a release-time requirement.
pub(super) fn config_needs_release(config: &Config) -> bool {
    config.crate_universe().iter().any(|c| c.release.is_some())
}

/// `true` when any crate in the universe configures `nfpms:` — the nfpm
/// binary is then a release-time requirement.
pub(super) fn config_needs_nfpm(config: &Config) -> bool {
    config.crate_universe().iter().any(|c| c.nfpms.is_some())
}

pub(super) fn check_signing_tools(config: &Config, warnings: &mut Vec<String>) {
    if !config.signs.is_empty() {
        for sign_cfg in &config.signs {
            let sign_cmd = sign_cfg.cmd.as_deref().unwrap_or("gpg");
            if !anodizer_core::tool_detect::on_path(sign_cmd) {
                warnings.push(format!(
                    "'{}' is not installed but signs section is configured",
                    sign_cmd
                ));
            }
        }
    }
    if let Some(docker_signs) = &config.docker_signs {
        for ds in docker_signs {
            let cmd = ds.cmd.as_deref().unwrap_or("cosign");
            if !anodizer_core::tool_detect::on_path(cmd) {
                warnings.push(format!(
                    "'{}' is not installed but docker_signs section is configured",
                    cmd
                ));
            }
        }
    }
}
