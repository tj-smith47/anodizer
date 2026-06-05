//! SchemaStore publish orchestration: turns the configured `schemas` into a
//! single pull request against a fork of `SchemaStore/schemastore`, reusing
//! krew's clone/commit/push/PR machinery and delegating every decision to the
//! pure helpers in `catalog`/`manifest`.
//!
//! The decision core ([`plan_schema`]) is pure (string-in, value-out) so the
//! add/update/no-op verdict, vendor formatting, and versioned `<VER>` filename
//! derivation are all unit-testable without git or network. The I/O shell
//! ([`run_publish`]) reads the synced upstream catalog, applies the planned
//! splices/writes, and opens the PR.

use std::path::PathBuf;

use anodizer_core::PublishEvidence;
use anodizer_core::config::{SchemaEntry, SchemaMode, SchemastoreConfig};
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::Context as _;
use serde_json::Value;

use super::manifest::{self, Dialect};
use super::{catalog, entry_label};

/// Canonical upstream the SchemaStore PR targets.
const UPSTREAM_OWNER: &str = "SchemaStore";
const UPSTREAM_REPO: &str = "schemastore";
/// Default branch of `SchemaStore/schemastore`. The fork drifts behind, so the
/// work branch is synced to this before splicing (see [`run_publish`]).
const UPSTREAM_DEFAULT_BRANCH: &str = "master";
/// Repo-relative path of the catalog the publisher splices entries into.
const CATALOG_PATH: &str = "src/api/json/catalog.json";
/// Repo-relative path of the dialect allowlist (`highSchemaVersion`).
const DIALECT_ALLOWLIST_PATH: &str = "src/schema-validation.jsonc";
/// Env var the rollback path consults for the close-PR token.
const TOKEN_ENV_VAR: &str = "SCHEMASTORE_TOKEN";

/// The resolved publish action for one schema entry, computed purely from the
/// entry, its resolved metadata, and (when available) the upstream catalog.
///
/// `verdict` is `None` when the upstream catalog was not available to compare
/// against (the no-network dry-run path); on the real path it is always `Some`.
#[derive(Debug)]
pub(crate) struct SchemaPlan {
    /// Catalog display name (the entry's `name`).
    pub(crate) name: String,
    /// Hosting mode inferred from field presence.
    pub(crate) mode: SchemaMode,
    /// The catalog `url` (external: the entry's `url`; vendor: the
    /// `schemastore.org` URL, version-suffixed when versioned).
    pub(crate) url: String,
    /// Repo-relative path the vendored schema file is written to, or `None`
    /// for external entries.
    pub(crate) vendor_path: Option<PathBuf>,
    /// True when this is a versioned vendor emission.
    pub(crate) versioned: bool,
    /// The desired catalog entry object (prettier key order).
    pub(crate) desired_entry: Value,
    /// add/update/no-op against the upstream catalog, or `None` when no
    /// catalog was available to compare against.
    pub(crate) verdict: Option<catalog::Verdict>,
}

impl SchemaPlan {
    /// One-line operator-facing summary of the planned action, used by the
    /// dry-run log so an operator sees exactly what a real run would do.
    fn planned_line(&self) -> String {
        let mode = match self.mode {
            SchemaMode::External => "external",
            SchemaMode::Vendor if self.versioned => "vendor/versioned",
            SchemaMode::Vendor => "vendor",
        };
        let verb = match self.verdict {
            Some(catalog::Verdict::NoOp) => "no-op (already registered)",
            Some(catalog::Verdict::Add) => "register",
            Some(catalog::Verdict::Update) => "refresh",
            None => "register/refresh",
        };
        let vendor = match &self.vendor_path {
            Some(p) => format!(", vendor file {}", p.display()),
            None => String::new(),
        };
        format!(
            "schemastore: would {verb} `{}` ({mode}) → url {}{vendor}",
            self.name, self.url
        )
    }
}

