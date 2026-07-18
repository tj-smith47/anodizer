use std::path::PathBuf;

use anodizer_core::config::{SchemaEntry, SchemaMode, SchemastoreConfig};
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use serde_json::Value;

use super::super::manifest::{self, Dialect};
use super::super::{catalog, entry_label};

use super::*;

/// Canonical upstream the SchemaStore PR targets.
pub(super) const UPSTREAM_OWNER: &str = "SchemaStore";
pub(super) const UPSTREAM_REPO: &str = "schemastore";
/// Default branch of `SchemaStore/schemastore`. The fork drifts behind, so the
/// work branch is synced to this before splicing (see [`run_publish`]).
pub(super) const UPSTREAM_DEFAULT_BRANCH: &str = "master";
/// Repo-relative path of the catalog the publisher splices entries into.
pub(super) const CATALOG_PATH: &str = "src/api/json/catalog.json";
/// Repo-relative path of the dialect allowlist (`highSchemaVersion`).
pub(super) const DIALECT_ALLOWLIST_PATH: &str = "src/schema-validation.jsonc";
/// Env var the rollback path consults for the close-PR token.
pub(super) const TOKEN_ENV_VAR: &str = "SCHEMASTORE_TOKEN";

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
    pub(super) fn mode_label(&self) -> &'static str {
        match self.mode {
            SchemaMode::External => "external",
            SchemaMode::Vendor if self.versioned => "vendor, versioned",
            SchemaMode::Vendor => "vendor",
        }
    }

    /// One-line operator-facing summary of the planned action, used by the
    /// dry-run log so an operator sees exactly what a real run would do.
    pub(super) fn planned_line(&self) -> String {
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
                .and_then(|c| catalog::upstream_versions_by_file_match(c, &entry.file_match))
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
        Some(c) => Some(catalog::verdict(c, &desired_entry)?),
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
    match catalog::verdict(remote.catalog_json, &plan.desired_entry) {
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
        if !super::super::scan::jsonc_array_contains(jsonc, "highSchemaVersion", &allow_name) {
            return true;
        }
    }

    false
}

/// Effective schemas after the per-entry `skip` and `if:` gates, paired with
/// the resolved description for each. Returns an error if a description cannot
/// be derived or fails the content rules (preflight already checks this, but
/// the publish path must not assume preflight ran).
pub(super) fn effective_schemas<'a>(
    ctx: &Context,
    cfg: &'a SchemastoreConfig,
    log: &StageLogger,
) -> anyhow::Result<Vec<(&'a SchemaEntry, String)>> {
    let mut out = Vec::new();
    for entry in &cfg.schemas {
        if cfg.resolved_skip(entry) {
            continue;
        }
        // A schema bound to a crate absent from THIS leg's universe belongs to
        // another leg (per-crate / workspace-split publish runs each leg with
        // a config whose crate universe holds only that leg's crates). Skip
        // before `resolve_description`, which would otherwise choke trying to
        // derive metadata for a crate this leg can't see.
        if let Some(crate_name) = entry.crate_.as_deref()
            && ctx.config.find_crate(crate_name).is_none()
        {
            log.verbose(&format!(
                "{}: binds crate '{crate_name}' not in this leg's crate universe; \
                 skipping (its owning leg publishes it)",
                entry_label(&entry.name)
            ));
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
