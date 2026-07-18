use std::path::PathBuf;

use anodizer_core::PublishEvidence;
use anodizer_core::config::{SchemaEntry, SchemaMode, SchemastoreConfig};
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::Context as _;
use serde_json::Value;

use super::super::manifest::{self, Dialect};
use super::super::{catalog, entry_label};

use super::*;

/// Run the SchemaStore publish, returning evidence of what was registered.
pub(crate) fn run_publish(ctx: &mut Context) -> anyhow::Result<PublishEvidence> {
    let log = ctx.logger("publish");
    // Clone the config out of `ctx` so the per-schema body can re-borrow `ctx`
    // mutably (version re-scoping) without aliasing `ctx.config`.
    let cfg = ctx.config.schemastore.clone();

    let effective = effective_schemas(ctx, &cfg, &log)?;
    if effective.is_empty() {
        log.status("no schemas to register (all skipped or none configured)");
        return Ok(PublishEvidence::new("schemastore"));
    }

    if ctx.is_dry_run() || ctx.is_snapshot() {
        return plan_dry_run(ctx, &cfg, &effective, &log);
    }

    // Steady-state fast path: probe the upstream catalog (one HTTP GET, plus a
    // GET per vendor file) BEFORE cloning. When every schema is a CERTAIN no-op
    // the publish needs no fork, no clone, and no token — anodizer's own
    // external entry is a no-op every release. The probe is best-effort: any
    // fetch failure or uncertainty falls through to the authoritative
    // `run_real`, which re-derives from the synced clone.
    if probe_remote_all_noop(ctx, &cfg, &effective, &log) {
        log.status(&format!(
            "all {} schemastore schema(s) already current upstream — nothing to publish (no clone)",
            effective.len()
        ));
        return Ok(PublishEvidence::new("schemastore"));
    }

    run_real(ctx, &cfg, &effective, &log)
}

/// Best-effort pre-clone probe: GET the upstream catalog (and, per vendor
/// schema, the upstream vendored file and — for a too-high dialect — the
/// `schema-validation.jsonc` allowlist) and return `true` only when EVERY
/// effective schema is a CERTAIN no-op via the shared [`schema_change_needed`].
///
/// The decision logic is the same pure fn `run_real` uses, so the probe can
/// never disagree with the authoritative path. Conservatism is total: any
/// HTTP error, non-success status the caller can't interpret as "absent", or
/// unexpected failure returns `false` (fall through to the clone). A `true`
/// result therefore means every required piece was fetched and matched — never
/// an assumption on missing data.
pub(super) fn probe_remote_all_noop(
    ctx: &mut Context,
    cfg: &SchemastoreConfig,
    effective: &[(&SchemaEntry, String)],
    log: &StageLogger,
) -> bool {
    match probe_remote_all_noop_inner(ctx, cfg, effective) {
        Ok(all_noop) => all_noop,
        Err(e) => {
            // Never abort the release on a probe failure; fall through to the
            // clone, which is the source of truth.
            log.status(&format!(
                "schemastore pre-clone catalog probe skipped ({e}); proceeding to clone"
            ));
            false
        }
    }
}

