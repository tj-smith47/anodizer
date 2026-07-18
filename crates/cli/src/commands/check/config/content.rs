use super::*;

/// Warn on unrecognized target triples in `defaults.targets` and per-build
/// `targets`.
pub(super) fn check_target_triples(config: &Config, warnings: &mut Vec<String>) {
    let known_prefixes = [
        "x86_64",
        "aarch64",
        "i686",
        "armv7",
        "arm",
        "riscv64gc",
        "s390x",
        "powerpc64le",
    ];
    let known_os = [
        "linux", "darwin", "apple", "windows", "freebsd", "netbsd", "android",
    ];
    let mut check_triple = |triple: &str, context: &str| {
        let parts: Vec<&str> = triple.split('-').collect();
        let arch_ok = parts
            .first()
            .is_some_and(|a| known_prefixes.iter().any(|p| a.starts_with(p)));
        let os_ok = known_os.iter().any(|os| triple.contains(os));
        if !arch_ok || !os_ok {
            warnings.push(format!(
                "unrecognized target triple '{}' in {}",
                triple, context
            ));
        }
    };
    if let Some(defaults) = &config.defaults
        && let Some(targets) = &defaults.targets
    {
        for t in targets {
            check_triple(t, "defaults.targets");
        }
    }
    for c in config.crate_universe() {
        if let Some(builds) = &c.builds {
            for b in builds {
                if let Some(targets) = &b.targets {
                    let bin = b.binary.as_deref().unwrap_or(c.name.as_str());
                    for t in targets {
                        check_triple(t, &format!("crate '{}' build '{}'", c.name, bin));
                    }
                }
            }
        }
    }
}

/// Warn when changelog `skip:true` coexists with other configured fields,
/// and when `use:` has an unrecognized value.
pub(super) fn check_changelog(config: &Config, warnings: &mut Vec<String>) {
    if let Some(cl) = &config.changelog
        && cl.skip == Some(anodizer_core::config::StringOrBool::Bool(true))
    {
        let has_other = cl.sort.is_some()
            || cl.filters.is_some()
            || cl.groups.is_some()
            || cl.header.is_some()
            || cl.footer.is_some()
            || cl.use_source.is_some()
            || cl.abbrev.is_some();
        if has_other {
            warnings.push(
                "changelog.skip is true but other changelog fields are also set (they will be ignored)".to_string(),
            );
        }
    }

    if let Some(cl) = &config.changelog
        && let Some(ref use_source) = cl.use_source
        && use_source != "git"
        && use_source != "github-native"
    {
        warnings.push(format!(
            "unrecognized changelog 'use' value '{}' (valid: git, github-native)",
            use_source
        ));
    }
}

