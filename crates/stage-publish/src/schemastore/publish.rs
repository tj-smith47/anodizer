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
    /// Operator-facing hosting-mode label, distinguishing a versioned vendor
    /// from a plain one. Shared by the dry-run log line and the PR body so the
    /// two surfaces never drift.
    fn mode_label(&self) -> &'static str {
        match self.mode {
            SchemaMode::External => "external",
            SchemaMode::Vendor if self.versioned => "vendor, versioned",
            SchemaMode::Vendor => "vendor",
        }
    }

    /// One-line operator-facing summary of the planned action, used by the
    /// dry-run log so an operator sees exactly what a real run would do.
    fn planned_line(&self) -> String {
        let mode = self.mode_label();
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

/// Upstream state the change-decision compares the desired plan against,
/// carrying only the strings the caller fetched (the probe fetches them over
/// HTTP; `run_real` reads them from the synced clone).
///
/// A `None` field means the caller could not obtain that piece. The
/// change-decision treats every `None` it needs as "change required" so a
/// missing fetch can never collapse to a false no-op — see
/// [`schema_change_needed`].
pub(crate) struct RemoteState<'a> {
    /// Upstream `src/api/json/catalog.json` content.
    pub(crate) catalog_json: &'a str,
    /// Upstream `src/schemas/json/<slug>.json` content, or `None` when the
    /// file is absent (404) or was not fetched. Only consulted for vendor.
    pub(crate) vendor_file: Option<&'a str>,
    /// Upstream `src/schema-validation.jsonc` content, or `None` when not
    /// fetched. Only consulted for a too-high-dialect vendor schema.
    pub(crate) jsonc: Option<&'a str>,
}