/// Build the desired catalog entry + action for one schema, purely.
///
/// `description` is the already-resolved, sanitized catalog description.
/// `versioned` / `version` carry the resolved versioned flag and the bound
/// crate's version (the caller resolves the version inside
/// `with_published_crate_scope` so per-crate mode stamps the right version).
/// `catalog_json` is the upstream catalog string when available (real path);
/// pass `None` on the no-network dry-run path to skip the verdict.
pub(crate) fn plan_schema(
    entry: &SchemaEntry,
    description: &str,
    versioned: bool,
    version: Option<&str>,
    catalog_json: Option<&str>,
) -> anyhow::Result<SchemaPlan> {
    let mode = entry.mode()?;
    let slug = entry
        .slug
        .clone()
        .unwrap_or_else(|| manifest::slugify(&entry.name));

    let (url, vendor_path, versions) = match mode {
        SchemaMode::External => (
            entry
                .url
                .clone()
                .ok_or_else(|| anyhow::anyhow!("{}: external entry has no url", entry.name))?,
            None,
            None,
        ),
        SchemaMode::Vendor if versioned => {
            let ver = version.ok_or_else(|| {
                anyhow::anyhow!(
                    "{}: versioned vendor entry needs a resolved crate version",
                    entry.name
                )
            })?;
            let url = format!("https://www.schemastore.org/{slug}-{ver}.json");
            // Carry prior versions forward by merging into whatever the
            // upstream entry already lists, so older versioned files keep
            // their catalog references (SchemaStore CI requires every listed
            // `versions` URL to resolve to a present file).
            let prior = catalog_json
                .and_then(|c| upstream_versions(c, &entry.name))
                .transpose()?;
            let versions = catalog::merge_versions(prior.as_ref(), ver, &url);
            (
                url,
                Some(PathBuf::from(format!("src/schemas/json/{slug}-{ver}.json"))),
                Some(versions),
            )
        }
        SchemaMode::Vendor => (
            format!("https://www.schemastore.org/{slug}.json"),
            Some(PathBuf::from(format!("src/schemas/json/{slug}.json"))),
            None,
        ),
    };

    let desired_entry = catalog::build_entry_json(
        &entry.name,
        description,
        &entry.file_match,
        &url,
        versions.as_ref(),
    );

    let verdict = match catalog_json {
        Some(c) => Some(catalog::verdict(c, &entry.name, &desired_entry)?),
        None => None,
    };

    Ok(SchemaPlan {
        name: entry.name.clone(),
        mode,
        url,
        vendor_path,
        versioned,
        desired_entry,
        verdict,
    })
}

/// Extract a catalog entry's existing `versions` map by `name`, if present.
/// Returns `None` when the entry is absent or has no `versions`; `Some(Err)`
/// only on malformed catalog JSON.
fn upstream_versions(
    catalog_json: &str,
    name: &str,
) -> Option<anyhow::Result<serde_json::Map<String, Value>>> {
    let cat: Value = match serde_json::from_str(catalog_json) {
        Ok(v) => v,
        Err(e) => return Some(Err(e.into())),
    };
    let entry = cat
        .get("schemas")
        .and_then(Value::as_array)?
        .iter()
        .find(|e| e.get("name").and_then(Value::as_str) == Some(name))?;
    let versions = entry.get("versions").and_then(Value::as_object)?;
    Some(Ok(versions.clone()))
}

/// Effective schemas after the per-entry `skip` and `if:` gates, paired with
/// the resolved description for each. Returns an error if a description cannot
/// be derived or fails the content rules (preflight already checks this, but
/// the publish path must not assume preflight ran).
fn effective_schemas<'a>(
    ctx: &Context,
    cfg: &'a SchemastoreConfig,
) -> anyhow::Result<Vec<(&'a SchemaEntry, String)>> {
    let mut out = Vec::new();
    for entry in &cfg.schemas {
        if cfg.resolved_skip(entry) {
            continue;
        }
        // `if:` gate — falsy renders skip the entry. Reuse the shared
        // `if`-eval so the semantics match every other publisher.
        let proceed = anodizer_core::config::evaluate_if_condition(
            cfg.resolved_if(entry),
            &entry_label(&entry.name),
            |t| ctx.render_template(t),
        )?;
        if !proceed {
            continue;
        }
        let description = resolve_description(ctx, entry)?;
        out.push((entry, description));
    }
    Ok(out)
}