/// Fallible core of [`probe_remote_all_noop`]. Returns `Ok(true)` only when the
/// catalog GET succeeded and every schema is a certain no-op; `Ok(false)` when
/// any schema needs a change; `Err` when the catalog could not be fetched (the
/// wrapper turns that into a conservative fall-through).
fn probe_remote_all_noop_inner(
    ctx: &mut Context,
    cfg: &SchemastoreConfig,
    effective: &[(&SchemaEntry, String)],
) -> anyhow::Result<bool> {
    let client = anodizer_core::http::blocking_client(std::time::Duration::from_secs(30))?;
    let raw_base = format!(
        "https://raw.githubusercontent.com/{UPSTREAM_OWNER}/{UPSTREAM_REPO}/{UPSTREAM_DEFAULT_BRANCH}"
    );
    let catalog_url = format!("{raw_base}/{CATALOG_PATH}");
    let catalog_json = fetch_raw_required(&client, &catalog_url)?;

    let project_root = ctx
        .options
        .project_root
        .clone()
        .unwrap_or_else(|| PathBuf::from("."));

    for (entry, description) in effective {
        let plan = plan_schema_scoped(ctx, cfg, entry, description, Some(&catalog_json))?;

        // Vendor: format the LOCAL file and fetch the upstream copy + (for a
        // too-high dialect) the allowlist. External: no file.
        let (local_schema, vendor_file, jsonc) = if plan.mode == SchemaMode::Vendor {
            let local = read_local_vendor_schema(&project_root, entry)?;
            let vendor_url = match plan.vendor_path.as_ref() {
                Some(rel) => format!("{raw_base}/{}", rel.display()),
                None => return Ok(false),
            };
            // A 404 ⇒ file absent upstream ⇒ Add ⇒ change-needed; a transport
            // error is uncertainty ⇒ change-needed. Both are `None`, which the
            // decision reads as change-needed.
            let vendor_file = fetch_raw_optional(&client, &vendor_url)?;
            let jsonc = if raw_dialect(&local) == Dialect::TooHigh {
                let jsonc_url = format!("{raw_base}/{DIALECT_ALLOWLIST_PATH}");
                fetch_raw_optional(&client, &jsonc_url)?
            } else {
                None
            };
            (Some(local), vendor_file, jsonc)
        } else {
            (None, None, None)
        };

        let remote = RemoteState {
            catalog_json: &catalog_json,
            vendor_file: vendor_file.as_deref(),
            jsonc: jsonc.as_deref(),
        };
        if schema_change_needed(&plan, local_schema.as_deref(), &remote) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// GET a raw.githubusercontent.com URL whose presence is REQUIRED for the probe
/// to proceed. A non-success status or transport error is an error (the probe
/// wrapper falls through to the clone).
pub(super) fn fetch_raw_required(
    client: &reqwest::blocking::Client,
    url: &str,
) -> anyhow::Result<String> {
    let resp = client
        .get(url)
        .send()
        .with_context(|| format!("schemastore: probe GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("schemastore: probe GET {url} returned {status}");
    }
    Ok(anodizer_core::http::body_of_blocking(resp))
}

/// GET a raw.githubusercontent.com URL whose absence is MEANINGFUL: a 404 maps
/// to `None` (file absent upstream ⇒ change-needed), a success maps to
/// `Some(body)`, and any OTHER non-success / transport error is an error (so
/// the probe falls through rather than guessing).
pub(super) fn fetch_raw_optional(
    client: &reqwest::blocking::Client,
    url: &str,
) -> anyhow::Result<Option<String>> {
    let resp = client
        .get(url)
        .send()
        .with_context(|| format!("schemastore: probe GET {url}"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        anyhow::bail!("schemastore: probe GET {url} returned {status}");
    }
    Ok(Some(anodizer_core::http::body_of_blocking(resp)))
}

/// Dry-run / snapshot path: compute the planned action per schema and log it
/// without touching the network. The verdict is left unresolved (no upstream
/// catalog is fetched), so the line reports the planned mode/url/vendor file.
fn plan_dry_run(
    ctx: &mut Context,
    cfg: &SchemastoreConfig,
    effective: &[(&SchemaEntry, String)],
    log: &StageLogger,
) -> anyhow::Result<PublishEvidence> {
    for (entry, description) in effective {
        let plan = plan_schema_scoped(ctx, cfg, entry, description, None)?;
        log.status(&plan.planned_line());
    }
    log.status(&format!(
        "(dry-run) planned {} schemastore registration(s); no PR opened",
        effective.len()
    ));
    Ok(PublishEvidence::new("schemastore"))
}

/// Build a [`SchemaPlan`] resolving the bound crate's version inside the
/// per-crate scope for a versioned vendor entry, so the `<VER>` stamped into
/// the filename/`versions` key is THIS crate's tag — never crate[0]'s in
/// workspace per-crate independent-version mode.
pub(super) fn plan_schema_scoped(
    ctx: &mut Context,
    cfg: &SchemastoreConfig,
    entry: &SchemaEntry,
    description: &str,
    catalog_json: Option<&str>,
) -> anyhow::Result<SchemaPlan> {
    let versioned = cfg.resolved_versioned(entry);
    if !versioned {
        return plan_schema(entry, description, false, None, catalog_json);
    }
    // Versioned vendor needs a version; bind it to the schema's own crate so
    // sibling schemas in a per-crate workspace don't all inherit crate[0]'s
    // version. Default to the primary crate in single/lockstep modes.
    let crate_name = entry
        .crate_
        .clone()
        .or_else(|| ctx.config.crate_universe().first().map(|c| c.name.clone()))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{}: versioned vendor entry needs a `crate` to bind its version scope",
                entry_label(&entry.name)
            )
        })?;
    crate::publisher_helpers::with_published_crate_scope(
        ctx,
        &crate_name,
        &anodizer_core::crate_scope::resolve_crate_tag,
        |ctx| {
            let version = ctx.version();
            plan_schema(entry, description, true, Some(&version), catalog_json)
        },
    )
}

/// Real path: clone the fork, sync to upstream, splice/vendor every schema,
/// then (if there is work) commit, push, and open one PR.
pub(super) fn run_real(
    ctx: &mut Context,
    cfg: &SchemastoreConfig,
    effective: &[(&SchemaEntry, String)],
    log: &StageLogger,
) -> anyhow::Result<PublishEvidence> {
    // The work branch and the pending-PR idempotency check are both keyed on
    // the release version (`schemastore-v<version>`). An empty `Version` would
    // yield a bare `schemastore-v` that collides release-to-release and defeats
    // the duplicate-PR guard — bail before any irreversible clone/push.
    if ctx.version().is_empty() {
        anyhow::bail!(
            "schemastore: the release Version is empty — cannot build a stable PR branch; \
             ensure the tag/version is resolved before the publish stage runs"
        );
    }

    let repo = cfg.repository.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "schemastore: no `repository` (fork of {UPSTREAM_OWNER}/{UPSTREAM_REPO}) configured \
             — a fork is required to push the branch and open the PR"
        )
    })?;
    let (fork_owner_raw, fork_name_raw) = crate::util::resolve_repo_owner_name(Some(repo))
        .ok_or_else(|| {
            anyhow::anyhow!("schemastore: `repository` must set both `owner` and `name` (the fork)")
        })?;
    let fork_owner =
        crate::util::render_or_warn(ctx, log, "schemastore.repository.owner", &fork_owner_raw)?;
    let fork_name =
        crate::util::render_or_warn(ctx, log, "schemastore.repository.name", &fork_name_raw)?;

    let token = crate::util::resolve_repo_token(ctx, Some(repo), Some(TOKEN_ENV_VAR));

    let tmp_dir = tempfile::tempdir().context("schemastore: create temp dir")?;
    let repo_path = tmp_dir.path();
    crate::util::clone_repo(
        ctx,
        Some(repo),
        &fork_owner,
        &fork_name,
        token.as_deref(),
        repo_path,
        "schemastore",
        log,
    )?;

    // The fork drifts behind upstream, so reset the work tree onto the current
    // upstream master before splicing — otherwise edits target a stale catalog
    // and the PR carries a noisy, conflict-prone diff.
    sync_to_upstream(repo_path, log)?;

    let catalog_abs = repo_path.join(CATALOG_PATH);
    let mut catalog_json = std::fs::read_to_string(&catalog_abs)
        .with_context(|| format!("schemastore: read {}", catalog_abs.display()))?;

    let project_root = ctx
        .options
        .project_root
        .clone()
        .unwrap_or_else(|| PathBuf::from("."));

    // Plans that produced a real change (Add/Update), in apply order — the
    // PR title/body/commit message are built from these so they distinguish
    // vendor/versioned and never re-derive a mode that was already proven.
    let mut applied: Vec<SchemaPlan> = Vec::new();
    for (entry, description) in effective {
        let plan = plan_schema_scoped(ctx, cfg, entry, description, Some(&catalog_json))?;

        // For a vendor schema, read + format the LOCAL file now so the change-
        // decision can byte-compare it against the upstream copy. External
        // entries have no file (`None`).
        let local_schema = if plan.mode == SchemaMode::Vendor {
            Some(read_local_vendor_schema(&project_root, entry)?)
        } else {
            None
        };

        // Gate on the SHARED change-decision, not the catalog-entry verdict
        // alone: a vendor schema whose catalog entry is unchanged but whose
        // file content drifted upstream must still be re-pushed. The clone
        // already holds every upstream file, so the comparison is authoritative.
        let cloned_vendor = read_cloned_vendor_file(repo_path, &plan);
        let cloned_jsonc = read_cloned_jsonc(repo_path);
        let remote = RemoteState {
            catalog_json: &catalog_json,
            vendor_file: cloned_vendor.as_deref(),
            jsonc: cloned_jsonc.as_deref(),
        };
        if !schema_change_needed(&plan, local_schema.as_deref(), &remote) {
            log.status(&format!(
                "schemastore `{}` already registered and current — no change",
                plan.name
            ));
            continue;
        }

        // Vendor mode: copy the schema file in, and allowlist a too-high
        // dialect in the SAME PR (SchemaStore CI rejects 2019-09/2020-12
        // otherwise). External mode writes nothing but the catalog entry. The
        // file is written even when the catalog entry alone was a no-op (the
        // drift case) — the decision above already proved a change is needed.
        if let Some(formatted) = local_schema.as_deref() {
            write_vendor_schema(repo_path, entry, &plan, formatted, log)?;
        }

        catalog_json = catalog::splice_entry(&catalog_json, &plan.desired_entry)
            .with_context(|| format!("schemastore: splice catalog entry for `{}`", plan.name))?;
        applied.push(plan);
    }

    if applied.is_empty() {
        log.status("every schema already registered and current — nothing to publish");
        return Ok(PublishEvidence::new("schemastore"));
    }

    std::fs::write(&catalog_abs, &catalog_json)
        .with_context(|| format!("schemastore: write {}", catalog_abs.display()))?;

    let branch = schemastore_branch(&ctx.version());

    // Pending-PR idempotency: if a fork→upstream PR on this branch is already
    // open and unmerged, treat the work as in-flight rather than pushing a
    // duplicate. Best-effort — a query failure must not abort the publish.
    match crate::util::find_open_pr_numbers_for_head(
        UPSTREAM_OWNER,
        UPSTREAM_REPO,
        &fork_owner,
        &branch,
        token.as_deref(),
        TOKEN_ENV_VAR,
    ) {
        Ok(nums) if !nums.is_empty() => {
            log.status(&format!(
                "a schemastore PR for {fork_owner}:{branch} → {UPSTREAM_OWNER}/{UPSTREAM_REPO} \
                 is already open (#{}) — treating as in-flight, not re-pushing",
                nums[0]
            ));
            return Ok(schemastore_evidence(&fork_owner, &branch));
        }
        Ok(_) => {}
        Err(e) => log.warn(&format!(
            "could not check for an existing open schemastore PR ({e}); proceeding to push"
        )),
    }

    let commit_msg = schemastore_commit_msg(&applied);
    let commit_opts = crate::util::resolve_commit_opts(ctx, cfg.commit_author.as_ref(), log)?;
    let push = crate::util::commit_and_push_with_opts(
        repo_path,
        &["."],
        &commit_msg,
        Some(branch.as_str()),
        "schemastore",
        &commit_opts,
        log,
    )?;
    if !push.is_pushed() {
        log.status("fork branch already matches the staged tree — nothing pushed");
        return Ok(PublishEvidence::new("schemastore"));
    }

    let upstream_slug = format!("{UPSTREAM_OWNER}/{UPSTREAM_REPO}");
    let pr_outcome = crate::util::submit_pr_via_gh_with_opts(
        repo_path,
        &upstream_slug,
        &format!("{fork_owner}:{branch}"),
        &schemastore_pr_title(&applied),
        &schemastore_pr_body(&applied),
        "schemastore",
        log,
        crate::util::SubmitPrOpts {
            update_existing_pr: false,
        },
    );
    if let Some(outcome) = pr_outcome {
        ctx.record_publisher_outcome(outcome);
    }

    Ok(schemastore_evidence(&fork_owner, &branch))
}