/// Warn when a recipient-visible announce template references a secret-named
/// `Env` variable (e.g. `{{ Env.GITHUB_TOKEN }}`).
///
/// Outbound redaction masks any secret-named env value before it reaches a
/// recipient (sent as the literal `$NAME`), so embedding such a reference in
/// the message/title/body a reader will see is almost always an authoring
/// mistake. Only content fields are scanned — routing fields (webhook URLs,
/// bot tokens, channel IDs, SMTP credentials) legitimately carry secrets and
/// are skipped to avoid noise. `reddit.url_template` is treated as content
/// because a token-named reference in a public link is a leak.
pub(super) fn check_announce_secret_exposure(config: &Config, warnings: &mut Vec<String>) {
    let Some(announce) = &config.announce else {
        return;
    };

    let scan = |field: &str, value: &Option<String>, warnings: &mut Vec<String>| {
        if let Some(text) = value {
            warn_secret_env_refs(field, text, warnings);
        }
    };

    if let Some(b) = &announce.bluesky {
        scan(
            "announce.bluesky.message_template",
            &b.message_template,
            warnings,
        );
    }
    if let Some(d) = &announce.discourse {
        scan(
            "announce.discourse.title_template",
            &d.title_template,
            warnings,
        );
        scan(
            "announce.discourse.message_template",
            &d.message_template,
            warnings,
        );
    }
    if let Some(l) = &announce.linkedin {
        scan(
            "announce.linkedin.message_template",
            &l.message_template,
            warnings,
        );
    }
    if let Some(o) = &announce.opencollective {
        scan(
            "announce.opencollective.title_template",
            &o.title_template,
            warnings,
        );
        scan(
            "announce.opencollective.message_template",
            &o.message_template,
            warnings,
        );
    }
    if let Some(t) = &announce.twitter {
        scan(
            "announce.twitter.message_template",
            &t.message_template,
            warnings,
        );
    }
    if let Some(m) = &announce.mastodon {
        scan(
            "announce.mastodon.message_template",
            &m.message_template,
            warnings,
        );
    }
    if let Some(d) = &announce.discord {
        scan(
            "announce.discord.message_template",
            &d.message_template,
            warnings,
        );
        scan("announce.discord.author", &d.author, warnings);
    }
    if let Some(w) = &announce.webhook {
        scan(
            "announce.webhook.message_template",
            &w.message_template,
            warnings,
        );
    }
    if let Some(t) = &announce.telegram {
        scan(
            "announce.telegram.message_template",
            &t.message_template,
            warnings,
        );
    }
    if let Some(t) = &announce.teams {
        scan(
            "announce.teams.message_template",
            &t.message_template,
            warnings,
        );
        scan("announce.teams.title_template", &t.title_template, warnings);
    }
    if let Some(m) = &announce.mattermost {
        scan(
            "announce.mattermost.message_template",
            &m.message_template,
            warnings,
        );
        scan(
            "announce.mattermost.title_template",
            &m.title_template,
            warnings,
        );
    }
    if let Some(e) = &announce.email {
        scan(
            "announce.email.subject_template",
            &e.subject_template,
            warnings,
        );
        scan(
            "announce.email.message_template",
            &e.message_template,
            warnings,
        );
    }
    if let Some(r) = &announce.reddit {
        scan(
            "announce.reddit.title_template",
            &r.title_template,
            warnings,
        );
        scan("announce.reddit.url_template", &r.url_template, warnings);
    }
    if let Some(s) = &announce.slack {
        scan(
            "announce.slack.message_template",
            &s.message_template,
            warnings,
        );
        if let Some(blocks) = &s.blocks {
            for (i, block) in blocks.iter().enumerate() {
                if let Some(text) = &block.text {
                    warn_secret_env_refs(
                        &format!("announce.slack.blocks[{}].text", i),
                        &text.text,
                        warnings,
                    );
                }
            }
        }
        if let Some(attachments) = &s.attachments {
            for (i, att) in attachments.iter().enumerate() {
                let prefix = format!("announce.slack.attachments[{}]", i);
                scan(&format!("{}.text", prefix), &att.text, warnings);
                scan(&format!("{}.title", prefix), &att.title, warnings);
                scan(&format!("{}.fallback", prefix), &att.fallback, warnings);
                scan(&format!("{}.pretext", prefix), &att.pretext, warnings);
                scan(&format!("{}.footer", prefix), &att.footer, warnings);
            }
        }
    }
}