/// Resolve and sanitize a schema's catalog description: the entry's own
/// `description` if set, else derived from the bound crate's metadata (or the
/// project metadata when no crate is bound), then validated against
/// SchemaStore's content rules.
fn resolve_description(ctx: &Context, entry: &SchemaEntry) -> anyhow::Result<String> {
    let raw = match entry.description.as_deref() {
        Some(d) => d.to_string(),
        None => {
            let derived = match entry.crate_.as_deref() {
                Some(c) => ctx.config.meta_description_for(c),
                None => ctx.config.meta_description_project(),
            };
            derived
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "{}: no description set and none derivable from project/crate metadata",
                        entry_label(&entry.name)
                    )
                })?
                .to_string()
        }
    };
    manifest::sanitize_description(&raw)
        .map_err(|e| anyhow::anyhow!("{} description: {e}", entry_label(&entry.name)))
}

/// Run the SchemaStore publish, returning evidence of what was registered.
pub(crate) fn run_publish(ctx: &mut Context) -> anyhow::Result<PublishEvidence> {
    let log = ctx.logger("publish");
    // Clone the config out of `ctx` so the per-schema body can re-borrow `ctx`
    // mutably (version re-scoping) without aliasing `ctx.config`.
    let cfg = ctx.config.schemastore.clone();

    let effective = effective_schemas(ctx, &cfg)?;
    if effective.is_empty() {
        log.status("schemastore: no schemas to register (all skipped or none configured)");
        return Ok(PublishEvidence::new("schemastore"));
    }

    if ctx.is_dry_run() || ctx.is_snapshot() {
        return plan_dry_run(ctx, &cfg, &effective, &log);
    }

    run_real(ctx, &cfg, &effective, &log)
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
        "schemastore: (dry-run) planned {} schema registration(s); no PR opened",
        effective.len()
    ));
    Ok(PublishEvidence::new("schemastore"))
}