/// Reset the cloned fork's work tree onto upstream `SchemaStore/schemastore`'s
/// default branch so edits target the current tree.
pub(super) fn sync_to_upstream(
    repo_path: &std::path::Path,
    log: &StageLogger,
) -> anyhow::Result<()> {
    let upstream_url = format!("https://github.com/{UPSTREAM_OWNER}/{UPSTREAM_REPO}.git");
    // Add the upstream remote (ignore "already exists").
    let _ = crate::util::run_cmd_in(
        repo_path,
        "git",
        &["remote", "add", "upstream", &upstream_url],
        "schemastore: git remote add upstream",
    );
    // A network fetch of the (hardcoded, public) upstream: bound it with the
    // shared fetch deadline and non-interactive prompt handling so a wedged
    // remote or an unexpected credential prompt fails instead of hanging the
    // release.
    crate::util::run_cmd_in_timeout(
        repo_path,
        "git",
        &["fetch", "--depth=1", "upstream", UPSTREAM_DEFAULT_BRANCH],
        "schemastore: git fetch upstream",
        None,
        log,
        crate::util::GIT_FETCH_TIMEOUT,
    )?;
    // Hard-reset onto the freshly fetched upstream tip. A reset (not rebase) is
    // correct here because no local commits exist yet — the working tree is a
    // bare clone of the fork's default branch; pointing it at upstream is the
    // intent, and a rebase would be a no-op with extra failure surface.
    crate::util::run_cmd_in(
        repo_path,
        "git",
        &[
            "reset",
            "--hard",
            &format!("upstream/{UPSTREAM_DEFAULT_BRANCH}"),
        ],
        "schemastore: git reset --hard upstream",
    )?;
    log.status(&format!(
        "synced schemastore fork work tree to {UPSTREAM_OWNER}/{UPSTREAM_REPO}@{UPSTREAM_DEFAULT_BRANCH}"
    ));
    Ok(())
}