/// Push a warning for every `Env.<NAME>` reference inside a render block of
/// `text` whose `NAME` looks like a secret.
///
/// Only refs inside a `{{ ... }}` expression or a `{% ... %}` statement are
/// considered — bare prose like `set Env.GITHUB_TOKEN first` never renders
/// under Tera, so it cannot leak and must not warn. Each block span is
/// scanned independently with [`anodizer_core::template::ENV_REF_PATTERN`], so
/// multiple refs in one block (e.g. `{{ Env.A | default(Env.B_TOKEN) }}`) are
/// all caught. Both Tera (`Env.X`) and Go-style (`.Env.X`) forms match — the
/// capture starts after the dot, so a leading `.` is irrelevant.
pub(super) fn warn_secret_env_refs(field: &str, text: &str, warnings: &mut Vec<String>) {
    static BLOCK_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        // Non-greedy inner captures so adjacent blocks stay separate spans.
        anodizer_core::util::static_regex(r"(?s)\{\{(.*?)\}\}|\{%(.*?)%\}")
    });
    static ENV_REF: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        anodizer_core::util::static_regex(anodizer_core::template::ENV_REF_PATTERN)
    });
    for block in BLOCK_RE.captures_iter(text) {
        // Exactly one alternation arm matches per block; take whichever did.
        let inner = block
            .get(1)
            .or_else(|| block.get(2))
            .map(|m| m.as_str())
            .unwrap_or("");
        for cap in ENV_REF.captures_iter(inner) {
            let name = &cap[1];
            let upper = name.to_uppercase();
            if anodizer_core::redact::SECRET_KEY_SUFFIXES
                .iter()
                .any(|suffix| upper.ends_with(suffix))
            {
                warnings.push(format!(
                    "{field} references secret-named var Env.{name}; its value is masked by outbound redaction (sent as \"${name}\"), so embedding it here is almost certainly a mistake — remove the reference"
                ));
            }
        }
    }
}

/// Warn when checksum `skip:true` coexists with other configured fields
/// (both `defaults.checksum` and per-crate `checksum`).
pub(super) fn check_checksum_skip_conflicts(config: &Config, warnings: &mut Vec<String>) {
    if let Some(defaults) = &config.defaults
        && let Some(cksum) = &defaults.checksum
        && cksum.skip.as_ref().is_some_and(|d| d.as_bool())
    {
        let has_other = cksum.algorithm.is_some()
            || cksum.name_template.is_some()
            || cksum.extra_files.is_some()
            || cksum.ids.is_some();
        if has_other {
            warnings.push(
                "defaults.checksum.skip is true but other checksum fields are also set (they will be ignored)".to_string(),
            );
        }
    }

    for c in config.crate_universe() {
        if let Some(cksum) = &c.checksum
            && cksum.skip.as_ref().is_some_and(|d| d.as_bool())
        {
            let has_other = cksum.algorithm.is_some()
                || cksum.name_template.is_some()
                || cksum.extra_files.is_some()
                || cksum.ids.is_some();
            if has_other {
                warnings.push(format!(
                    "checksum skip is true for crate '{}' but other checksum fields are also set (they will be ignored)",
                    c.name,
                ));
            }
        }
    }
}

/// Warn on unrecognized sign artifact filter values.
///
/// The accepted vocabulary is the runtime resolver's own
/// `VALID_SIGN_ARTIFACT_FILTERS` (the source of truth for
/// `should_sign_artifact`), so check-time validation cannot drift behind a
/// value the sign stage actually honors.
pub(super) fn check_sign_artifact_filters(config: &Config, warnings: &mut Vec<String>) {
    let valid_artifact_filters = anodizer_stage_sign::VALID_SIGN_ARTIFACT_FILTERS;
    for sign_cfg in &config.signs {
        if let Some(ref filter) = sign_cfg.artifacts
            && !valid_artifact_filters.contains(&filter.as_str())
        {
            warnings.push(format!(
                "unrecognized signs artifacts filter '{}' (valid: {})",
                filter,
                valid_artifact_filters.join(", ")
            ));
        }
        // The authenticode block carries its own `artifacts` selector, resolved
        // through the same `should_sign_artifact` vocabulary. An unrecognized
        // value here matches no artifact and (now that the stage propagates the
        // error) fails the run — surface it at check time too.
        if let Some(ref auth) = sign_cfg.authenticode
            && let Some(ref filter) = auth.artifacts
            && !valid_artifact_filters.contains(&filter.as_str())
        {
            warnings.push(format!(
                "unrecognized signs authenticode artifacts filter '{}' (valid: {})",
                filter,
                valid_artifact_filters.join(", ")
            ));
        }
    }
}