/// Build a [`SchemaPlan`] resolving the bound crate's version inside the
/// per-crate scope for a versioned vendor entry, so the `<VER>` stamped into
/// the filename/`versions` key is THIS crate's tag — never crate[0]'s in
/// workspace per-crate independent-version mode.
fn plan_schema_scoped(
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
        .or_else(|| crate::util::all_crates(ctx).first().map(|c| c.name.clone()))
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
fn run_real(
    ctx: &mut Context,
    cfg: &SchemastoreConfig,
    effective: &[(&SchemaEntry, String)],
    log: &StageLogger,
) -> anyhow::Result<PublishEvidence> {
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
    let fork_owner = ctx
        .render_template(&fork_owner_raw)
        .unwrap_or(fork_owner_raw);
    let fork_name = ctx.render_template(&fork_name_raw).unwrap_or(fork_name_raw);

    let token = crate::util::resolve_repo_token(ctx, Some(repo), Some(TOKEN_ENV_VAR));

    let tmp_dir = tempfile::tempdir().context("schemastore: create temp dir")?;
    let repo_path = tmp_dir.path();
    crate::util::clone_repo(
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

    let mut any_change = false;
    for (entry, description) in effective {
        let plan = plan_schema_scoped(ctx, cfg, entry, description, Some(&catalog_json))?;
        match plan.verdict {
            Some(catalog::Verdict::NoOp) => {
                log.status(&format!(
                    "schemastore: `{}` already registered and current — no change",
                    plan.name
                ));
                continue;
            }
            Some(catalog::Verdict::Add) | Some(catalog::Verdict::Update) | None => {}
        }

        // Vendor mode: copy the schema file in, and allowlist a too-high
        // dialect in the SAME PR (SchemaStore CI rejects 2019-09/2020-12
        // otherwise). External mode writes nothing but the catalog entry.
        if plan.mode == SchemaMode::Vendor {
            write_vendor_schema(repo_path, &project_root, entry, &plan, log)?;
        }

        catalog_json = catalog::splice_entry(&catalog_json, &plan.name, &plan.desired_entry)
            .with_context(|| format!("schemastore: splice catalog entry for `{}`", plan.name))?;
        any_change = true;
    }

    if !any_change {
        log.status("schemastore: every schema already registered and current — nothing to publish");
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
                "schemastore: a PR for {fork_owner}:{branch} → {UPSTREAM_OWNER}/{UPSTREAM_REPO} \
                 is already open (#{}) — treating as in-flight, not re-pushing",
                nums[0]
            ));
            return Ok(schemastore_evidence(&fork_owner, &branch));
        }
        Ok(_) => {}
        Err(e) => log.warn(&format!(
            "schemastore: could not check for an existing open PR ({e}); proceeding to push"
        )),
    }

    let commit_msg = schemastore_commit_msg(effective);
    let commit_opts = crate::util::resolve_commit_opts(ctx, cfg.commit_author.as_ref());
    let push = crate::util::commit_and_push_with_opts(
        repo_path,
        &["."],
        &commit_msg,
        Some(branch.as_str()),
        "schemastore",
        &commit_opts,
    )?;
    if !push.is_pushed() {
        log.status("schemastore: fork branch already matches the staged tree — nothing pushed");
        return Ok(PublishEvidence::new("schemastore"));
    }

    let upstream_slug = format!("{UPSTREAM_OWNER}/{UPSTREAM_REPO}");
    let pr_outcome = crate::util::submit_pr_via_gh_with_opts(
        repo_path,
        &upstream_slug,
        &format!("{fork_owner}:{branch}"),
        &schemastore_pr_title(effective),
        &schemastore_pr_body(effective),
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
fn sync_to_upstream(repo_path: &std::path::Path, log: &StageLogger) -> anyhow::Result<()> {
    let upstream_url = format!("https://github.com/{UPSTREAM_OWNER}/{UPSTREAM_REPO}.git");
    // Add the upstream remote (ignore "already exists").
    let _ = crate::util::run_cmd_in(
        repo_path,
        "git",
        &["remote", "add", "upstream", &upstream_url],
        "schemastore: git remote add upstream",
    );
    crate::util::run_cmd_in(
        repo_path,
        "git",
        &["fetch", "--depth=1", "upstream", UPSTREAM_DEFAULT_BRANCH],
        "schemastore: git fetch upstream",
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
        "schemastore: synced fork work tree to {UPSTREAM_OWNER}/{UPSTREAM_REPO}@{UPSTREAM_DEFAULT_BRANCH}"
    ));
    Ok(())
}

/// Read the vendor schema off disk, format it, and write it into the cloned
/// repo. When the schema's `$schema` dialect is too high for SchemaStore's CI,
/// allowlist its catalog name in `schema-validation.jsonc` in the same PR.
fn write_vendor_schema(
    repo_path: &std::path::Path,
    project_root: &std::path::Path,
    entry: &SchemaEntry,
    plan: &SchemaPlan,
    log: &StageLogger,
) -> anyhow::Result<()> {
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
    let formatted = manifest::format_vendor_schema(&raw)
        .with_context(|| format!("{}: format vendor schema", entry_label(&entry.name)))?;

    let vendor_rel = plan
        .vendor_path
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("{}: vendor plan has no path", entry_label(&entry.name)))?;
    let dest = repo_path.join(vendor_rel);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("schemastore: mkdir {}", parent.display()))?;
    }
    std::fs::write(&dest, &formatted)
        .with_context(|| format!("schemastore: write {}", dest.display()))?;
    log.status(&format!(
        "schemastore: vendored `{}` → {}",
        plan.name,
        vendor_rel.display()
    ));

    // A 2019-09 / 2020-12 schema fails SchemaStore CI unless its catalog name
    // is allowlisted under `highSchemaVersion` — add it in this same PR so the
    // schema lands as-authored.
    let dialect = raw_dialect(&raw);
    if dialect == Dialect::TooHigh {
        let allow_abs = repo_path.join(DIALECT_ALLOWLIST_PATH);
        let jsonc = std::fs::read_to_string(&allow_abs)
            .with_context(|| format!("schemastore: read {}", allow_abs.display()))?;
        let updated = catalog::add_high_schema_version(&jsonc, &entry.name).with_context(|| {
            format!(
                "schemastore: allowlist high-dialect schema `{}`",
                entry.name
            )
        })?;
        std::fs::write(&allow_abs, &updated)
            .with_context(|| format!("schemastore: write {}", allow_abs.display()))?;
        log.status(&format!(
            "schemastore: allowlisted high-dialect schema `{}` in {DIALECT_ALLOWLIST_PATH}",
            entry.name
        ));
    }
    Ok(())
}