/// Read the LOCAL vendor schema off `project_root` and reformat it to
/// SchemaStore's prettier defaults — the exact bytes a publish would write.
///
/// Shared by the change-decision (which byte-compares this against the upstream
/// copy) and the write path, so the content that gates the no-op and the
/// content that lands in the PR are derived identically.
pub(super) fn read_local_vendor_schema(
    project_root: &std::path::Path,
    entry: &SchemaEntry,
) -> anyhow::Result<String> {
    let rel = entry.schema_file.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "{}: vendor entry has no schema_file",
            entry_label(&entry.name)
        )
    })?;
    let src = project_root.join(rel);
    let raw = std::fs::read_to_string(&src).with_context(|| {
        format!(
            "{}: read schema_file {}",
            entry_label(&entry.name),
            src.display()
        )
    })?;
    manifest::format_vendor_schema(&raw)
        .with_context(|| format!("{}: format vendor schema", entry_label(&entry.name)))
}

/// Read the upstream copy of a vendor plan's file from the cloned tree, or
/// `None` when the plan has no vendor path or the file is absent upstream
/// (which the change-decision reads as "differs ⇒ change needed").
pub(super) fn read_cloned_vendor_file(
    repo_path: &std::path::Path,
    plan: &SchemaPlan,
) -> Option<String> {
    let rel = plan.vendor_path.as_ref()?;
    std::fs::read_to_string(repo_path.join(rel)).ok()
}