/// Warn on unrecognized checksum algorithm values in `defaults.checksum`
/// and per-crate `checksum`.
pub(super) fn check_checksum_algorithms(config: &Config, warnings: &mut Vec<String>) {
    let valid_algorithms = [
        "sha1", "sha224", "sha256", "sha384", "sha512", "blake2b", "blake2s",
    ];
    if let Some(defaults) = &config.defaults
        && let Some(cksum) = &defaults.checksum
        && let Some(ref algo) = cksum.algorithm
        && !valid_algorithms.contains(&algo.as_str())
    {
        warnings.push(format!(
            "unrecognized defaults.checksum algorithm '{}' (valid: {})",
            algo,
            valid_algorithms.join(", ")
        ));
    }
    for c in config.crate_universe() {
        if let Some(cksum) = &c.checksum
            && let Some(ref algo) = cksum.algorithm
            && !valid_algorithms.contains(&algo.as_str())
        {
            warnings.push(format!(
                "unrecognized checksum algorithm '{1}' for crate '{0}' (valid: {2})",
                c.name,
                algo,
                valid_algorithms.join(", ")
            ));
        }
    }
}

/// `source.format` must be one of the supported archive formats.
pub(super) fn check_source_format(config: &Config, errors: &mut Vec<String>) {
    if let Some(ref source) = config.source
        && let Some(ref fmt) = source.format
    {
        let valid_source_formats = ["tar.gz", "tgz", "tar", "zip"];
        if !valid_source_formats.contains(&fmt.as_str()) {
            errors.push(format!(
                "source: unrecognized format '{}' (valid: {})",
                fmt,
                valid_source_formats.join(", ")
            ));
        }
    }
}

/// SBOM `artifacts` values must be from the allow-list.
pub(super) fn check_sbom_configs(config: &Config, errors: &mut Vec<String>) {
    for (i, sbom) in config.sboms.iter().enumerate() {
        let idx_str = i.to_string();
        let label = sbom
            .id
            .as_deref()
            .unwrap_or_else(|| if i == 0 { "default" } else { &idx_str });
        if let Some(ref artifacts) = sbom.artifacts {
            let valid = [
                "source",
                "archive",
                "binary",
                "package",
                "diskimage",
                "installer",
                "any",
            ];
            if !valid.contains(&artifacts.as_str()) {
                errors.push(format!(
                    "sboms[{}]: invalid artifacts type '{}' (valid: {})",
                    label,
                    artifacts,
                    valid.join(", ")
                ));
            }
        }
    }
}

/// Per-crate blob entries require a recognized `provider` and a non-empty
/// `bucket`.
pub(super) fn check_blob_configs(config: &Config, errors: &mut Vec<String>) {
    let valid_blob_providers = ["s3", "gs", "gcs", "azblob", "azure"];
    for c in config.crate_universe() {
        if let Some(ref blobs) = c.blobs {
            for (i, blob) in blobs.iter().enumerate() {
                let idx = i.to_string();
                let label = blob.id.as_deref().unwrap_or(&idx);
                if blob.provider.is_empty() {
                    errors.push(format!(
                        "crate '{}' blobs[{}]: provider is required",
                        c.name, label
                    ));
                } else if !valid_blob_providers.contains(&blob.provider.as_str()) {
                    errors.push(format!(
                        "crate '{}' blobs[{}]: unrecognized provider '{}' (valid: {})",
                        c.name,
                        label,
                        blob.provider,
                        valid_blob_providers.join(", ")
                    ));
                }
                if blob.bucket.is_empty() {
                    errors.push(format!(
                        "crate '{}' blobs[{}]: bucket is required",
                        c.name, label
                    ));
                }
            }
        }
    }
}