/// Classify a schema's `$schema` dialect from its raw JSON, defaulting to
/// `Unknown` when the field is absent (so the caller skips the allowlist).
fn raw_dialect(raw: &str) -> Dialect {
    serde_json::from_str::<Value>(raw)
        .ok()
        .as_ref()
        .and_then(|v| v.get("$schema"))
        .and_then(Value::as_str)
        .map(manifest::classify_dialect)
        .unwrap_or(Dialect::Unknown)
}

/// Branch name for the fork-side work branch. One branch per release version
/// carries every schema entry (external + vendor mixed).
fn schemastore_branch(version: &str) -> String {
    format!("schemastore-v{version}")
}

/// Build the PR-target evidence so a later `--rollback-only` can close the PR.
fn schemastore_evidence(fork_owner: &str, branch: &str) -> PublishEvidence {
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

/// Commit message naming the registered schemas.
fn schemastore_commit_msg(effective: &[(&SchemaEntry, String)]) -> String {
    let names: Vec<&str> = effective.iter().map(|(e, _)| e.name.as_str()).collect();
    format!("Register/refresh {}", names.join(", "))
}

/// PR title naming the registered schemas.
fn schemastore_pr_title(effective: &[(&SchemaEntry, String)]) -> String {
    let names: Vec<&str> = effective.iter().map(|(e, _)| e.name.as_str()).collect();
    format!("Add/update {} schema(s)", names.join(", "))
}

/// PR body listing each schema's name, mode, and url.
fn schemastore_pr_body(effective: &[(&SchemaEntry, String)]) -> String {
    let mut body = String::from("## Schemas\n");
    for (e, _) in effective {
        let mode = match e.mode() {
            Ok(SchemaMode::External) => "external",
            Ok(SchemaMode::Vendor) => "vendor",
            Err(_) => "?",
        };
        body.push_str(&format!("- **{}** ({mode})\n", e.name));
    }
    body.push_str("\nAutomatically submitted by anodizer.");
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::config::{SchemaEntry, SchemastoreConfig};
    use anodizer_core::test_helpers::TestContextBuilder;

    fn external_entry() -> SchemaEntry {
        SchemaEntry {
            name: "Anodizer".into(),
            file_match: vec![".anodizer.yaml".into(), ".anodizer.yml".into()],
            url: Some("https://tj-smith47.github.io/anodizer/schema.json".into()),
            description: Some("Anodizer Rust release-automation configuration file".into()),
            ..Default::default()
        }
    }

    fn vendor_entry() -> SchemaEntry {
        SchemaEntry {
            name: "cfgd-config".into(),
            file_match: vec!["cfgd.yaml".into()],
            schema_file: Some("schemas/cfgd-config.schema.json".into()),
            description: Some("cfgd machine configuration".into()),
            ..Default::default()
        }
    }

    /// A minimal upstream catalog containing only the entry that the test
    /// builds, so `verdict` can resolve to NoOp/Update/Add deterministically.
    fn catalog_with(entries: &[Value]) -> String {
        serde_json::to_string_pretty(&serde_json::json!({
            "$schema": "../../schema-catalog.json",
            "version": 1.0,
            "schemas": entries,
        }))
        .unwrap()
    }

    // --- plan_schema (pure) ---------------------------------------------

    #[test]
    fn plan_external_uses_entry_url_and_no_vendor_file() {
        let e = external_entry();
        let plan = plan_schema(&e, "Anodizer config", false, None, None).unwrap();
        assert_eq!(plan.mode, SchemaMode::External);
        assert_eq!(
            plan.url,
            "https://tj-smith47.github.io/anodizer/schema.json"
        );
        assert!(plan.vendor_path.is_none());
        assert!(plan.verdict.is_none(), "no catalog provided ⇒ no verdict");
    }

    #[test]
    fn plan_vendor_derives_slug_url_and_path() {
        let e = vendor_entry();
        let plan = plan_schema(&e, "cfgd machine config", false, None, None).unwrap();
        assert_eq!(plan.mode, SchemaMode::Vendor);
        assert_eq!(plan.url, "https://www.schemastore.org/cfgd-config.json");
        assert_eq!(
            plan.vendor_path.unwrap(),
            PathBuf::from("src/schemas/json/cfgd-config.json")
        );
    }

    #[test]
    fn plan_versioned_vendor_stamps_version_into_filename_and_url() {
        let e = vendor_entry();
        let plan = plan_schema(&e, "cfgd machine config", true, Some("0.4.2"), None).unwrap();
        assert_eq!(
            plan.url,
            "https://www.schemastore.org/cfgd-config-0.4.2.json"
        );
        assert_eq!(
            plan.vendor_path.unwrap(),
            PathBuf::from("src/schemas/json/cfgd-config-0.4.2.json")
        );
        // The versions map carries the new version → versioned url.
        let versions = plan
            .desired_entry
            .get("versions")
            .and_then(Value::as_object);
        assert_eq!(
            versions
                .and_then(|m| m.get("0.4.2"))
                .and_then(Value::as_str),
            Some("https://www.schemastore.org/cfgd-config-0.4.2.json")
        );
    }

    #[test]
    fn plan_versioned_vendor_merges_prior_versions_forward() {
        let e = vendor_entry();
        // Upstream already lists 0.4.1; the new 0.4.2 must not drop it.
        let prior = serde_json::json!({
            "name": "cfgd-config",
            "description": "cfgd machine configuration",
            "fileMatch": ["cfgd.yaml"],
            "url": "https://www.schemastore.org/cfgd-config-0.4.1.json",
            "versions": { "0.4.1": "https://www.schemastore.org/cfgd-config-0.4.1.json" },
        });
        let cat = catalog_with(&[prior]);
        let plan = plan_schema(
            &e,
            "cfgd machine configuration",
            true,
            Some("0.4.2"),
            Some(&cat),
        )
        .unwrap();
        let versions = plan
            .desired_entry
            .get("versions")
            .and_then(Value::as_object)
            .unwrap();
        assert!(
            versions.contains_key("0.4.1"),
            "prior version carried forward"
        );
        assert!(versions.contains_key("0.4.2"), "new version added");
    }

    // --- verdict against a fixture catalog ------------------------------

    #[test]
    fn plan_verdict_is_add_when_absent() {
        let e = external_entry();
        let cat = catalog_with(&[serde_json::json!({
            "name": "SomethingElse",
            "description": "other",
            "fileMatch": ["x.yaml"],
            "url": "https://example.com/x.json",
        })]);
        let plan = plan_schema(
            &e,
            "Anodizer Rust release-automation configuration file",
            false,
            None,
            Some(&cat),
        )
        .unwrap();
        assert_eq!(plan.verdict, Some(catalog::Verdict::Add));
    }

    #[test]
    fn plan_verdict_is_noop_when_identical() {
        let e = external_entry();
        // The catalog already holds the exact desired entry.
        let desired = catalog::build_entry_json(
            &e.name,
            "Anodizer Rust release-automation configuration file",
            &e.file_match,
            e.url.as_deref().unwrap(),
            None,
        );
        let cat = catalog_with(&[desired]);
        let plan = plan_schema(
            &e,
            "Anodizer Rust release-automation configuration file",
            false,
            None,
            Some(&cat),
        )
        .unwrap();
        assert_eq!(plan.verdict, Some(catalog::Verdict::NoOp));
    }

    #[test]
    fn plan_verdict_is_update_when_description_differs() {
        let e = external_entry();
        let stale = catalog::build_entry_json(
            &e.name,
            "an older description",
            &e.file_match,
            e.url.as_deref().unwrap(),
            None,
        );
        let cat = catalog_with(&[stale]);
        let plan = plan_schema(
            &e,
            "Anodizer Rust release-automation configuration file",
            false,
            None,
            Some(&cat),
        )
        .unwrap();
        assert_eq!(plan.verdict, Some(catalog::Verdict::Update));
    }

    // --- dry-run run_publish (NO network) -------------------------------

    #[test]
    fn dry_run_external_logs_planned_line_and_opens_no_pr() {
        let capture = anodizer_core::log::LogCapture::new();
        let mut ctx = TestContextBuilder::new().dry_run(true).build();
        ctx.with_log_capture(capture.clone());
        ctx.config.schemastore = SchemastoreConfig {
            schemas: vec![external_entry()],
            ..Default::default()
        };
        let ev = run_publish(&mut ctx).expect("dry-run external ok");
        assert_eq!(ev.publisher, "schemastore");
        assert_eq!(
            ev.extra,
            anodizer_core::PublishEvidenceExtra::Empty,
            "dry-run records no PR target"
        );
        let msgs: Vec<String> = capture.all_messages().into_iter().map(|(_, m)| m).collect();
        assert!(
            msgs.iter()
                .any(|m| m.contains("would") && m.contains("Anodizer") && m.contains("external")),
            "expected a planned 'would …' line naming the external schema; got {msgs:?}"
        );
    }

    #[test]
    fn dry_run_vendor_logs_vendor_file_path_and_slug() {
        let capture = anodizer_core::log::LogCapture::new();
        let mut ctx = TestContextBuilder::new().dry_run(true).build();
        ctx.with_log_capture(capture.clone());
        ctx.config.schemastore = SchemastoreConfig {
            schemas: vec![vendor_entry()],
            ..Default::default()
        };
        let ev = run_publish(&mut ctx).expect("dry-run vendor ok");
        assert_eq!(ev.extra, anodizer_core::PublishEvidenceExtra::Empty);
        let msgs: Vec<String> = capture.all_messages().into_iter().map(|(_, m)| m).collect();
        assert!(
            msgs.iter().any(|m| m.contains("would")
                && m.contains("cfgd-config")
                && m.contains("src/schemas/json/cfgd-config.json")),
            "expected a planned 'would …' line naming the vendor file path + slug; got {msgs:?}"
        );
    }

    #[test]
    fn dry_run_skips_disabled_entries() {
        use anodizer_core::config::StringOrBool;
        let capture = anodizer_core::log::LogCapture::new();
        let mut ctx = TestContextBuilder::new().dry_run(true).build();
        ctx.with_log_capture(capture.clone());
        let mut entry = external_entry();
        entry.skip = Some(StringOrBool::Bool(true));
        ctx.config.schemastore = SchemastoreConfig {
            schemas: vec![entry],
            ..Default::default()
        };
        let ev = run_publish(&mut ctx).expect("dry-run skip ok");
        assert_eq!(ev.extra, anodizer_core::PublishEvidenceExtra::Empty);
        let msgs: Vec<String> = capture.all_messages().into_iter().map(|(_, m)| m).collect();
        assert!(
            !msgs.iter().any(|m| m.contains("would")),
            "a skipped entry must not produce a planned line; got {msgs:?}"
        );
    }

    // --- evidence shape -------------------------------------------------

    #[test]
    fn schemastore_evidence_carries_pr_target_with_env_var_name_not_value() {
        let ev = schemastore_evidence("tj-smith47", "schemastore-v0.4.2");
        match ev.extra {
            anodizer_core::PublishEvidenceExtra::Schemastore(s) => {
                let t = &s.schemastore_targets[0];
                assert_eq!(t.upstream_owner, "SchemaStore");
                assert_eq!(t.upstream_repo, "schemastore");
                assert_eq!(t.fork_owner, "tj-smith47");
                assert_eq!(t.branch, "schemastore-v0.4.2");
                assert_eq!(t.token_env_var.as_deref(), Some("SCHEMASTORE_TOKEN"));
            }
            other => panic!("expected Schemastore extra, got {other:?}"),
        }
    }
}