/// Decide whether publishing `plan` would change the upstream tree.
///
/// This is the SINGLE change-decision shared by the pre-clone network probe
/// ([`probe_remote_all_noop`]) and the authoritative `run_real` path, so the
/// two can never disagree about whether a schema is already current.
///
/// `local_schema` is the locally-formatted vendored schema content (the bytes a
/// real publish would write); pass `None` for external entries, which carry no
/// file.
///
/// A schema is a no-op (returns `false`) ONLY when every required piece is
/// present and matches:
/// - **external:** the catalog entry already matches ([`catalog::verdict`] ⇒
///   `NoOp`). There is no file, so that is the whole story.
/// - **vendor:** the catalog entry matches AND the upstream vendored file byte-
///   equals `local_schema` AND, when the schema's `$schema` dialect is
///   [`Dialect::TooHigh`], the vendored filename is already listed in the
///   upstream `highSchemaVersion` allowlist.
///
/// Any uncertainty is reported as change-needed (`true`): a malformed/absent
/// catalog, a vendor schema whose upstream file was not fetched
/// (`vendor_file: None`), or a too-high vendor whose `schema-validation.jsonc`
/// was not fetched (`jsonc: None`). A no-op verdict is therefore always
/// CERTAIN — never assumed on missing data.
pub(crate) fn schema_change_needed(
    plan: &SchemaPlan,
    local_schema: Option<&str>,
    remote: &RemoteState,
) -> bool {
    // The catalog entry must already match; an `Err` (malformed catalog) is
    // uncertainty ⇒ change needed.
    match catalog::verdict(remote.catalog_json, &plan.name, &plan.desired_entry) {
        Ok(catalog::Verdict::NoOp) => {}
        Ok(catalog::Verdict::Add) | Ok(catalog::Verdict::Update) | Err(_) => return true,
    }

    if plan.mode != SchemaMode::Vendor {
        // External: catalog entry match is sufficient — no file to compare.
        return false;
    }

    // Vendor: the upstream file must byte-equal what we would write. A missing
    // upstream file (None) or missing local content is uncertainty ⇒ change.
    let (Some(local), Some(upstream)) = (local_schema, remote.vendor_file) else {
        return true;
    };
    if local != upstream {
        return true;
    }

    // Too-high dialect: the vendored filename must already be allowlisted. The
    // dialect is read off the local schema (what we'd publish); if the jsonc
    // wasn't fetched, treat as change-needed.
    if raw_dialect(local) == Dialect::TooHigh {
        let Some(jsonc) = remote.jsonc else {
            return true;
        };
        let Ok(allow_name) = allowlist_name_for(plan) else {
            return true;
        };
        if !super::scan::jsonc_array_contains(jsonc, "highSchemaVersion", &allow_name) {
            return true;
        }
    }

    false
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
///
/// Shared by the publish path and preflight so a DERIVED description (the
/// omitted-`description` path) is validated at preflight exactly as it will be
/// at publish time — the two surfaces can never disagree on what passes.
pub(crate) fn resolve_description(ctx: &Context, entry: &SchemaEntry) -> anyhow::Result<String> {
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

    // Steady-state fast path: probe the upstream catalog (one HTTP GET, plus a
    // GET per vendor file) BEFORE cloning. When every schema is a CERTAIN no-op
    // the publish needs no fork, no clone, and no token — anodizer's own
    // external entry is a no-op every release. The probe is best-effort: any
    // fetch failure or uncertainty falls through to the authoritative
    // `run_real`, which re-derives from the synced clone.
    if probe_remote_all_noop(ctx, &cfg, &effective, &log) {
        log.status(&format!(
            "schemastore: all {} schema(s) already current upstream — nothing to publish (no clone)",
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
fn probe_remote_all_noop(
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
                "schemastore: pre-clone catalog probe skipped ({e}); proceeding to clone"
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
fn fetch_raw_required(client: &reqwest::blocking::Client, url: &str) -> anyhow::Result<String> {
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
fn fetch_raw_optional(
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
                "schemastore: `{}` already registered and current — no change",
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

        catalog_json = catalog::splice_entry(&catalog_json, &plan.name, &plan.desired_entry)
            .with_context(|| format!("schemastore: splice catalog entry for `{}`", plan.name))?;
        applied.push(plan);
    }

    if applied.is_empty() {
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

    let commit_msg = schemastore_commit_msg(&applied);
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

/// Read the LOCAL vendor schema off `project_root` and reformat it to
/// SchemaStore's prettier defaults — the exact bytes a publish would write.
///
/// Shared by the change-decision (which byte-compares this against the upstream
/// copy) and the write path, so the content that gates the no-op and the
/// content that lands in the PR are derived identically.
fn read_local_vendor_schema(
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
fn read_cloned_vendor_file(repo_path: &std::path::Path, plan: &SchemaPlan) -> Option<String> {
    let rel = plan.vendor_path.as_ref()?;
    std::fs::read_to_string(repo_path.join(rel)).ok()
}

/// Read the upstream `schema-validation.jsonc` from the cloned tree, or `None`
/// when it is missing (the change-decision reads `None` for a too-high schema
/// as "couldn't confirm the allowlist ⇒ change needed").
fn read_cloned_jsonc(repo_path: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(repo_path.join(DIALECT_ALLOWLIST_PATH)).ok()
}

/// Write the already-formatted vendor schema `formatted` into the cloned repo.
/// When the schema's `$schema` dialect is too high for SchemaStore's CI,
/// allowlist its vendored filename in `schema-validation.jsonc` in the same PR.
fn write_vendor_schema(
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
        "schemastore: vendored `{}` → {}",
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
            "schemastore: allowlisted high-dialect schema `{allow_name}` in {DIALECT_ALLOWLIST_PATH}"
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

/// The `highSchemaVersion` allowlist key for a vendor plan: the vendored
/// file's basename **including** the `.json` extension (`cfgd-module.json` for
/// a plain vendor, `cfgd-module-0.4.2.json` for a versioned one).
///
/// SchemaStore's CI matches this allowlist against `path.basename(schemaPath)`
/// — the vendored filename — never the catalog display name, so the key must
/// be derived from [`SchemaPlan::vendor_path`], not [`SchemaPlan::name`].
fn allowlist_name_for(plan: &SchemaPlan) -> anyhow::Result<String> {
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
fn schemastore_commit_msg(applied: &[SchemaPlan]) -> String {
    let names: Vec<&str> = applied.iter().map(|p| p.name.as_str()).collect();
    format!("Register/refresh {}", names.join(", "))
}

/// PR title naming the registered schemas.
fn schemastore_pr_title(applied: &[SchemaPlan]) -> String {
    let names: Vec<&str> = applied.iter().map(|p| p.name.as_str()).collect();
    format!("Add/update {} schema(s)", names.join(", "))
}

/// PR body listing each registered schema's name, hosting mode, and url.
/// Built from the already-computed [`SchemaPlan`]s so the mode (including
/// `vendor, versioned`) is the proven one — never re-derived.
fn schemastore_pr_body(applied: &[SchemaPlan]) -> String {
    let mut body = String::from("## Schemas\n");
    for p in applied {
        body.push_str(&format!(
            "- **{}** ({}) → {}\n",
            p.name,
            p.mode_label(),
            p.url
        ));
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

    // --- allowlist key derivation ---------------------------------------

    #[test]
    fn allowlist_key_is_vendor_filename_with_json_extension() {
        // The catalog display name here is the Title-case `cfgd-module`, but the
        // allowlist key must be the vendored file basename WITH `.json` so
        // SchemaStore's `path.basename` match succeeds — never the display name.
        let e = SchemaEntry {
            name: "cfgd-module".into(),
            file_match: vec!["cfgd.yaml".into()],
            schema_file: Some("schemas/cfgd-module.schema.json".into()),
            description: Some("cfgd module configuration".into()),
            ..Default::default()
        };
        let plan = plan_schema(&e, "cfgd module configuration", false, None, None).unwrap();
        assert_eq!(allowlist_name_for(&plan).unwrap(), "cfgd-module.json");
        assert_ne!(allowlist_name_for(&plan).unwrap(), "cfgd-module");
    }

    #[test]
    fn allowlist_key_is_versioned_vendor_filename() {
        let e = SchemaEntry {
            name: "cfgd-module".into(),
            file_match: vec!["cfgd.yaml".into()],
            schema_file: Some("schemas/cfgd-module.schema.json".into()),
            description: Some("cfgd module configuration".into()),
            ..Default::default()
        };
        let plan = plan_schema(&e, "cfgd module configuration", true, Some("0.4.2"), None).unwrap();
        assert_eq!(allowlist_name_for(&plan).unwrap(), "cfgd-module-0.4.2.json");
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

    // --- schema_change_needed (pure, shared by probe + run_real) --------
    //
    // This is the SINGLE change-decision both the pre-clone probe and
    // `run_real` gate on. Its no-op result must be CERTAIN: every uncertainty
    // (missing file, unfetched jsonc, malformed catalog) yields change-needed.

    /// Build a vendor plan + its locally-formatted schema content, plus a
    /// catalog that already holds the exact desired entry (so the catalog half
    /// of the decision is a no-op and the file/dialect half is under test).
    fn vendor_plan_with_matching_catalog(schema_src: &str) -> (SchemaPlan, String, String) {
        let e = vendor_entry();
        let plan = plan_schema(&e, "cfgd machine config", false, None, None).unwrap();
        let cat = catalog_with(std::slice::from_ref(&plan.desired_entry));
        let local = manifest::format_vendor_schema(schema_src).unwrap();
        (plan, cat, local)
    }

    /// A draft-07 schema body (dialect `Ok` ⇒ no allowlist needed).
    const DRAFT07_SCHEMA: &str =
        r#"{"$schema":"https://json-schema.org/draft-07/schema#","type":"object"}"#;
    /// A 2020-12 schema body (dialect `TooHigh` ⇒ allowlist required).
    const DRAFT2020_SCHEMA: &str =
        r#"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object"}"#;

    #[test]
    fn change_needed_external_entry_match_is_noop() {
        let e = external_entry();
        let plan = plan_schema(&e, "Anodizer config", false, None, None).unwrap();
        let cat = catalog_with(std::slice::from_ref(&plan.desired_entry));
        let remote = RemoteState {
            catalog_json: &cat,
            vendor_file: None,
            jsonc: None,
        };
        assert!(
            !schema_change_needed(&plan, None, &remote),
            "external entry matching the catalog must be a no-op"
        );
    }

    #[test]
    fn change_needed_external_entry_absent_needs_change() {
        let e = external_entry();
        let plan = plan_schema(&e, "Anodizer config", false, None, None).unwrap();
        let cat = catalog_with(&[]); // entry absent ⇒ Add
        let remote = RemoteState {
            catalog_json: &cat,
            vendor_file: None,
            jsonc: None,
        };
        assert!(schema_change_needed(&plan, None, &remote));
    }

    #[test]
    fn change_needed_external_entry_differs_needs_change() {
        let e = external_entry();
        let plan = plan_schema(&e, "Anodizer config", false, None, None).unwrap();
        // Same name, different description ⇒ Update.
        let stale = catalog::build_entry_json(
            &e.name,
            "an older description",
            &e.file_match,
            e.url.as_deref().unwrap(),
            None,
        );
        let cat = catalog_with(&[stale]);
        let remote = RemoteState {
            catalog_json: &cat,
            vendor_file: None,
            jsonc: None,
        };
        assert!(schema_change_needed(&plan, None, &remote));
    }

    #[test]
    fn change_needed_vendor_entry_and_file_match_is_noop() {
        let (plan, cat, local) = vendor_plan_with_matching_catalog(DRAFT07_SCHEMA);
        // Upstream file byte-equals the locally-formatted content.
        let remote = RemoteState {
            catalog_json: &cat,
            vendor_file: Some(&local),
            jsonc: None,
        };
        assert!(
            !schema_change_needed(&plan, Some(&local), &remote),
            "vendor entry + file both matching (draft-07, no allowlist) ⇒ no-op"
        );
    }

    /// The latent-bug regression test: the catalog entry matches but the
    /// vendored FILE content drifted. The old code gated on the catalog-entry
    /// verdict alone (`catalog::verdict` ⇒ NoOp) and never re-pushed the file.
    /// `schema_change_needed` compares the file too ⇒ change-needed.
    #[test]
    fn change_needed_vendor_file_drift_with_matching_catalog_needs_change() {
        let (plan, cat, local) = vendor_plan_with_matching_catalog(DRAFT07_SCHEMA);
        // Prove the catalog half alone would have been a false no-op.
        assert_eq!(
            catalog::verdict(&cat, &plan.name, &plan.desired_entry).unwrap(),
            catalog::Verdict::NoOp,
            "precondition: catalog entry matches ⇒ entry-only verdict is NoOp"
        );
        // Upstream file content differs from the local formatted content.
        let drifted_upstream = manifest::format_vendor_schema(
            r#"{"$schema":"https://json-schema.org/draft-07/schema#","type":"string"}"#,
        )
        .unwrap();
        assert_ne!(local, drifted_upstream, "fixture must actually differ");
        let remote = RemoteState {
            catalog_json: &cat,
            vendor_file: Some(&drifted_upstream),
            jsonc: None,
        };
        assert!(
            schema_change_needed(&plan, Some(&local), &remote),
            "vendor file content drift MUST trigger a change even when the \
             catalog entry is unchanged — this is the latent-bug regression"
        );
    }

    #[test]
    fn change_needed_vendor_missing_upstream_file_is_conservative_change() {
        let (plan, cat, local) = vendor_plan_with_matching_catalog(DRAFT07_SCHEMA);
        // Upstream file unfetched / absent ⇒ never a false no-op.
        let remote = RemoteState {
            catalog_json: &cat,
            vendor_file: None,
            jsonc: None,
        };
        assert!(
            schema_change_needed(&plan, Some(&local), &remote),
            "a vendor schema with no upstream file content is uncertain ⇒ change"
        );
    }

    #[test]
    fn change_needed_too_high_with_allowlisted_filename_is_noop() {
        let (plan, cat, local) = vendor_plan_with_matching_catalog(DRAFT2020_SCHEMA);
        let allow_name = allowlist_name_for(&plan).unwrap(); // cfgd-config.json
        let jsonc = format!(
            "{{\n  // dialects\n  \"highSchemaVersion\": [\n    \"{allow_name}\"\n  ]\n}}\n"
        );
        let remote = RemoteState {
            catalog_json: &cat,
            vendor_file: Some(&local),
            jsonc: Some(&jsonc),
        };
        assert!(
            !schema_change_needed(&plan, Some(&local), &remote),
            "too-high vendor whose filename is already allowlisted ⇒ no-op"
        );
    }

    #[test]
    fn change_needed_too_high_missing_from_allowlist_needs_change() {
        let (plan, cat, local) = vendor_plan_with_matching_catalog(DRAFT2020_SCHEMA);
        // Allowlist present but does NOT contain the vendored filename.
        let jsonc = "{\n  \"highSchemaVersion\": [\n    \"something-else.json\"\n  ]\n}\n";
        let remote = RemoteState {
            catalog_json: &cat,
            vendor_file: Some(&local),
            jsonc: Some(jsonc),
        };
        assert!(
            schema_change_needed(&plan, Some(&local), &remote),
            "too-high vendor missing from the allowlist must be a change"
        );
    }

    #[test]
    fn change_needed_too_high_unfetched_jsonc_is_conservative_change() {
        let (plan, cat, local) = vendor_plan_with_matching_catalog(DRAFT2020_SCHEMA);
        // Entry + file match, but the allowlist could not be fetched (None) ⇒
        // can't confirm the dialect is allowlisted ⇒ conservative change.
        let remote = RemoteState {
            catalog_json: &cat,
            vendor_file: Some(&local),
            jsonc: None,
        };
        assert!(
            schema_change_needed(&plan, Some(&local), &remote),
            "too-high vendor with unfetched jsonc is uncertain ⇒ change"
        );
    }

    #[test]
    fn change_needed_malformed_catalog_is_conservative_change() {
        let e = external_entry();
        let plan = plan_schema(&e, "Anodizer config", false, None, None).unwrap();
        let remote = RemoteState {
            catalog_json: "{ not valid json",
            vendor_file: None,
            jsonc: None,
        };
        assert!(
            schema_change_needed(&plan, None, &remote),
            "a malformed catalog is uncertainty ⇒ change, never a false no-op"
        );
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

    #[test]
    fn dry_run_if_false_filters_entry() {
        let capture = anodizer_core::log::LogCapture::new();
        let mut ctx = TestContextBuilder::new().dry_run(true).build();
        ctx.with_log_capture(capture.clone());
        let mut entry = external_entry();
        // A falsy `if:` must filter the entry out of the effective set, the
        // same as `skip:` — exercising the `resolved_if` falsy branch.
        entry.if_condition = Some("false".into());
        ctx.config.schemastore = SchemastoreConfig {
            schemas: vec![entry],
            ..Default::default()
        };
        let ev = run_publish(&mut ctx).expect("dry-run if-false ok");
        assert_eq!(ev.extra, anodizer_core::PublishEvidenceExtra::Empty);
        let msgs: Vec<String> = capture.all_messages().into_iter().map(|(_, m)| m).collect();
        assert!(
            !msgs.iter().any(|m| m.contains("would")),
            "an `if: false` entry must not produce a planned line; got {msgs:?}"
        );
    }

    #[test]
    fn empty_effective_set_returns_empty_evidence_and_logs_no_schemas() {
        let capture = anodizer_core::log::LogCapture::new();
        // Not dry-run: the early return must fire BEFORE any network/fork path,
        // proving the empty-set guard short-circuits regardless of mode.
        let mut ctx = TestContextBuilder::new().build();
        ctx.with_log_capture(capture.clone());
        ctx.config.schemastore = SchemastoreConfig {
            schemas: vec![],
            ..Default::default()
        };
        let ev = run_publish(&mut ctx).expect("empty schemas ok");
        assert_eq!(ev.publisher, "schemastore");
        assert_eq!(ev.extra, anodizer_core::PublishEvidenceExtra::Empty);
        let msgs: Vec<String> = capture.all_messages().into_iter().map(|(_, m)| m).collect();
        assert!(
            msgs.iter().any(|m| m.contains("no schemas to register")),
            "expected the 'no schemas to register' status line; got {msgs:?}"
        );
    }

    // --- resolve_description (both branches) ----------------------------

    #[test]
    fn resolve_description_derives_from_project_metadata_when_unset() {
        let mut ctx = TestContextBuilder::new().build();
        ctx.config.metadata = Some(anodizer_core::config::MetadataConfig {
            description: Some("derived project config".into()),
            ..Default::default()
        });
        let mut entry = external_entry();
        entry.description = None; // force the metadata-derivation branch
        let desc = resolve_description(&ctx, &entry).expect("derive + sanitize ok");
        assert_eq!(desc, "derived project config");
    }

    #[test]
    fn resolve_description_bails_when_nothing_derivable() {
        // No entry description and no project/crate metadata → the error path.
        let ctx = TestContextBuilder::new().build();
        let mut entry = external_entry();
        entry.description = None;
        let err = resolve_description(&ctx, &entry)
            .expect_err("must bail when no description is derivable");
        let msg = err.to_string();
        assert!(
            msg.contains("Anodizer") && msg.contains("no description"),
            "expected an actionable no-description error; got {msg}"
        );
    }

    // --- PR body distinguishes vendor/versioned -------------------------

    #[test]
    fn pr_body_labels_external_vendor_and_versioned_distinctly() {
        let external =
            plan_schema(&external_entry(), "Anodizer config", false, None, None).unwrap();
        let vendor =
            plan_schema(&vendor_entry(), "cfgd machine config", false, None, None).unwrap();
        let versioned = plan_schema(
            &vendor_entry(),
            "cfgd machine config",
            true,
            Some("0.4.2"),
            None,
        )
        .unwrap();
        let body = schemastore_pr_body(&[external, vendor, versioned]);
        assert!(body.contains("**Anodizer** (external)"), "{body}");
        assert!(body.contains("**cfgd-config** (vendor) →"), "{body}");
        assert!(
            body.contains("**cfgd-config** (vendor, versioned) → https://www.schemastore.org/cfgd-config-0.4.2.json"),
            "versioned vendor must be labeled distinctly with its versioned url; got {body}"
        );
    }

    // --- per-crate version scope across config modes --------------------
    //
    // A versioned vendor schema stamps `<VER>` from the SCHEMA'S OWN crate's
    // tag — never crate[0]'s — in every config mode. The all-config-modes axis
    // is the canonical anodizer bug surface (flat/clobbering/first-crate
    // resolution), so each mode gets an executable proof. `plan_schema_scoped`
    // drives the real `with_published_crate_scope` → `resolve_crate_tag` path
    // against a git fixture, so a regression of the scope to crate[0] would
    // change the asserted `<VER>` and fail the test.

    /// Build a versioned vendor entry bound to `crate_name`.
    fn versioned_vendor_entry(crate_name: &str) -> SchemaEntry {
        SchemaEntry {
            name: "cfgd-config".into(),
            slug: Some("cfgd-config".into()),
            file_match: vec!["cfgd.yaml".into()],
            schema_file: Some("schemas/cfgd-config.schema.json".into()),
            crate_: Some(crate_name.into()),
            versioned: Some(true),
            description: Some("cfgd machine configuration".into()),
            ..Default::default()
        }
    }

    fn crate_cfg(name: &str, tag_template: &str) -> anodizer_core::config::CrateConfig {
        anodizer_core::config::CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: tag_template.to_string(),
            ..Default::default()
        }
    }

    /// The schema's crate (`cfgd`, tag `cfgd-v2.0.0` ⇒ 2.0.0) is the SECOND
    /// crate; crate[0] is `cfgd-core` (tag `cfgd-core-v1.0.0` ⇒ 1.0.0). A
    /// versioned vendor schema must stamp the bound crate's own version
    /// (2.0.0) — if the scope regressed to crate[0], `<VER>` would be 1.0.0.
    #[test]
    fn per_crate_mode_stamps_schema_crate_version_not_crate_zero() {
        // Independent per-crate tags on a hermetic repo so the production
        // `resolve_crate_tag` path resolves each crate's OWN version.
        let repo = crate::testing::hermetic_repo_with_tags(&["cfgd-core-v1.0.0", "cfgd-v2.0.0"]);
        let mut ctx = TestContextBuilder::new()
            .crates(vec![
                crate_cfg("cfgd-core", "cfgd-core-v{{ .Version }}"),
                crate_cfg("cfgd", "cfgd-v{{ .Version }}"),
            ])
            .project_root(repo.path().to_path_buf())
            .build();

        let cfg = SchemastoreConfig::default();
        let entry = versioned_vendor_entry("cfgd");
        let plan = plan_schema_scoped(&mut ctx, &cfg, &entry, "cfgd machine configuration", None)
            .expect("plan_schema_scoped for per-crate versioned vendor");

        assert_eq!(
            plan.url, "https://www.schemastore.org/cfgd-config-2.0.0.json",
            "expected cfgd's own version 2.0.0 in the catalog url; \
             a scope regressed to crate[0] would yield cfgd-core's 1.0.0"
        );
        assert_eq!(
            plan.vendor_path.as_ref().unwrap(),
            &PathBuf::from("src/schemas/json/cfgd-config-2.0.0.json"),
            "expected the vendor filename stamped with cfgd's own 2.0.0"
        );
        assert!(
            !plan.url.contains("1.0.0"),
            "the schema's crate version (cfgd@2.0.0) must NOT be crate[0]'s \
             (cfgd-core@1.0.0); url was {}",
            plan.url
        );
        let versions = plan
            .desired_entry
            .get("versions")
            .and_then(Value::as_object)
            .expect("versioned entry carries a versions map");
        assert!(
            versions.contains_key("2.0.0"),
            "versions map keyed by cfgd's own 2.0.0; got {versions:?}"
        );
        assert!(
            !versions.contains_key("1.0.0"),
            "versions map must NOT carry crate[0]'s 1.0.0; got {versions:?}"
        );
    }

    /// Single-crate mode: one crate `mytool` tagged `v3.1.0`. A versioned
    /// vendor schema (crate unset ⇒ defaults to the sole crate) stamps 3.1.0.
    #[test]
    fn single_crate_mode_stamps_sole_crate_version() {
        let repo = crate::testing::hermetic_repo_with_tags(&["v3.1.0"]);
        let mut ctx = TestContextBuilder::new()
            .crates(vec![crate_cfg("mytool", "v{{ .Version }}")])
            .project_root(repo.path().to_path_buf())
            .build();

        let cfg = SchemastoreConfig::default();
        // `crate` unset: `plan_schema_scoped` binds the version to the sole
        // crate via the all_crates().first() fallback.
        let mut entry = versioned_vendor_entry("mytool");
        entry.crate_ = None;
        let plan = plan_schema_scoped(&mut ctx, &cfg, &entry, "mytool configuration", None)
            .expect("plan_schema_scoped for single-crate versioned vendor");

        assert_eq!(
            plan.url, "https://www.schemastore.org/cfgd-config-3.1.0.json",
            "single-crate versioned vendor must stamp the sole crate's 3.1.0"
        );
        assert_eq!(
            plan.vendor_path.as_ref().unwrap(),
            &PathBuf::from("src/schemas/json/cfgd-config-3.1.0.json"),
        );
    }

    /// Lockstep mode: two crates share ONE tag `v4.0.0`. A versioned schema
    /// bound to the SECOND crate must still resolve the shared 4.0.0,
    /// proving lockstep resolution is independent of which crate is named.
    #[test]
    fn lockstep_mode_stamps_shared_version_regardless_of_named_crate() {
        // Both crates use the same `v{{ .Version }}` template, so the single
        // `v4.0.0` tag resolves identically for either crate.
        let repo = crate::testing::hermetic_repo_with_tags(&["v4.0.0"]);
        let mut ctx = TestContextBuilder::new()
            .crates(vec![
                crate_cfg("alpha", "v{{ .Version }}"),
                crate_cfg("beta", "v{{ .Version }}"),
            ])
            .project_root(repo.path().to_path_buf())
            .build();

        let cfg = SchemastoreConfig::default();
        let entry = versioned_vendor_entry("beta");
        let plan = plan_schema_scoped(&mut ctx, &cfg, &entry, "beta configuration", None)
            .expect("plan_schema_scoped for lockstep versioned vendor");

        assert_eq!(
            plan.url, "https://www.schemastore.org/cfgd-config-4.0.0.json",
            "lockstep versioned vendor must stamp the shared 4.0.0 even for \
             the second-named crate"
        );
        assert_eq!(
            plan.vendor_path.as_ref().unwrap(),
            &PathBuf::from("src/schemas/json/cfgd-config-4.0.0.json"),
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