/// Read the upstream `schema-validation.jsonc` from the cloned tree, or `None`
/// when it is missing (the change-decision reads `None` for a too-high schema
/// as "couldn't confirm the allowlist ⇒ change needed").
pub(super) fn read_cloned_jsonc(repo_path: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(repo_path.join(DIALECT_ALLOWLIST_PATH)).ok()
}

/// Write the already-formatted vendor schema `formatted` into the cloned repo.
/// When the schema's `$schema` dialect is too high for SchemaStore's CI,
/// allowlist its vendored filename in `schema-validation.jsonc` in the same PR.
pub(super) fn write_vendor_schema(
    repo_path: &std::path::Path,
    entry: &SchemaEntry,
    plan: &SchemaPlan,
    formatted: &str,
    log: &StageLogger,
) -> anyhow::Result<()> {
    let vendor_rel = plan
        .vendor_path
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("{}: vendor plan has no path", entry_label(&entry.name)))?;
    let dest = repo_path.join(vendor_rel);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("schemastore: mkdir {}", parent.display()))?;
    }
    std::fs::write(&dest, formatted)
        .with_context(|| format!("schemastore: write {}", dest.display()))?;
    log.status(&format!(
        "vendored schemastore `{}` → {}",
        plan.name,
        vendor_rel.display()
    ));

    // A 2019-09 / 2020-12 schema fails SchemaStore CI unless its catalog name
    // is allowlisted under `highSchemaVersion` — add it in this same PR so the
    // schema lands as-authored. The `$schema` survives reformatting, so the
    // dialect read off `formatted` matches the source.
    let dialect = raw_dialect(formatted);
    if dialect == Dialect::TooHigh {
        let allow_abs = repo_path.join(DIALECT_ALLOWLIST_PATH);
        let jsonc = std::fs::read_to_string(&allow_abs)
            .with_context(|| format!("schemastore: read {}", allow_abs.display()))?;
        // SchemaStore matches the allowlist against `path.basename(schemaPath)`
        // (cli.js: `highSchemaVersion.includes(schema.name)`), i.e. the vendored
        // file's basename WITH `.json` (`cfgd-module.json`, `cfgd-module-0.4.2.json`)
        // — NOT the catalog display name. Keying on the display name never
        // matches the file and hard-fails SchemaStore CI.
        let allow_name = allowlist_name_for(plan)?;
        let updated = catalog::add_high_schema_version(&jsonc, &allow_name).with_context(|| {
            format!("schemastore: allowlist high-dialect schema `{allow_name}`")
        })?;
        std::fs::write(&allow_abs, &updated)
            .with_context(|| format!("schemastore: write {}", allow_abs.display()))?;
        log.status(&format!(
            "allowlisted high-dialect schema `{allow_name}` in {DIALECT_ALLOWLIST_PATH}"
        ));
    }
    Ok(())
}

