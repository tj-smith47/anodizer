use super::*;

/// Upload packages to CloudSmith via the CloudSmith API.
///
/// This is a top-level publisher: it reads from `ctx.config.cloudsmiths` rather
/// than from per-crate publish configs.  Each entry specifies an organization,
/// repository, optional credential env var, and optional format/distribution
/// filters.
///
/// Returns the list of [`CloudsmithTarget`]s actually uploaded this run, with
/// the `slug` (Cloudsmith's per-package permanent identifier) populated when
/// the step-3 `packages/upload/<format>/` response surfaced one. The returned
/// list drives `PublishEvidence::extra.cloudsmith_targets` so [`rollback`]
/// can issue real `DELETE /v1/packages/<org>/<repo>/<slug>/` calls; targets
/// whose slug couldn't be parsed degrade to the warn-only manual-cleanup
/// path (see [`cloudsmith_manual_cleanup_msg`]).
///
/// SkipIdempotent matches (artifact already present with matching md5) are
/// NOT included in the return — rollback's semantic is "undo what this run
/// uploaded," and a remote-side hit was put there by an earlier run.
pub(crate) fn publish_to_cloudsmith(
    ctx: &Context,
    log: &StageLogger,
) -> Result<Vec<CloudsmithTarget>> {
    let mut uploaded: Vec<CloudsmithTarget> = Vec::new();
    let entries = match ctx.config.cloudsmiths {
        Some(ref v) if !v.is_empty() => v,
        _ => return Ok(uploaded),
    };

    // Single retry policy resolved from the top-level `retry:` block; reused
    // for every step of the 3-stage upload (files/create → S3 presigned →
    // packages/upload). The retry policy is set
    // once per pipe invocation.
    let policy = ctx.retry_policy();
    let deadline = ctx.retry_deadline();

    for entry in entries {
        // Check skip flag.
        if let Some(ref s) = entry.skip {
            let off = s
                .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                .with_context(|| "cloudsmith: render skip template")?;
            if off {
                log.status("skipped cloudsmith entry — skip evaluates true");
                continue;
            }
        }

        let proceed = anodizer_core::config::evaluate_if_condition(
            entry.if_condition.as_deref(),
            "cloudsmith entry",
            |t| ctx.render_template(t),
        )?;
        if !proceed {
            log.status("skipped cloudsmith entry — `if` condition evaluated falsy");
            continue;
        }

        // Organization is required — bail before dry-run so config errors
        // surface even in dry-run mode.
        let org_raw = match entry.organization.as_deref() {
            Some(o) if !o.is_empty() => o,
            _ => bail!("cloudsmith: 'organization' is required but not set"),
        };

        // Repository is required.
        let repo_raw = match entry.repository.as_deref() {
            Some(r) if !r.is_empty() => r,
            _ => bail!("cloudsmith: 'repository' is required but not set"),
        };

        // Render organization and repository through template engine in case
        // they contain template expressions.
        let organization = ctx
            .render_template(org_raw)
            .with_context(|| format!("cloudsmith: failed to render organization '{}'", org_raw))?;

        let repository = ctx
            .render_template(repo_raw)
            .with_context(|| format!("cloudsmith: failed to render repository '{}'", repo_raw))?;

        // Resolve the secret env-var name (default: CLOUDSMITH_TOKEN).
        let secret_name_rendered =
            crate::util::resolve_secret_name(ctx, entry.secret_name.as_deref(), "CLOUDSMITH_TOKEN");

        // Determine formats filter.
        let formats: Vec<String> = match entry.formats {
            Some(ref f) if !f.is_empty() => f.clone(),
            _ => cloudsmith_default_formats()
                .iter()
                .map(|s| s.to_string())
                .collect(),
        };

        // Resolve distributions map (format -> Vec<distro string>). Each
        // entry yields one or more distribution slugs (the publisher
        // issues one upload per slug). A
        // template-rendering failure on any slug is a config error and
        // hard-bails so a typo doesn't silently route an upload to the
        // wrong distribution.
        let distributions: HashMap<String, Vec<String>> = match entry.distributions {
            Some(ref d) => {
                let mut out: HashMap<String, Vec<String>> = HashMap::new();
                for (k, v) in d {
                    let raw_entries = v.to_str_vec();
                    let mut rendered_entries: Vec<String> = Vec::with_capacity(raw_entries.len());
                    for raw in raw_entries {
                        let rendered = ctx.render_template(raw).with_context(|| {
                            format!(
                                "cloudsmith: render distribution slug '{}' for format '{}'",
                                raw, k
                            )
                        })?;
                        rendered_entries.push(rendered);
                    }
                    out.insert(k.clone(), rendered_entries);
                }
                out
            }
            None => HashMap::new(),
        };

        // Resolve component (optional, used for deb).
        let component = entry
            .component
            .as_ref()
            .map(|c| crate::util::render_or_warn(ctx, log, "cloudsmith.component", c))
            .transpose()?;

        // Check republish flag.
        let republish = match entry.republish.as_ref() {
            Some(r) => r
                .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                .with_context(|| "cloudsmith: render republish template")?,
            None => false,
        };

        // Collect matching artifacts. The `exclude:` glob filter is applied
        // last so the pre-exclude count is available for the eliminated-all
        // warning (a typo'd glob silently dropping every package).
        let id_filtered: Vec<_> = ctx
            .artifacts
            .all()
            .iter()
            .filter(|a| {
                let valid_kind =
                    matches!(a.kind, ArtifactKind::LinuxPackage | ArtifactKind::Archive);
                if !valid_kind {
                    return false;
                }
                if !cloudsmith_format_matches(a.name(), &formats) {
                    return false;
                }
                crate::util::matches_id_filter(a, entry.ids.as_deref())
            })
            .collect();
        let pre_exclude = id_filtered.len();
        let artifacts: Vec<_> = id_filtered
            .into_iter()
            .filter(|a| anodizer_core::artifact::passes_exclude_filter(a, entry.exclude.as_deref()))
            .collect();
        if anodizer_core::artifact::exclude_filter_eliminated_all(
            entry.exclude.as_deref(),
            pre_exclude,
            artifacts.len(),
        ) {
            log.warn(&format!(
                "exclude filter {:?} dropped all {} candidate package(s) for CloudSmith \
                 repo '{}/{}'; check the globs match asset names, not full paths",
                entry.exclude.as_deref().unwrap_or_default(),
                pre_exclude,
                organization,
                repository
            ));
        }

        // --- Dry-run logging ---
        if ctx.is_dry_run() {
            let sample_url =
                cloudsmith_upload_url(&organization, &repository, "{format}", "{distribution}");
            log.status(&format!(
                "(dry-run) would upload packages to CloudSmith org '{}' repo '{}' at {}",
                organization, repository, sample_url
            ));
            log.status(&format!("(dry-run) would filter to formats {:?}", formats));
            if let Some(ref ids) = entry.ids {
                log.status(&format!("(dry-run) would filter to build IDs {:?}", ids));
            }
            if !distributions.is_empty() {
                log.status(&format!(
                    "(dry-run) would publish to distributions {:?}",
                    distributions
                ));
            }
            if let Some(ref comp) = component {
                log.status(&format!("(dry-run) would use component {}", comp));
            }
            if republish {
                log.status("(dry-run) would republish existing versions");
            }
            log.status(&format!(
                "(dry-run) would read credentials from {}",
                secret_name_rendered
            ));
            log.status(&format!("(dry-run) {} artifacts matched", artifacts.len()));
            for a in &artifacts {
                log.status(&format!("(dry-run) {} ({})", a.name(), a.kind));
            }
            continue;
        }

        // --- Live mode ---
        // Resolve token from environment.
        let token = ctx.env_var(&secret_name_rendered).ok_or_else(|| {
            anyhow!(
                "cloudsmith: environment variable '{}' not set (needed for org '{}' repo '{}')",
                secret_name_rendered,
                organization,
                repository
            )
        })?;

        if artifacts.is_empty() {
            log.status(&format!(
                "no matching cloudsmith artifacts for org '{}' repo '{}' (formats: {:?})",
                organization, repository, formats
            ));
            continue;
        }

        let client = anodizer_core::http::blocking_client(std::time::Duration::from_secs(60))
            .context("cloudsmith: failed to build HTTP client")?;

        log.status(&format!(
            "uploading {} packages to cloudsmith org '{}' repo '{}'",
            artifacts.len(),
            organization,
            repository
        ));

        // Distinct CloudSmith package names uploaded under this entry, used to
        // scope post-upload `keep_versions` pruning to each package alone.
        let mut prune_package_names: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        // Per-entry tallies for the single default-verbosity summary line; the
        // per-file upload/skip detail below is verbose-only. `uploaded_count`
        // increments per landed package-create (per distro slug, matching the
        // verbose `uploaded …` lines); `skipped_count` per already-present
        // idempotent skip.
        let mut uploaded_count = 0usize;
        let mut skipped_count = 0usize;

        for artifact in &artifacts {
            let path = &artifact.path;
            if !path.exists() {
                bail!("cloudsmith: artifact file not found: {}", path.display());
            }

            let art_name = artifact.name();
            let fmt = detect_format(art_name);

            // Look up distribution(s) for this format. Cloudsmith accepts an
            // `any-distro/any-version` pseudo-entry for repos that aren't
            // distro-pinned, so an empty list is valid input and treated as
            // "no distribution override". The array form
            // produces one upload per slug.
            //
            // Routing is keyed on the API-side format slug (`apk`/`alpine`,
            // `deb`, `rpm`, `srpm`). The user-facing config key may be
            // either spelling — handle both so a config written against
            // the docs (which use `apk`) and one written against
            // CloudSmith's API path (`alpine`) both work.
            let distro_slugs: Vec<String> = {
                let mut slugs: Vec<String> = distributions.get(fmt).cloned().unwrap_or_default();
                if slugs.is_empty() && fmt == "alpine" {
                    slugs = distributions.get("apk").cloned().unwrap_or_default();
                }
                if slugs.is_empty() && fmt == "srpm" {
                    slugs = distributions.get("src.rpm").cloned().unwrap_or_default();
                }
                slugs
            };

            let file_bytes = std::fs::read(path)
                .with_context(|| format!("cloudsmith: failed to read '{}'", path.display()))?;
            let size_bytes = file_bytes.len();

            // Cloudsmith's files/create API wants a hex-lowercase md5 of
            // the raw bytes.
            let md5_hex = {
                use md5::Digest as _;
                let mut hasher = md5::Md5::new();
                hasher.update(&file_bytes);
                anodizer_core::hashing::hex_lower(&hasher.finalize())
            };

            // Pre-check (republish=false only): query Cloudsmith for an
            // existing package with this filename. If found and md5
            // matches, skip (idempotent). If found but md5 differs,
            // bail — we can't fix the mismatch (the package is immutable
            // on Cloudsmith's side) and silently re-uploading produces
            // duplicate packages with different hashes.
            //
            // The `check_url` / `query` are built unconditionally so the
            // step-3 409-recovery path below can re-issue the same query
            // when an upload races against another concurrent CI loop
            // submitting the same package between pre-check and step-3.
            let api_base = cloudsmith_api_base_from(ctx.env_source());
            let check_url = format!("{}/packages/{}/{}/", api_base, organization, repository);
            let check_query = format!("filename:{}", art_name);
            if !republish {
                match check_cloudsmith_package_exists(
                    &client,
                    &check_url,
                    &check_query,
                    &token,
                    art_name,
                    &md5_hex,
                    &policy,
                    deadline,
                    log,
                )? {
                    CloudsmithPackageState::SkipIdempotent => {
                        // Per-file skip detail is verbose-only; the entry
                        // summary reports the aggregate skip count.
                        log.verbose(&format!(
                            "skipped '{}' — already uploaded with matching md5",
                            art_name
                        ));
                        skipped_count += 1;
                        continue;
                    }
                    CloudsmithPackageState::Md5Mismatch { remote } => {
                        bail!(
                            "cloudsmith: '{}' already exists in org '{}' repo '{}' \
                             with a different md5 (remote={}, local={}). \
                             Re-uploading would create a conflicting duplicate. \
                             Set republish: true to force overwrite.",
                            art_name,
                            organization,
                            repository,
                            remote,
                            md5_hex
                        );
                    }
                    CloudsmithPackageState::Unverifiable => {
                        // Filename present but no remote checksum to compare:
                        // upload rather than skip-and-claim-match. The step-3
                        // 409 path resolves a genuine duplicate.
                        log.verbose(&format!(
                            "'{}' present on cloudsmith but no remote md5 to verify; uploading (idempotency unconfirmed)",
                            art_name
                        ));
                    }
                    CloudsmithPackageState::NotFound => {}
                }
            }

            // Iterate at least once even when no distributions are
            // configured. For formats CloudSmith requires a distribution on
            // (`deb`, `alpine`), fall back to the accept-all catch-all slug so
            // the package still indexes and stays installable; an empty slug
            // for those would land the bytes unindexed. Formats that don't
            // require a distribution (`rpm`/`srpm`/`raw`) keep the
            // empty-slug "no override" behaviour.
            let upload_slugs: Vec<String> = if distro_slugs.is_empty() {
                match cloudsmith_default_distribution(fmt) {
                    Some(default_distro) => {
                        log.verbose(&format!(
                            "no distribution configured for '{}' ({}); defaulting to '{}' so it indexes (set `distributions.{}` to pin a real distro)",
                            art_name, fmt, default_distro, fmt
                        ));
                        vec![default_distro.to_string()]
                    }
                    None => vec![String::new()],
                }
            } else {
                distro_slugs.clone()
            };

            // Per-file upload detail is verbose-only; the entry summary
            // reports the aggregate upload count at default verbosity.
            log.verbose(&format!(
                "uploading {} ({}, {} bytes, md5={}) → org '{}' repo '{}'{}",
                art_name,
                fmt,
                size_bytes,
                md5_hex,
                organization,
                repository,
                if distro_slugs.is_empty() {
                    String::new()
                } else {
                    format!(" distros={:?}", distro_slugs)
                },
            ));

            // --- Step 3/3 prep: package-create URL + component gating ---
            //
            // POST /v1/packages/{org}/{repo}/upload/{format}/ with the
            // identifier + distribution tells Cloudsmith to take the
            // uploaded raw file and register it as a deb/rpm/alpine
            // package. Without this step the bytes are dangling.
            //
            // When multiple distributions are configured
            // array form), step 3 is issued once per slug — CloudSmith's
            // API accepts only one `distribution` per call. Each
            // files/create slot (`identifier`) is consumed by a single
            // package-create, so the file stage (steps 1+2) runs once PER
            // distribution inside the loop — reusing one identifier across
            // distributions 4xx's on the 2nd+ call (the slot is spent).
            let package_upload_url = format!(
                "{}/packages/{}/{}/upload/{}/",
                api_base, organization, repository, fmt
            );
            let component_for_format = component
                .as_ref()
                .filter(|_| COMPONENT_BEARING_FORMATS.contains(&fmt));
            if component.is_some() && component_for_format.is_none() {
                log.verbose(&format!(
                    "cloudsmith component is set but format '{}' does not accept a component; dropping",
                    fmt
                ));
            }

            for distro in &upload_slugs {
                // Stage a fresh files/create slot + presigned upload for THIS
                // distribution. The identifier is single-use, so every
                // distribution needs its own.
                let identifier = stage_cloudsmith_file(
                    &client,
                    &api_base,
                    &organization,
                    &repository,
                    art_name,
                    &md5_hex,
                    &file_bytes,
                    &token,
                    &policy,
                    deadline,
                    log,
                )?;

                let mut package_body = serde_json::json!({
                    "package_file": identifier,
                });
                if !distro.is_empty() {
                    package_body["distribution"] = serde_json::Value::String(distro.clone());
                }
                if let Some(comp) = component_for_format {
                    package_body["component"] = serde_json::Value::String(comp.clone());
                }
                if republish {
                    package_body["republish"] = serde_json::Value::Bool(true);
                }

                log.verbose(&format!(
                    "POST {} (identifier={}, distro={:?}, step 3 of 3)",
                    package_upload_url, identifier, distro
                ));
                let label = format!("packages/upload/{}", fmt);
                let step3_result = retry_request(&label, art_name, &policy, deadline, log, || {
                    client
                        .post(&package_upload_url)
                        .header("Authorization", format!("token {}", token))
                        .header("Accept", "application/json")
                        .json(&package_body)
                        .send()
                });

                let (pkg_status, pkg_body) = match step3_result {
                    Ok(pair) => pair,
                    Err(err) => {
                        // Race-recovery: a concurrent CI loop can submit the
                        // same name+version between our pre-check (or
                        // first-attempt step-3) and this step-3, returning
                        // 409/422 here. Without recovery, the upload aborts
                        // even though the operator's intent — "land this
                        // artifact on the registry" — was satisfied by the
                        // racing process. Re-query the remote: if it now
                        // exists with our md5, treat as idempotent skip; if
                        // it exists with a different md5, surface the same
                        // conflict the pre-check would have. Anything else
                        // (transport failure, 5xx after retries) propagates.
                        let status_in_chain: Option<u16> = err.chain().find_map(|e| {
                            e.downcast_ref::<anodizer_core::retry::HttpError>()
                                .map(|h| h.status)
                        });
                        let is_conflict = matches!(status_in_chain, Some(409) | Some(422));
                        if !is_conflict {
                            return Err(err);
                        }
                        log.warn(&format!(
                            "cloudsmith step-3 returned {:?} for '{}'; re-checking remote to \
                             decide between idempotent skip and real conflict",
                            status_in_chain, art_name
                        ));
                        match check_cloudsmith_package_exists(
                            &client,
                            &check_url,
                            &check_query,
                            &token,
                            art_name,
                            &md5_hex,
                            &policy,
                            deadline,
                            log,
                        )? {
                            CloudsmithPackageState::SkipIdempotent => {
                                let msg = format!(
                                    "'{}' already landed on cloudsmith with matching md5 \
                                     (concurrent uploader); treating as idempotent skip",
                                    art_name
                                );
                                if republish {
                                    // A racing uploader landing the same bytes
                                    // while republish was requested is a real
                                    // surprise worth surfacing at default
                                    // verbosity.
                                    log.warn(&msg);
                                } else {
                                    log.verbose(&msg);
                                }
                                skipped_count += 1;
                                continue;
                            }
                            CloudsmithPackageState::Md5Mismatch { remote } => {
                                bail!(
                                    "cloudsmith: step-3 conflict for '{}' in org '{}' repo \
                                     '{}'; remote md5={} differs from local={}. A concurrent \
                                     upload submitted different bytes under the same name. \
                                     Set republish: true to force overwrite, or bump the \
                                     release.",
                                    art_name,
                                    organization,
                                    repository,
                                    remote,
                                    md5_hex
                                );
                            }
                            CloudsmithPackageState::Unverifiable => {
                                // The re-query shows the filename present but
                                // with no checksum to compare — cannot confirm
                                // the racing upload landed OUR bytes. Surface
                                // the conflict rather than claim an idempotent
                                // skip we can't prove.
                                log.warn(&format!(
                                    "cloudsmith: step-3 conflict for '{}'; remote reports no md5 to \
                                     verify the landed bytes match local — surfacing the conflict \
                                     instead of assuming an idempotent skip",
                                    art_name
                                ));
                                return Err(err);
                            }
                            CloudsmithPackageState::NotFound => {
                                return Err(err);
                            }
                        }
                    }
                };

                let pkg_json = serde_json::from_str::<serde_json::Value>(&pkg_body).ok();
                let slug = pkg_json.as_ref().and_then(|v| {
                    v.get("slug_perm")
                        .or_else(|| v.get("slug"))
                        .and_then(|s| s.as_str())
                        .map(|s| s.to_string())
                });
                // Capture the CloudSmith package `name` so post-upload
                // `keep_versions` pruning can scope its list+delete to this
                // package alone (not siblings sharing the repo).
                if let Some(name) = pkg_json
                    .as_ref()
                    .and_then(|v| v.get("name"))
                    .and_then(|n| n.as_str())
                    && !name.is_empty()
                {
                    prune_package_names.insert(name.to_string());
                }
                // Per-file upload-success detail is verbose-only; the entry
                // summary reports the aggregate upload count.
                if let Some(ref s) = slug {
                    log.verbose(&format!(
                        "uploaded {} (slug={}{})",
                        art_name,
                        s,
                        if distro.is_empty() {
                            String::new()
                        } else {
                            format!(", distro={}", distro)
                        }
                    ));
                } else {
                    log.verbose(&format!("uploaded {} (HTTP {})", art_name, pkg_status));
                }
                uploaded_count += 1;
                uploaded.push(CloudsmithTarget {
                    org: organization.clone(),
                    repo: repository.clone(),
                    filename: art_name.to_string(),
                    slug,
                });
            }
        }

        log.status(&cloudsmith_upload_summary(
            uploaded_count,
            skipped_count,
            &organization,
            &repository,
        ));

        // --- Post-upload retention pruning (keep_versions) ---
        //
        // The upload (the real work) has already succeeded. Pruning is a
        // best-effort follow-up: a list/delete failure warns and continues —
        // it must NOT fail the stage or roll back the upload. `keep == 0` is
        // refused; unset (None) prunes nothing.
        if let Some(keep) = entry.keep_versions {
            if ctx.is_snapshot() {
                // Snapshot publishes are blocked by the shared non-release
                // version guard, but guard the destructive prune independently
                // so it can never delete real releases on behalf of a snapshot run.
                log.verbose("skipped cloudsmith keep_versions prune — snapshot mode");
            } else if keep == 0 {
                log.warn(
                    "skipped cloudsmith keep_versions prune — 0 is invalid (would prune every version)",
                );
            } else if ctx.version().is_empty() {
                // Without a known current version, the pure selector loses its
                // "always keep the just-uploaded version" safety net (an empty
                // version normalizes to "" and matches no bucket), so a ranking
                // quirk could delete what this run just uploaded. Refuse rather
                // than prune blind.
                log.warn(
                    "skipped cloudsmith keep_versions prune — current version is unknown (avoids deleting the just-uploaded release)",
                );
            } else {
                let current_version = ctx.version();
                for pkg_name in &prune_package_names {
                    prune_cloudsmith_versions(
                        &client,
                        &cloudsmith_api_base_from(ctx.env_source()),
                        &organization,
                        &repository,
                        pkg_name,
                        &current_version,
                        keep,
                        &token,
                        &policy,
                        deadline,
                        log,
                    );
                }
            }
        }
    }

    Ok(uploaded)
}