/// Classify a schema's `$schema` dialect from its raw JSON, defaulting to
/// `Unknown` when the field is absent (so the caller skips the allowlist).
pub(super) fn raw_dialect(raw: &str) -> Dialect {
    serde_json::from_str::<Value>(raw)
        .ok()
        .as_ref()
        .and_then(|v| v.get("$schema"))
        .and_then(Value::as_str)
        .map(manifest::classify_dialect)
        .unwrap_or(Dialect::Unknown)
}

/// The `highSchemaVersion` allowlist key for a vendor plan: the vendored
/// file's basename **including** the `.json` extension (`cfgd-module.json` for
/// a plain vendor, `cfgd-module-0.4.2.json` for a versioned one).
///
/// SchemaStore's CI matches this allowlist against `path.basename(schemaPath)`
/// — the vendored filename — never the catalog display name, so the key must
/// be derived from [`SchemaPlan::vendor_path`], not [`SchemaPlan::name`].
pub(super) fn allowlist_name_for(plan: &SchemaPlan) -> anyhow::Result<String> {
    let vendor_rel = plan.vendor_path.as_ref().ok_or_else(|| {
        anyhow::anyhow!("{}: vendor plan has no path for allowlist key", plan.name)
    })?;
    vendor_rel
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{}: vendor path `{}` has no file name for allowlist key",
                plan.name,
                vendor_rel.display()
            )
        })
}

/// Branch name for the fork-side work branch. One branch per release version
/// carries every schema entry (external + vendor mixed).
pub(super) fn schemastore_branch(version: &str) -> String {
    format!("schemastore-v{version}")
}

/// Build the PR-target evidence so a later `--rollback-only` can close the PR.
pub(super) fn schemastore_evidence(fork_owner: &str, branch: &str) -> PublishEvidence {
    let mut evidence = PublishEvidence::new("schemastore");
    evidence.extra = anodizer_core::PublishEvidenceExtra::Schemastore(
        anodizer_core::publish_evidence::SchemastoreExtra {
            schemastore_targets: vec![anodizer_core::publish_evidence::SchemastoreTargetSnapshot {
                upstream_owner: UPSTREAM_OWNER.to_string(),
                upstream_repo: UPSTREAM_REPO.to_string(),
                fork_owner: fork_owner.to_string(),
                branch: branch.to_string(),
                token_env_var: Some(TOKEN_ENV_VAR.to_string()),
            }],
        },
    );
    evidence
}

/// Render the verb-grouped schema summary used by both the commit message and
/// the PR title, e.g. `Add a, b` / `Update c` / `Add a; update b`.
///
/// The verb is the per-plan [`catalog::Verdict`] (`Add` vs `Update`), so the
/// message states truthfully what the PR does — "Add if it doesn't exist,
/// update if it does." A plan whose verdict is `None` (no upstream catalog was
/// available, e.g. a forced run) is treated as an add.
///
/// `NoOp` routes to "Update", not "Add": the `applied` set is gated on
/// `schema_change_needed`, not the catalog verdict, so a vendor plan whose
/// catalog entry is unchanged (`NoOp`) but whose vendored FILE drifted still
/// reaches here — that is a file-content refresh of an existing registration,
/// not a new add.
pub(super) fn schemastore_summary(applied: &[SchemaPlan]) -> String {
    let mut adds: Vec<&str> = Vec::new();
    let mut updates: Vec<&str> = Vec::new();
    for p in applied {
        match p.verdict {
            Some(catalog::Verdict::Update) | Some(catalog::Verdict::NoOp) => {
                updates.push(p.name.as_str())
            }
            _ => adds.push(p.name.as_str()),
        }
    }
    match (adds.is_empty(), updates.is_empty()) {
        (false, true) => format!("Add {}", adds.join(", ")),
        (true, false) => format!("Update {}", updates.join(", ")),
        // Mixed (or, defensively, the empty `applied` edge) — name both verbs.
        _ => format!("Add {}; update {}", adds.join(", "), updates.join(", ")),
    }
}

/// Commit message naming the registered schemas, verb derived from each plan's
/// verdict (add vs update).
pub(super) fn schemastore_commit_msg(applied: &[SchemaPlan]) -> String {
    format!("{} schema(s)", schemastore_summary(applied))
}

/// PR title naming the registered schemas, verb derived from each plan's
/// verdict (add vs update).
pub(super) fn schemastore_pr_title(applied: &[SchemaPlan]) -> String {
    format!("{} schema(s)", schemastore_summary(applied))
}

/// PR body listing each registered schema's name, hosting mode, and url.
/// Built from the already-computed [`SchemaPlan`]s so the mode (including
/// `vendor, versioned`) is the proven one — never re-derived.
pub(super) fn schemastore_pr_body(applied: &[SchemaPlan]) -> String {
    let mut body = String::from("## Schemas\n");
    for p in applied {
        body.push_str(&format!(
            "- **{}** ({}) → {}\n",
            p.name,
            p.mode_label(),
            p.url
        ));
    }
    body.push('\n');
    body.push_str(crate::util::SUBMITTED_BY_FOOTER);
    body
}
