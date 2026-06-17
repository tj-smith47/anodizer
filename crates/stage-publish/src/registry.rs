//! Publisher registry — single source of truth for which publishers run.
//!
//! [`configured_publishers`] walks the active [`Context`] and instantiates
//! a `Box<dyn Publisher>` for each configured publisher. The returned slice
//! is what [`crate::dispatch::dispatch`] iterates over.
//!
//! The registry is populated incrementally by the per-publisher migration
//! tasks. The existing macro-driven `PublishStage::run` body continues to
//! dispatch publishers until those migrations are complete; this module +
//! [`crate::dispatch`] live alongside it and are exercised only by tests
//! until the swap lands.

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::{Publisher, PublisherGroup};

/// Collapse a set of per-crate / per-entry `required` overrides into one
/// publisher-level value, escalating to `true`.
///
/// `required` is a safety gate: when any crate (or any top-level entry) marks a
/// publisher `required: true`, a failure there must fail the release — so a
/// single `Some(true)` wins over every `Some(false)`. Returns `Some(false)`
/// only when at least one override is present and none is `true`; `None` when
/// no override is set anywhere (the publisher keeps its built-in default).
///
/// This replaces a first-non-None `find_map` collapse, under which an earlier
/// crate's `required: false` would silently mask a later crate's `true` and
/// drop the safety gate.
fn collapse_required(overrides: impl Iterator<Item = Option<bool>>) -> Option<bool> {
    let mut result: Option<bool> = None;
    for o in overrides {
        match o {
            Some(true) => return Some(true),
            Some(false) => result = Some(false),
            None => {}
        }
    }
    result
}

/// Returns the publishers configured for this release run.
///
/// Walks `ctx.config.crates[*].publish` and the top-level publisher blocks
/// (`dockerhub`, `artifactories`, `cloudsmiths`) and instantiates a
/// `Box<dyn Publisher>` for each configured publisher. The returned slice
/// is the single source of truth that [`crate::dispatch::dispatch`]
/// iterates.
///
/// These publishers run via the trait registry. Blob and Snapcraft do NOT
/// — they own their own pipeline stages (`BlobStage`,
/// `SnapcraftPublishStage`) and record their outcomes directly into
/// `ctx.publish_report`. Registering trait-based wrappers here would fire
/// the underlying upload (`object_store::put` for blob, `snapcraft upload`
/// for snapcraft) a second time per release. See
/// `crates/stage-blob/src/run.rs::record_blob_result` and
/// `crates/stage-snapcraft/src/publish_stage.rs::record_snapcraft_result`
/// for the precedent.
///
/// The `BlobPublisher` trait impl in `stage-blob` stays for forward-compat
/// (and as a vocabulary entry the dispatch path can consult once the
/// publisher dispatch path can replace the dedicated stage entirely).
pub fn configured_publishers(ctx: &Context) -> Vec<Box<dyn Publisher>> {
    let mut v: Vec<Box<dyn Publisher>> = Vec::new();
    if is_cargo_configured(ctx) {
        // Escalate-to-true across crates: any crate's `required: true` wins.
        let req = collapse_required(
            ctx.config
                .crates
                .iter()
                .map(|c| c.publish.as_ref().and_then(|p| p.cargo.as_ref()?.required)),
        );
        let retain = collapse_required(ctx.config.crates.iter().map(|c| {
            c.publish
                .as_ref()
                .and_then(|p| p.cargo.as_ref()?.retain_on_rollback)
        }));
        v.push(Box::new(crate::cargo::CargoPublisher::with_overrides(
            req, retain,
        )));
    }
    // Assets group: dockerhub, artifactory, cloudsmith.
    // `blob` is also Assets-group but runs as its own `BlobStage` (see
    // doc on `configured_publishers` above for why it's not registered).
    if is_dockerhub_configured(ctx) {
        // Escalate-to-true across `dockerhub:` entries.
        let req = collapse_required(ctx.config.dockerhub.iter().flatten().map(|c| c.required));
        let retain = collapse_required(
            ctx.config
                .dockerhub
                .iter()
                .flatten()
                .map(|c| c.retain_on_rollback),
        );
        v.push(Box::new(
            crate::dockerhub::DockerhubPublisher::with_overrides(req, retain),
        ));
    }
    if is_artifactory_configured(ctx) {
        // Escalate-to-true across `artifactories:` entries.
        let req = collapse_required(
            ctx.config
                .artifactories
                .iter()
                .flatten()
                .map(|c| c.required),
        );
        let retain = collapse_required(
            ctx.config
                .artifactories
                .iter()
                .flatten()
                .map(|c| c.retain_on_rollback),
        );
        v.push(Box::new(
            crate::artifactory::ArtifactoryPublisher::with_overrides(req, retain),
        ));
    }
    if is_uploads_configured(ctx) {
        // Escalate-to-true across `uploads:` entries.
        let req = collapse_required(ctx.config.uploads.iter().flatten().map(|c| c.required));
        let retain = collapse_required(
            ctx.config
                .uploads
                .iter()
                .flatten()
                .map(|c| c.retain_on_rollback),
        );
        v.push(Box::new(crate::uploads::UploadsPublisher::with_overrides(
            req, retain,
        )));
    }
    if is_cloudsmith_configured(ctx) {
        // Escalate-to-true across `cloudsmiths:` entries.
        let req = collapse_required(ctx.config.cloudsmiths.iter().flatten().map(|c| c.required));
        let retain = collapse_required(
            ctx.config
                .cloudsmiths
                .iter()
                .flatten()
                .map(|c| c.retain_on_rollback),
        );
        v.push(Box::new(
            crate::cloudsmith::CloudsmithPublisher::with_overrides(req, retain),
        ));
    }
    if is_github_release_configured(ctx) {
        // Escalate-to-true across crates' `release.required`.
        let req = collapse_required(
            ctx.config
                .crates
                .iter()
                .map(|c| c.release.as_ref().and_then(|r| r.required)),
        );
        let retain = collapse_required(
            ctx.config
                .crates
                .iter()
                .map(|c| c.release.as_ref().and_then(|r| r.retain_on_rollback)),
        );
        v.push(Box::new(
            anodizer_stage_release::publisher::GithubReleasePublisher::with_overrides(req, retain),
        ));
    }
    // Manager group — git-revert rollback against publisher-owned repo.
    if is_homebrew_configured(ctx) {
        // Escalate-to-true across per-crate `publish.homebrew.required` AND
        // top-level `homebrew_casks:` entries: a `required: true` anywhere
        // (formula or cask config) wins, so a cask-only setup with no per-crate
        // publish block can still escalate the gate.
        let per_crate_req = ctx.config.crates.iter().map(|c| {
            c.publish
                .as_ref()
                .and_then(|p| p.homebrew.as_ref()?.required)
        });
        let casks_req = ctx
            .config
            .homebrew_casks
            .iter()
            .flatten()
            .map(|c| c.required);
        let req = collapse_required(per_crate_req.chain(casks_req));
        let per_crate_retain = ctx.config.crates.iter().map(|c| {
            c.publish
                .as_ref()
                .and_then(|p| p.homebrew.as_ref()?.retain_on_rollback)
        });
        let casks_retain = ctx
            .config
            .homebrew_casks
            .iter()
            .flatten()
            .map(|c| c.retain_on_rollback);
        let retain = collapse_required(per_crate_retain.chain(casks_retain));
        v.push(Box::new(
            crate::homebrew::publisher::HomebrewPublisher::with_overrides(req, retain),
        ));
    }
    if is_scoop_configured(ctx) {
        // Escalate-to-true across crates.
        let req = collapse_required(
            ctx.config
                .crates
                .iter()
                .map(|c| c.publish.as_ref().and_then(|p| p.scoop.as_ref()?.required)),
        );
        let retain = collapse_required(ctx.config.crates.iter().map(|c| {
            c.publish
                .as_ref()
                .and_then(|p| p.scoop.as_ref()?.retain_on_rollback)
        }));
        v.push(Box::new(crate::scoop::ScoopPublisher::with_overrides(
            req, retain,
        )));
    }
    if is_nix_configured(ctx) {
        // Escalate-to-true across crates.
        let req = collapse_required(
            ctx.config
                .crates
                .iter()
                .map(|c| c.publish.as_ref().and_then(|p| p.nix.as_ref()?.required)),
        );
        let retain = collapse_required(ctx.config.crates.iter().map(|c| {
            c.publish
                .as_ref()
                .and_then(|p| p.nix.as_ref()?.retain_on_rollback)
        }));
        v.push(Box::new(
            crate::nix::publisher::NixPublisher::with_overrides(req, retain),
        ));
    }
    if is_aur_configured(ctx) {
        // Escalate-to-true across crates.
        let req = collapse_required(
            ctx.config
                .crates
                .iter()
                .map(|c| c.publish.as_ref().and_then(|p| p.aur.as_ref()?.required)),
        );
        let retain = collapse_required(ctx.config.crates.iter().map(|c| {
            c.publish
                .as_ref()
                .and_then(|p| p.aur.as_ref()?.retain_on_rollback)
        }));
        v.push(Box::new(crate::aur::AurOurPublisher::with_overrides(
            req, retain,
        )));
    }
    // Manager group — close-PR / registry rollback.
    if is_krew_configured(ctx) {
        // Escalate-to-true across crates.
        let req = collapse_required(
            ctx.config
                .crates
                .iter()
                .map(|c| c.publish.as_ref().and_then(|p| p.krew.as_ref()?.required)),
        );
        let retain = collapse_required(ctx.config.crates.iter().map(|c| {
            c.publish
                .as_ref()
                .and_then(|p| p.krew.as_ref()?.retain_on_rollback)
        }));
        v.push(Box::new(crate::krew::KrewPublisher::with_overrides(
            req, retain,
        )));
    }
    if is_mcp_configured(ctx) {
        // mcp is single top-level config — no precedence to resolve.
        let req = ctx.config.mcp.required;
        let retain = ctx.config.mcp.retain_on_rollback;
        v.push(Box::new(
            crate::mcp::publisher::McpPublisher::with_overrides(req, retain),
        ));
    }
    if is_schemastore_configured(ctx) {
        // Escalate-to-true across `schemastore.schemas[]` entries. One block →
        // one publisher; it iterates its own schemas internally.
        let req = collapse_required(ctx.config.schemastore.schemas.iter().map(|s| s.required));
        let retain = ctx.config.schemastore.retain_on_rollback;
        v.push(Box::new(
            crate::schemastore::SchemastorePublisher::with_overrides(req, retain),
        ));
    }
    if is_npm_configured(ctx) {
        // Escalate-to-true across `npms:` entries.
        let req = collapse_required(ctx.config.npms.iter().flatten().map(|c| c.required));
        let retain = collapse_required(
            ctx.config
                .npms
                .iter()
                .flatten()
                .map(|c| c.retain_on_rollback),
        );
        v.push(Box::new(crate::npm::NpmPublisher::with_overrides(
            req, retain,
        )));
    }
    if is_gemfury_configured(ctx) {
        // Escalate-to-true across `gemfury:` entries.
        let req = collapse_required(ctx.config.gemfury.iter().flatten().map(|c| c.required));
        let retain = collapse_required(
            ctx.config
                .gemfury
                .iter()
                .flatten()
                .map(|c| c.retain_on_rollback),
        );
        v.push(Box::new(crate::gemfury::GemFuryPublisher::with_overrides(
            req, retain,
        )));
    }
    // Submitter group (no programmatic rollback — warn-only).
    if is_chocolatey_configured(ctx) {
        // Escalate-to-true across crates.
        let req = collapse_required(ctx.config.crates.iter().map(|c| {
            c.publish
                .as_ref()
                .and_then(|p| p.chocolatey.as_ref()?.required)
        }));
        let retain = collapse_required(ctx.config.crates.iter().map(|c| {
            c.publish
                .as_ref()
                .and_then(|p| p.chocolatey.as_ref()?.retain_on_rollback)
        }));
        v.push(Box::new(
            crate::chocolatey::ChocolateyPublisher::with_overrides(req, retain),
        ));
    }
    if is_winget_configured(ctx) {
        // Escalate-to-true across crates.
        let req = collapse_required(
            ctx.config
                .crates
                .iter()
                .map(|c| c.publish.as_ref().and_then(|p| p.winget.as_ref()?.required)),
        );
        let retain = collapse_required(ctx.config.crates.iter().map(|c| {
            c.publish
                .as_ref()
                .and_then(|p| p.winget.as_ref()?.retain_on_rollback)
        }));
        v.push(Box::new(crate::winget::WingetPublisher::with_overrides(
            req, retain,
        )));
    }
    if crate::aur_source::is_aur_source_configured(ctx) {
        // Escalate-to-true across per-crate `publish.aur_source.required` AND
        // top-level `aur_sources:` entries: a `required: true` anywhere wins.
        let per_crate_req = ctx.config.crates.iter().map(|c| {
            c.publish
                .as_ref()
                .and_then(|p| p.aur_source.as_ref()?.required)
        });
        let top_req = ctx.config.aur_sources.iter().flatten().map(|c| c.required);
        let req = collapse_required(per_crate_req.chain(top_req));
        let per_crate_retain = ctx.config.crates.iter().map(|c| {
            c.publish
                .as_ref()
                .and_then(|p| p.aur_source.as_ref()?.retain_on_rollback)
        });
        let top_retain = ctx
            .config
            .aur_sources
            .iter()
            .flatten()
            .map(|c| c.retain_on_rollback);
        let retain = collapse_required(per_crate_retain.chain(top_retain));
        v.push(Box::new(
            crate::aur_source::AurSourcePublisher::with_overrides(req, retain),
        ));
    }
    // Snapcraft is intentionally NOT registered here — see the
    // doc comment on `configured_publishers` above.
    // `SnapcraftPublishStage` writes its own `PublisherResult`.
    v
}

/// Every publisher anodizer knows, with no configuration gating.
///
/// Built for environment-preflight requirement collection: each
/// [`Publisher::requirements`] self-gates on the resolved config (returning
/// empty when unconfigured) and walks the FULL crate universe — including
/// workspace crates that [`configured_publishers`]'s top-level-crate
/// predicates cannot see before the per-crate overlay flattens them. Never
/// use this list for dispatch; `run`/`rollback` on an unconfigured
/// publisher is not a supported path.
pub fn all_publishers() -> Vec<Box<dyn Publisher>> {
    vec![
        Box::new(crate::cargo::CargoPublisher::new()),
        Box::new(crate::dockerhub::DockerhubPublisher::new()),
        Box::new(crate::artifactory::ArtifactoryPublisher::new()),
        Box::new(crate::uploads::UploadsPublisher::new()),
        Box::new(crate::cloudsmith::CloudsmithPublisher::new()),
        Box::new(anodizer_stage_release::publisher::GithubReleasePublisher::new()),
        Box::new(crate::homebrew::publisher::HomebrewPublisher::new()),
        Box::new(crate::scoop::ScoopPublisher::new()),
        Box::new(crate::nix::publisher::NixPublisher::new()),
        Box::new(crate::mcp::publisher::McpPublisher::new()),
        Box::new(crate::aur::AurOurPublisher::new()),
        Box::new(crate::krew::KrewPublisher::new()),
        Box::new(crate::schemastore::SchemastorePublisher::new()),
        Box::new(crate::npm::NpmPublisher::new()),
        Box::new(crate::gemfury::GemFuryPublisher::new()),
        Box::new(crate::chocolatey::ChocolateyPublisher::new()),
        Box::new(crate::winget::WingetPublisher::new()),
        Box::new(crate::aur_source::AurSourcePublisher::new()),
    ]
}

/// True when at least one crate has a `publish.chocolatey` block.
fn is_chocolatey_configured(ctx: &Context) -> bool {
    ctx.config
        .crates
        .iter()
        .any(|c| c.publish.as_ref().is_some_and(|p| p.chocolatey.is_some()))
}

/// True when at least one crate has a `publish.winget` block.
fn is_winget_configured(ctx: &Context) -> bool {
    ctx.config
        .crates
        .iter()
        .any(|c| c.publish.as_ref().is_some_and(|p| p.winget.is_some()))
}

/// True when ANY crate has `publish.homebrew` OR the top-level
/// `homebrew_casks:` block is non-empty. Mirrors the dispatch in
/// `lib.rs` so the publisher runs whenever the existing per_crate +
/// top_level macros would have.
fn is_homebrew_configured(ctx: &Context) -> bool {
    let per_crate = ctx
        .config
        .crates
        .iter()
        .any(|c| c.publish.as_ref().is_some_and(|p| p.homebrew.is_some()));
    let top_level = ctx
        .config
        .homebrew_casks
        .as_ref()
        .is_some_and(|v| !v.is_empty());
    per_crate || top_level
}

/// True when at least one crate has a `publish.scoop` block.
fn is_scoop_configured(ctx: &Context) -> bool {
    ctx.config
        .crates
        .iter()
        .any(|c| c.publish.as_ref().is_some_and(|p| p.scoop.is_some()))
}

/// True when at least one crate has a `publish.nix` block.
fn is_nix_configured(ctx: &Context) -> bool {
    ctx.config
        .crates
        .iter()
        .any(|c| c.publish.as_ref().is_some_and(|p| p.nix.is_some()))
}

/// True when at least one crate has a `publish.aur` block. The
/// `publish.aur_source` upstream-AUR publisher is intentionally NOT
/// gated by this predicate — it has its own Submitter-group
/// publisher (see [`crate::aur_source::AurSourcePublisher`] +
/// [`crate::aur_source::is_aur_source_configured`]).
fn is_aur_configured(ctx: &Context) -> bool {
    ctx.config
        .crates
        .iter()
        .any(|c| c.publish.as_ref().is_some_and(|p| p.aur.is_some()))
}

/// True when at least one crate has a `publish.krew` block.
fn is_krew_configured(ctx: &Context) -> bool {
    ctx.config
        .crates
        .iter()
        .any(|c| c.publish.as_ref().is_some_and(|p| p.krew.is_some()))
}

/// True when the top-level `schemastore:` block carries at least one schema
/// entry. The per-entry `skip:` template is evaluated later in the publisher;
/// presence of any entry is the opt-in.
fn is_schemastore_configured(ctx: &Context) -> bool {
    !ctx.config.schemastore.schemas.is_empty()
}

/// True when the top-level `npms:` block has at least one entry.
fn is_npm_configured(ctx: &Context) -> bool {
    crate::publisher_helpers::is_top_level_block_configured(ctx.config.npms.as_ref())
}

/// True when the top-level `gemfury:` (or legacy `furies:`) block has at
/// least one entry. The alias collapse happens in serde — by the time we
/// reach this predicate the field is normalized to `gemfury:`.
fn is_gemfury_configured(ctx: &Context) -> bool {
    crate::publisher_helpers::is_top_level_block_configured(ctx.config.gemfury.as_ref())
}

/// True when the top-level `mcp.name` is set and non-empty. Mirrors
/// the skip-gate in [`crate::mcp::publish_to_mcp`] — an empty / unset
/// name short-circuits the publisher to a no-op, so we treat the same
/// state as not-configured here.
fn is_mcp_configured(ctx: &Context) -> bool {
    ctx.config
        .mcp
        .name
        .as_deref()
        .map(str::trim)
        .is_some_and(|s| !s.is_empty())
}

/// True when at least one crate in the active config has a
/// `publish.cargo` block. Presence of the block is the opt-in; the
/// per-crate `skip:` template is evaluated later in
/// [`crate::cargo::publish_to_cargo`].
///
/// Shape note: per-crate predicates use `.is_some()` because the inner
/// `CargoPublishConfig` is itself the opt-in — there is no list to count
/// non-empty. Top-level publishers (dockerhub, artifactories,
/// cloudsmiths) instead go through
/// [`crate::publisher_helpers::is_top_level_block_configured`], which
/// folds `Option<Vec<_>>` into a single uniform shape.
fn is_cargo_configured(ctx: &Context) -> bool {
    ctx.config
        .crates
        .iter()
        .any(|c| c.publish.as_ref().and_then(|p| p.cargo.as_ref()).is_some())
}

/// True when the top-level `dockerhub:` block has at least one entry.
/// `publish_to_dockerhub` short-circuits on an empty vec, so an empty-list
/// keep also returns false here.
fn is_dockerhub_configured(ctx: &Context) -> bool {
    crate::publisher_helpers::is_top_level_block_configured(ctx.config.dockerhub.as_ref())
}

/// True when the top-level `artifactories:` block has at least one entry.
fn is_artifactory_configured(ctx: &Context) -> bool {
    crate::publisher_helpers::is_top_level_block_configured(ctx.config.artifactories.as_ref())
}

/// True when the top-level `uploads:` block has at least one entry.
fn is_uploads_configured(ctx: &Context) -> bool {
    crate::publisher_helpers::is_top_level_block_configured(ctx.config.uploads.as_ref())
}

/// True when the top-level `cloudsmiths:` block has at least one entry.
fn is_cloudsmith_configured(ctx: &Context) -> bool {
    crate::publisher_helpers::is_top_level_block_configured(ctx.config.cloudsmiths.as_ref())
}

/// True when the resolved SCM is GitHub and at least one selected
/// crate has a `release:` block configured. Mirrors the per-crate
/// filter `ReleaseStage::run` applies internally (`c.release.is_some()`)
/// so the publisher iterates the same crate universe.
///
/// GitLab and Gitea backends have their own publishers (added in a
/// follow-up task); when `ctx.token_type` is one of those,
/// [`GithubReleasePublisher`](anodizer_stage_release::publisher::GithubReleasePublisher)
/// must NOT register so the registry doesn't double-publish a single
/// release run.
fn is_github_release_configured(ctx: &Context) -> bool {
    if !matches!(ctx.token_type, anodizer_core::scm::ScmTokenType::GitHub) {
        return false;
    }
    let selected = &ctx.options.selected_crates;
    ctx.config
        .crates
        .iter()
        .filter(|c| selected.is_empty() || selected.contains(&c.name))
        .any(|c| c.release.is_some())
}

/// Warn when the GitHub release is made non-required (`release.required:
/// false`) yet a publisher whose manifest points at the release's download URL
/// is enabled.
///
/// homebrew, scoop, and krew render install manifests that link to the GitHub
/// release assets. With `release.required: false`, a release-upload failure is
/// non-fatal — so those manifests can still ship pointing at a release URL that
/// 404s, silently breaking `brew install` / `scoop install` / `kubectl krew
/// install` for end users. The operator should see this coupling before the
/// run rather than discover it from a bug report.
///
/// Routed through the reporter (`log.warn`), never `eprintln!`.
pub fn warn_release_optional_with_dependent_publisher(ctx: &Context, log: &StageLogger) {
    if !is_github_release_configured(ctx) {
        return;
    }
    let release_required = collapse_required(
        ctx.config
            .crates
            .iter()
            .map(|c| c.release.as_ref().and_then(|r| r.required)),
    );
    // Only warn on an EXPLICIT opt-out. `None` keeps the publisher default and
    // is not a deliberate weakening of the gate.
    if release_required != Some(false) {
        return;
    }

    let mut dependents: Vec<&str> = Vec::new();
    if is_homebrew_configured(ctx) {
        dependents.push("homebrew");
    }
    if is_scoop_configured(ctx) {
        dependents.push("scoop");
    }
    if is_krew_configured(ctx) {
        dependents.push("krew");
    }
    if dependents.is_empty() {
        return;
    }

    log.warn(&format!(
        "release.required is false but release-URL-dependent publisher(s) [{}] are enabled: \
         if the GitHub release upload fails it will not fail the run, yet these manifests will \
         still ship pointing at a release URL that 404s. Set release.required: true (or verify \
         the release succeeds) before relying on those installers.",
        dependents.join(", ")
    ));
}

/// Group dispatch order: Assets first (uploadable bytes, server-side
/// deletable), then Manager (package-manager state, also reversible), then
/// Submitter (irreversible / moderation-locked: chocolatey, winget, krew).
///
/// The Submitter group runs last so its irreversible publishes can be
/// gated on the success of every reversible publisher that came before
/// it. See [`crate::dispatch::dispatch`] for the gate mechanics.
pub const fn group_dispatch_order() -> [PublisherGroup; 3] {
    [
        PublisherGroup::Assets,
        PublisherGroup::Manager,
        PublisherGroup::Submitter,
    ]
}

/// Publish surfaces that fire an external, irreversible publish from a
/// PIPELINE STAGE rather than the trait-based dispatch chokepoint, keyed by
/// the stage's [`anodizer_core::stage::Stage::name`] token.
///
/// These four stages each push to an external store/registry —
/// `blob` (object store), `snapcraft-publish` (Snap Store), `docker`
/// (image registry), `docker-sign` (cosign signatures to the registry) —
/// but, unlike npm/cargo/homebrew/…, they are NOT registered in
/// [`all_publishers`] (a parallel trait registration would double-publish;
/// see the doc comment on [`configured_publishers`]). They therefore never
/// pass through [`crate::dispatch::dispatch`], where the uniform
/// `--skip` / `--publishers` filter lives.
///
/// Listing their tokens here folds them into [`valid_publisher_names`] so
/// the SAME selector vocabulary governs them: `--publishers blob` is a valid
/// allowlist entry, and an allowlist that omits them correctly deselects
/// them (each stage consults [`anodizer_core::context::Context::publisher_deselected`]
/// before doing any irreversible work). This keeps `valid_publisher_names`
/// the single source of truth — there is no second list to drift.
///
/// `announce` is a governed leaf publisher: `AnnounceStage` broadcasts to
/// webhooks/Slack/Twitter/Mastodon/Bluesky — external, irreversible sends —
/// from a pipeline stage outside dispatch, so it consults `publisher_deselected`
/// before any broadcast. Like homebrew, it DEPENDS on the release substrate (it
/// reads `ReleaseURL`) yet is itself a leaf, so it is governed by the allowlist
/// exactly like blob/docker — NOT exempt the way `release` is.
///
/// `release` is deliberately ABSENT: the GitHub/GitLab/Gitea release the
/// `release` stage creates is the substrate every other publisher depends on
/// (homebrew/scoop/nix/krew manifests reference its assets; announce needs
/// `ReleaseURL`), so excluding it via an allowlist would silently break the
/// common `--publishers homebrew` case. It stays governed by `--skip=release`
/// (the denylist) only.
pub const PUBLISH_STAGE_PUBLISHERS: &[&str] = &[
    "blob",
    "snapcraft-publish",
    "docker",
    "docker-sign",
    "announce",
];

/// Every canonical publisher name: the trait-based publishers from
/// [`all_publishers`] PLUS the out-of-dispatch publish stages in
/// [`PUBLISH_STAGE_PUBLISHERS`].
///
/// This is the drift-proof source of valid `--publishers` / `--skip` publisher
/// tokens: it instantiates one of every publisher and reads each
/// [`anodizer_core::Publisher::name`], then appends the publish-stage tokens,
/// so a newly registered publisher is automatically a valid selector with no
/// hand-maintained literal list to update. The CLI validation (`init` /
/// `release` `--publishers` / `--skip`) and any help/error text consult this
/// rather than a duplicated constant.
///
/// Names are returned owned (`String`) because `Publisher::name` borrows the
/// boxed instance, which does not outlive this call.
pub fn valid_publisher_names() -> Vec<String> {
    all_publishers()
        .iter()
        .map(|p| p.name().to_string())
        .chain(PUBLISH_STAGE_PUBLISHERS.iter().map(|s| s.to_string()))
        .collect()
}

/// Validate operator publisher selection against the known publisher names.
///
/// Mirrors [`anodizer_core::context::validate_skip_values`]' message shape.
/// Two selectors are checked:
///
/// - `allowlist` (`--publishers`): every entry MUST be a known publisher name
///   (from [`valid_publisher_names`]).
/// - `skip` (`--skip`, the unified stage/publisher denylist): every entry MUST
///   be either a known publisher name OR a valid release-stage skip token
///   (from [`anodizer_core::context::VALID_RELEASE_SKIPS`]), so `--skip=npm`
///   and `--skip=build` both pass while `--skip=nmp` fails.
///
/// On any invalid value returns an `Err` string listing the offending value(s)
/// and the valid options for that selector. Returns `Ok(())` when both
/// selectors are clean (including when both are empty).
pub fn validate_publisher_selection(allowlist: &[String], skip: &[String]) -> Result<(), String> {
    let publishers = valid_publisher_names();

    let bad_allow: Vec<&str> = allowlist
        .iter()
        .map(|s| s.as_str())
        .filter(|s| !publishers.iter().any(|p| p == s))
        .collect();
    if !bad_allow.is_empty() {
        return Err(format!(
            "invalid --publishers value(s): {}. Valid publishers: {}",
            bad_allow.join(", "),
            publishers.join(", "),
        ));
    }

    let mut valid: Vec<&str> = anodizer_core::context::VALID_RELEASE_SKIPS.to_vec();
    valid.extend(publishers.iter().map(|s| s.as_str()));
    anodizer_core::context::validate_skip_values(skip, &valid)
}

/// Validate a `--publishers` allowlist against the *configured* publisher set
/// for `check config`.
///
/// Where [`validate_publisher_selection`] only proves a `--publishers` token is
/// a *known* publisher name (the vocabulary check the release/publish gate
/// needs before runtime selection), `check config` is a configuration-validation
/// command: its `--publishers` selectors must name publishers the active config
/// actually enables, or the operator's planned selection silently selects
/// nothing at release time. Two failure classes are distinguished:
///
/// - A token that is not even a known publisher (a typo) returns the same loud
///   `invalid --publishers value(s)` error as [`validate_publisher_selection`],
///   listing the valid names.
/// - A token that *is* a known publisher but is *not configured* (no matching
///   publish block) returns `publisher '<name>' named in --publishers is not
///   configured (no <name> publish block)` so the operator sees the genuine
///   config-validation signal.
///
/// Returns `Ok(())` when every token names a configured publisher (including
/// when the allowlist is empty).
pub fn validate_publisher_allowlist_configured(
    allowlist: &[String],
    ctx: &Context,
) -> Result<(), String> {
    let known = valid_publisher_names();

    let bad_allow: Vec<&str> = allowlist
        .iter()
        .map(|s| s.as_str())
        .filter(|s| !known.iter().any(|p| p == s))
        .collect();
    if !bad_allow.is_empty() {
        return Err(format!(
            "invalid --publishers value(s): {}. Valid publishers: {}",
            bad_allow.join(", "),
            known.join(", "),
        ));
    }

    let mut configured: Vec<String> = configured_publishers(ctx)
        .iter()
        .map(|p| p.name().to_string())
        .collect();
    // The out-of-dispatch publish stages (blob/snapcraft-publish/docker/
    // docker-sign) never appear in `configured_publishers` (they are not
    // trait publishers), so union in any that the active config enables —
    // otherwise `check config --publishers blob` on a config WITH a blob
    // block would falsely report blob "not configured".
    configured.extend(
        configured_publish_stage_publishers(ctx)
            .into_iter()
            .map(str::to_string),
    );
    for name in allowlist {
        if !configured.iter().any(|c| c == name) {
            return Err(format!(
                "publisher '{name}' named in --publishers is not configured \
                 (no {name} publish block)"
            ));
        }
    }
    Ok(())
}

/// The subset of [`PUBLISH_STAGE_PUBLISHERS`] the active config enables.
///
/// Each out-of-dispatch publish stage is "configured" when its config block
/// is present:
/// - `blob` — any crate has a `blobs:` block,
/// - `snapcraft-publish` — any crate has a `snapcrafts:` block,
/// - `docker` — any crate has a `dockers_v2:` or `docker_manifests:` block
///   (the same predicate [`anodizer_stage_docker`]'s `DockerStage::run` uses
///   to decide whether it has work),
/// - `docker-sign` — the top-level `docker_signs:` block is non-empty.
///
/// Consumed by [`validate_publisher_allowlist_configured`] so `check config
/// --publishers <stage>` validates against the real config, mirroring how the
/// trait publishers go through [`configured_publishers`].
fn configured_publish_stage_publishers(ctx: &Context) -> Vec<&'static str> {
    let mut out = Vec::new();
    if ctx.config.crates.iter().any(|c| c.blobs.is_some()) {
        out.push("blob");
    }
    if ctx.config.crates.iter().any(|c| c.snapcrafts.is_some()) {
        out.push("snapcraft-publish");
    }
    if ctx
        .config
        .crates
        .iter()
        .any(|c| c.dockers_v2.is_some() || c.docker_manifests.is_some())
    {
        out.push("docker");
    }
    if ctx
        .config
        .docker_signs
        .as_ref()
        .is_some_and(|v| !v.is_empty())
    {
        out.push("docker-sign");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::config::{CargoPublishConfig, CrateConfig, PublishConfig};
    use anodizer_core::test_helpers::TestContextBuilder;

    #[test]
    fn configured_publishers_empty_without_publish_blocks() {
        let ctx = Context::test_fixture();
        let publishers = configured_publishers(&ctx);
        assert!(
            publishers.is_empty(),
            "registry should stay empty when no crate opts into a publisher"
        );
    }

    #[test]
    fn valid_publisher_names_non_empty_with_known_members() {
        let names = valid_publisher_names();
        assert!(!names.is_empty(), "publisher name set must not be empty");
        for known in ["npm", "cargo", "uploads", "winget"] {
            assert!(
                names.iter().any(|n| n == known),
                "expected {known} in {names:?}"
            );
        }
    }

    #[test]
    fn validate_publisher_selection_accepts_valid_allowlist() {
        let allow = vec!["cargo".to_string(), "npm".to_string()];
        assert!(validate_publisher_selection(&allow, &[]).is_ok());
    }

    #[test]
    fn validate_publisher_selection_rejects_allowlist_typo() {
        let allow = vec!["crago".to_string()];
        let err = validate_publisher_selection(&allow, &[]).unwrap_err();
        assert!(err.contains("crago"), "{err}");
        assert!(err.contains("cargo"), "hint must list valid names: {err}");
    }

    #[test]
    fn validate_publisher_selection_skip_accepts_publisher_name() {
        let skip = vec!["npm".to_string()];
        assert!(validate_publisher_selection(&[], &skip).is_ok());
    }

    #[test]
    fn validate_publisher_selection_skip_accepts_stage_name() {
        let skip = vec!["build".to_string()];
        assert!(validate_publisher_selection(&[], &skip).is_ok());
    }

    #[test]
    fn validate_publisher_selection_skip_rejects_bogus() {
        let skip = vec!["bogus".to_string()];
        let err = validate_publisher_selection(&[], &skip).unwrap_err();
        assert!(err.contains("bogus"), "{err}");
    }

    #[test]
    fn valid_publisher_names_includes_out_of_dispatch_publish_stages() {
        // blob / snapcraft-publish / docker / docker-sign perform external,
        // irreversible publishes from a pipeline stage outside dispatch; they
        // must be part of the selector vocabulary so `--publishers <stage>`
        // is accepted and an allowlist omitting them deselects them.
        let names = valid_publisher_names();
        for stage in PUBLISH_STAGE_PUBLISHERS {
            assert!(
                names.iter().any(|n| n == stage),
                "expected {stage} in {names:?}"
            );
        }
    }

    #[test]
    fn validate_publisher_selection_accepts_publish_stage_allowlist() {
        // `--publishers blob` / `snapcraft-publish` / `docker` / `docker-sign`
        // must NOT be rejected as invalid.
        for stage in PUBLISH_STAGE_PUBLISHERS {
            let allow = vec![stage.to_string()];
            assert!(
                validate_publisher_selection(&allow, &[]).is_ok(),
                "--publishers {stage} must validate"
            );
        }
    }

    #[test]
    fn validate_publisher_selection_skip_accepts_publish_stage_name() {
        // `--skip=blob` / `--skip=snapcraft-publish` must still pass (denylist
        // must not regress now that they are publisher tokens too).
        for stage in ["blob", "snapcraft-publish", "docker", "docker-sign"] {
            let skip = vec![stage.to_string()];
            assert!(
                validate_publisher_selection(&[], &skip).is_ok(),
                "--skip={stage} must validate"
            );
        }
    }

    fn cargo_configured_ctx() -> Context {
        let crate_cfg = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                cargo: Some(CargoPublishConfig::default()),
                ..Default::default()
            }),
            ..Default::default()
        };
        TestContextBuilder::new().crates(vec![crate_cfg]).build()
    }

    #[test]
    fn allowlist_configured_accepts_configured_publisher() {
        let ctx = cargo_configured_ctx();
        let allow = vec!["cargo".to_string()];
        assert!(validate_publisher_allowlist_configured(&allow, &ctx).is_ok());
    }

    #[test]
    fn allowlist_configured_rejects_known_but_unconfigured_publisher() {
        let ctx = cargo_configured_ctx();
        let allow = vec!["npm".to_string()];
        let err = validate_publisher_allowlist_configured(&allow, &ctx).unwrap_err();
        assert!(err.contains("not configured"), "{err}");
        assert!(err.contains("npm"), "{err}");
        assert!(
            !err.contains("invalid --publishers value"),
            "unconfigured must NOT use the typo phrase: {err}"
        );
    }

    #[test]
    fn allowlist_configured_rejects_typo_with_loud_error() {
        let ctx = cargo_configured_ctx();
        let allow = vec!["crago".to_string()];
        let err = validate_publisher_allowlist_configured(&allow, &ctx).unwrap_err();
        assert!(err.contains("invalid --publishers value"), "{err}");
        assert!(err.contains("crago"), "{err}");
        assert!(err.contains("cargo"), "hint must list valid names: {err}");
    }

    #[test]
    fn allowlist_configured_accepts_empty() {
        let ctx = cargo_configured_ctx();
        assert!(validate_publisher_allowlist_configured(&[], &ctx).is_ok());
    }

    #[test]
    fn cargo_publisher_registered_when_configured() {
        let crate_cfg = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                cargo: Some(CargoPublishConfig::default()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new().crates(vec![crate_cfg]).build();
        let publishers = configured_publishers(&ctx);
        assert_eq!(publishers.len(), 1, "exactly one publisher expected");
        assert_eq!(publishers[0].name(), "cargo");
        assert_eq!(publishers[0].group(), PublisherGroup::Submitter);
        assert!(publishers[0].required());
    }

    #[test]
    fn group_dispatch_order_is_assets_manager_submitter() {
        assert_eq!(
            group_dispatch_order(),
            [
                PublisherGroup::Assets,
                PublisherGroup::Manager,
                PublisherGroup::Submitter,
            ]
        );
    }

    #[test]
    fn bundle_a_publishers_registered_when_configured() {
        use anodizer_core::config::{
            ArtifactoryConfig, BlobConfig, CloudSmithConfig, DockerHubConfig,
        };
        let crate_cfg = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            blobs: Some(vec![BlobConfig {
                provider: "s3".to_string(),
                bucket: "my-bucket".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let mut ctx = TestContextBuilder::new().crates(vec![crate_cfg]).build();
        // Top-level publisher blocks live on Config directly.
        ctx.config.dockerhub = Some(vec![DockerHubConfig {
            username: Some("u".to_string()),
            images: Some(vec!["acme/widget".to_string()]),
            ..Default::default()
        }]);
        ctx.config.artifactories = Some(vec![ArtifactoryConfig {
            name: Some("prod".to_string()),
            target: Some("https://art.example.com/repo/".to_string()),
            ..Default::default()
        }]);
        ctx.config.cloudsmiths = Some(vec![CloudSmithConfig {
            organization: Some("acme".to_string()),
            repository: Some("widget".to_string()),
            ..Default::default()
        }]);

        let publishers = configured_publishers(&ctx);
        let names: Vec<&str> = publishers.iter().map(|p| p.name()).collect();
        // Every Assets-group publisher that registers in this list
        // must appear; blob is Assets-group but runs as its own
        // `BlobStage`, not via the publisher dispatch path, so it is
        // NOT registered here (asserted separately below).
        for expected in ["dockerhub", "artifactory", "cloudsmith"] {
            assert!(
                names.contains(&expected),
                "{} missing from registered publishers (got {:?})",
                expected,
                names
            );
            let p = publishers
                .iter()
                .find(|p| p.name() == expected)
                .expect("publisher present");
            assert_eq!(p.group(), PublisherGroup::Assets, "{}", expected);
            assert!(!p.required(), "{} should not be required", expected);
        }
        // Pin: BlobPublisher must NOT register from the stage-publish
        // registry. `BlobStage` is the load-bearing runner and writes
        // its own entry into `ctx.publish_report`; registering the
        // publisher here would double-publish every blob target.
        assert!(
            !names.contains(&"blob"),
            "blob must NOT be in the publisher registry (BlobStage owns the upload); got {:?}",
            names
        );
    }

    #[test]
    fn git_revert_publishers_registered_when_configured() {
        use anodizer_core::config::{
            AurConfig, HomebrewConfig, NixConfig, RepositoryConfig, ScoopConfig,
        };
        // Build a single crate with all four git-revert per-crate
        // publishers configured so one fixture exercises every
        // gate in `configured_publishers`.
        let demo = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                homebrew: Some(HomebrewConfig {
                    repository: Some(RepositoryConfig {
                        owner: Some("acme".to_string()),
                        name: Some("homebrew-tap".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                scoop: Some(ScoopConfig {
                    repository: Some(RepositoryConfig {
                        owner: Some("acme".to_string()),
                        name: Some("scoop-bucket".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                nix: Some(NixConfig {
                    repository: Some(RepositoryConfig {
                        owner: Some("acme".to_string()),
                        name: Some("nixpkgs-overlay".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                aur: Some(AurConfig {
                    git_url: Some("ssh://aur@aur.archlinux.org/demo-bin.git".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new().crates(vec![demo]).build();
        let publishers = configured_publishers(&ctx);
        let names: Vec<&str> = publishers.iter().map(|p| p.name()).collect();
        for expected in ["homebrew", "scoop", "nix", "aur"] {
            assert!(
                names.contains(&expected),
                "{} missing from registered publishers (got {:?})",
                expected,
                names
            );
            let p = publishers
                .iter()
                .find(|p| p.name() == expected)
                .expect("publisher present");
            assert_eq!(
                p.group(),
                PublisherGroup::Manager,
                "{} should be Manager group",
                expected
            );
            assert!(!p.required(), "{} should not be required", expected);
        }
    }

    #[test]
    fn bundle_c_publishers_registered_when_configured() {
        use anodizer_core::config::{KrewConfig, McpConfig, RepositoryConfig};
        // krew is per-crate (publish.krew); mcp is top-level (Config.mcp).
        // One fixture exercises both registration gates.
        let demo = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                krew: Some(KrewConfig {
                    repository: Some(RepositoryConfig {
                        owner: Some("acme".to_string()),
                        name: Some("krew-index-fork".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut ctx = TestContextBuilder::new().crates(vec![demo]).build();
        ctx.config.mcp = McpConfig {
            name: Some("io.github.acme/widget".to_string()),
            ..Default::default()
        };
        let publishers = configured_publishers(&ctx);
        let names: Vec<&str> = publishers.iter().map(|p| p.name()).collect();
        for expected in ["krew", "mcp"] {
            assert!(
                names.contains(&expected),
                "{} missing from registered publishers (got {:?})",
                expected,
                names
            );
            let p = publishers
                .iter()
                .find(|p| p.name() == expected)
                .expect("publisher present");
            assert_eq!(
                p.group(),
                PublisherGroup::Manager,
                "{} should be Manager group",
                expected
            );
            assert!(!p.required(), "{} should not be required", expected);
            // krew opens a PR (rollback closes it via pull_request:write).
            // mcp posts to a registry API (no PR; rollback re-publish path
            // reads MCP_GITHUB_TOKEN — see McpPublisher rustdoc).
            let expected_scope = match expected {
                "krew" => Some("GITHUB_TOKEN pull_request:write"),
                "mcp" => Some("MCP_GITHUB_TOKEN status-mutation"),
                other => panic!("unexpected publisher in fixture: {}", other),
            };
            assert_eq!(
                p.rollback_scope_needed(),
                expected_scope,
                "{} rollback scope",
                expected
            );
        }
    }

    #[test]
    fn github_release_publisher_registered_when_configured() {
        use anodizer_core::config::{ReleaseConfig, ScmRepoConfig};
        // Per-crate `release.github` opts in. The default token_type
        // for `Context::test_fixture` / TestContextBuilder is GitHub,
        // matching the production default in `Context::new`.
        let crate_cfg = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ Version }}".to_string(),
            release: Some(ReleaseConfig {
                github: Some(ScmRepoConfig {
                    owner: "acme".to_string(),
                    name: "widget".to_string(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new().crates(vec![crate_cfg]).build();
        let publishers = configured_publishers(&ctx);
        let names: Vec<&str> = publishers.iter().map(|p| p.name()).collect();
        assert!(
            names.contains(&"github-release"),
            "github-release missing from registered publishers (got {names:?})"
        );
        let p = publishers
            .iter()
            .find(|p| p.name() == "github-release")
            .expect("github-release present");
        assert_eq!(p.group(), PublisherGroup::Assets);
        assert!(p.required(), "github-release is required");
        assert_eq!(
            p.rollback_scope_needed(),
            Some("GITHUB_TOKEN contents:write")
        );
    }

    #[test]
    fn github_release_publisher_not_registered_when_scm_is_gitlab() {
        use anodizer_core::config::{ReleaseConfig, ScmRepoConfig};
        use anodizer_core::scm::ScmTokenType;
        let crate_cfg = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ Version }}".to_string(),
            release: Some(ReleaseConfig {
                gitlab: Some(ScmRepoConfig {
                    owner: "acme".to_string(),
                    name: "widget".to_string(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut ctx = TestContextBuilder::new().crates(vec![crate_cfg]).build();
        ctx.token_type = ScmTokenType::GitLab;
        let publishers = configured_publishers(&ctx);
        let names: Vec<&str> = publishers.iter().map(|p| p.name()).collect();
        assert!(
            !names.contains(&"github-release"),
            "github-release should NOT register when SCM is GitLab (got {names:?})"
        );
    }

    #[test]
    fn mcp_publisher_skipped_when_name_empty() {
        // mcp's skip-gate triggers on empty `name`. The registry
        // predicate mirrors that gate so we don't instantiate a
        // publisher whose run() would no-op anyway.
        let mut ctx = Context::test_fixture();
        ctx.config.mcp = anodizer_core::config::McpConfig {
            name: Some("   ".to_string()),
            ..Default::default()
        };
        let publishers = configured_publishers(&ctx);
        let names: Vec<&str> = publishers.iter().map(|p| p.name()).collect();
        assert!(
            !names.contains(&"mcp"),
            "mcp should not register when name trims to empty (got {:?})",
            names
        );
    }

    #[test]
    fn submitter_solo_publishers_registered_when_configured() {
        use anodizer_core::config::{
            AurSourceConfig, ChocolateyConfig, RepositoryConfig, WingetConfig,
        };
        // One fixture exercises all three Submitter-group "solo"
        // (no-rollback) publishers: chocolatey, winget, upstream-aur.
        // cargo is also Submitter group but lives outside this trio
        // (it has its own scope + required=true classification).
        let demo = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                chocolatey: Some(ChocolateyConfig {
                    name: Some("demo".to_string()),
                    ..Default::default()
                }),
                winget: Some(WingetConfig {
                    publisher: Some("AcmeCo".to_string()),
                    repository: Some(RepositoryConfig {
                        owner: Some("acme".to_string()),
                        name: Some("winget-pkgs-fork".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                aur_source: Some(AurSourceConfig {
                    git_url: Some("ssh://aur@aur.archlinux.org/demo.git".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new().crates(vec![demo]).build();
        let publishers = configured_publishers(&ctx);
        let names: Vec<&str> = publishers.iter().map(|p| p.name()).collect();
        let expected_scopes: &[(&str, Option<&str>)] = &[
            ("chocolatey", None),
            ("winget", Some("GITHUB_TOKEN pull_request:write")),
            ("upstream-aur", Some("AUR_SSH_KEY write")),
        ];
        for (publisher_name, expected_scope) in expected_scopes {
            assert!(
                names.contains(publisher_name),
                "{} missing from registered publishers (got {:?})",
                publisher_name,
                names
            );
            let p = publishers
                .iter()
                .find(|p| &p.name() == publisher_name)
                .expect("publisher present");
            assert_eq!(
                p.group(),
                PublisherGroup::Submitter,
                "{} should be Submitter group",
                publisher_name
            );
            assert!(!p.required(), "{} should not be required", publisher_name);
            assert_eq!(
                p.rollback_scope_needed(),
                *expected_scope,
                "{} rollback scope",
                publisher_name
            );
        }
    }

    #[test]
    fn snapcraft_unconditionally_unregistered_regardless_of_publish_flag() {
        // Pin: SnapcraftPublisher must NOT register from the
        // stage-publish registry under any `publish:` flag value.
        // `SnapcraftPublishStage` is the load-bearing runner and writes
        // its own entry into `ctx.publish_report`; a trait-based
        // wrapper here would double-publish every snap target (parallel
        // to the BlobPublisher fix in commit 026c854). The
        // table form pins ALL three input shapes (unset, false, true)
        // so a future regression that re-introduces a `publish:
        // true`-gated registration is caught.
        use anodizer_core::config::SnapcraftConfig;
        for publish_flag in [None, Some(false), Some(true)] {
            let demo = CrateConfig {
                name: "demo".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                snapcrafts: Some(vec![SnapcraftConfig {
                    name: Some("demo".to_string()),
                    publish: publish_flag,
                    channel_templates: Some(vec!["stable".to_string()]),
                    ..Default::default()
                }]),
                ..Default::default()
            };
            let ctx = TestContextBuilder::new().crates(vec![demo]).build();
            let publishers = configured_publishers(&ctx);
            let names: Vec<&str> = publishers.iter().map(|p| p.name()).collect();
            assert!(
                !names.contains(&"snapcraft"),
                "snapcraft must NOT register for publish={publish_flag:?}; got {names:?}"
            );
        }
    }

    // -------------------------------------------------------------------------
    // required-override tests
    // -------------------------------------------------------------------------

    #[test]
    fn config_required_override_honored_homebrew() {
        use anodizer_core::config::{HomebrewConfig, RepositoryConfig};
        let demo = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                homebrew: Some(HomebrewConfig {
                    repository: Some(RepositoryConfig {
                        owner: Some("acme".to_string()),
                        name: Some("homebrew-tap".to_string()),
                        ..Default::default()
                    }),
                    required: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new().crates(vec![demo]).build();
        let publishers = configured_publishers(&ctx);
        let p = publishers
            .iter()
            .find(|p| p.name() == "homebrew")
            .expect("homebrew registered");
        assert!(
            p.required(),
            "homebrew.required = Some(true) must override the default false"
        );
    }

    #[test]
    fn config_required_none_uses_default_homebrew() {
        use anodizer_core::config::{HomebrewConfig, RepositoryConfig};
        let demo = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                homebrew: Some(HomebrewConfig {
                    repository: Some(RepositoryConfig {
                        owner: Some("acme".to_string()),
                        name: Some("homebrew-tap".to_string()),
                        ..Default::default()
                    }),
                    required: None,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new().crates(vec![demo]).build();
        let publishers = configured_publishers(&ctx);
        let p = publishers
            .iter()
            .find(|p| p.name() == "homebrew")
            .expect("homebrew registered");
        assert!(
            !p.required(),
            "homebrew.required = None must fall through to the default (false)"
        );
    }

    #[test]
    fn config_required_override_honored_chocolatey() {
        use anodizer_core::config::ChocolateyConfig;
        let demo = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                chocolatey: Some(ChocolateyConfig {
                    name: Some("demo".to_string()),
                    required: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new().crates(vec![demo]).build();
        let publishers = configured_publishers(&ctx);
        let p = publishers
            .iter()
            .find(|p| p.name() == "chocolatey")
            .expect("chocolatey registered");
        assert!(
            p.required(),
            "chocolatey.required = Some(true) must override the default false"
        );
    }

    #[test]
    fn config_required_false_overrides_default_cargo() {
        let demo = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                cargo: Some(CargoPublishConfig {
                    required: Some(false),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new().crates(vec![demo]).build();
        let publishers = configured_publishers(&ctx);
        let p = publishers
            .iter()
            .find(|p| p.name() == "cargo")
            .expect("cargo registered");
        assert!(
            !p.required(),
            "cargo.required = Some(false) must override the default true"
        );
    }

    #[test]
    fn config_required_none_preserves_cargo_default_true() {
        let demo = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                cargo: Some(CargoPublishConfig::default()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new().crates(vec![demo]).build();
        let publishers = configured_publishers(&ctx);
        let p = publishers
            .iter()
            .find(|p| p.name() == "cargo")
            .expect("cargo registered");
        assert!(
            p.required(),
            "cargo with no required override must keep the built-in default (true)"
        );
    }

    #[test]
    fn config_required_override_honored_homebrew_cask_only() {
        use anodizer_core::config::{HomebrewCaskConfig, RepositoryConfig};
        // Cask-only setup: no per-crate `publish.homebrew`, only top-level
        // `homebrew_casks:`. The cask config's `required` must reach
        // HomebrewPublisher via the fallback lookup branch.
        let demo = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        };
        let mut ctx = TestContextBuilder::new().crates(vec![demo]).build();
        ctx.config.homebrew_casks = Some(vec![HomebrewCaskConfig {
            name: Some("demo".to_string()),
            repository: Some(RepositoryConfig {
                owner: Some("acme".to_string()),
                name: Some("homebrew-tap".to_string()),
                ..Default::default()
            }),
            required: Some(true),
            ..Default::default()
        }]);
        let publishers = configured_publishers(&ctx);
        let p = publishers
            .iter()
            .find(|p| p.name() == "homebrew")
            .expect("homebrew registered via homebrew_casks");
        assert!(
            p.required(),
            "homebrew_casks[].required = Some(true) must override the default false for cask-only setups"
        );
    }

    #[test]
    fn config_required_escalates_to_true_across_crates() {
        use anodizer_core::config::{HomebrewConfig, RepositoryConfig};
        // `required` is a safety gate: a later crate's `required: true` must NOT
        // be masked by an earlier crate's `required: false`. The first crate
        // (alpha) opts OUT, the second (beta) opts IN — the collapse must
        // escalate to `true`. A first-non-None `find_map` would (wrongly)
        // return alpha's `false` and drop the gate.
        let homebrew = |required: bool| {
            Some(HomebrewConfig {
                repository: Some(RepositoryConfig {
                    owner: Some("acme".to_string()),
                    name: Some("homebrew-tap".to_string()),
                    ..Default::default()
                }),
                required: Some(required),
                ..Default::default()
            })
        };
        let alpha = CrateConfig {
            name: "alpha".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                homebrew: homebrew(false),
                ..Default::default()
            }),
            ..Default::default()
        };
        let beta = CrateConfig {
            name: "beta".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                homebrew: homebrew(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new().crates(vec![alpha, beta]).build();
        let publishers = configured_publishers(&ctx);
        let p = publishers
            .iter()
            .find(|p| p.name() == "homebrew")
            .expect("homebrew registered");
        assert!(
            p.required(),
            "any crate's required:true must escalate the gate to true, even when an earlier crate said false"
        );
    }

    #[test]
    fn collapse_required_escalates_true_over_false_and_handles_none() {
        // true anywhere wins regardless of order.
        assert_eq!(
            collapse_required([Some(false), Some(true), Some(false)].into_iter()),
            Some(true)
        );
        assert_eq!(
            collapse_required([Some(true), Some(false)].into_iter()),
            Some(true)
        );
        // All-false (with Nones interleaved) collapses to false.
        assert_eq!(
            collapse_required([None, Some(false), None].into_iter()),
            Some(false)
        );
        // No override anywhere → None (publisher keeps its built-in default).
        assert_eq!(collapse_required([None, None].into_iter()), None);
        assert_eq!(collapse_required(std::iter::empty()), None);
    }

    #[test]
    fn config_required_override_honored_dockerhub() {
        use anodizer_core::config::DockerHubConfig;
        let mut ctx = Context::test_fixture();
        ctx.config.dockerhub = Some(vec![DockerHubConfig {
            username: Some("u".to_string()),
            images: Some(vec!["acme/widget".to_string()]),
            required: Some(true),
            ..Default::default()
        }]);
        let publishers = configured_publishers(&ctx);
        let p = publishers
            .iter()
            .find(|p| p.name() == "dockerhub")
            .expect("dockerhub registered");
        assert!(
            p.required(),
            "dockerhub[].required = Some(true) must override the default false"
        );
    }

    #[test]
    fn config_required_override_honored_artifactory() {
        use anodizer_core::config::ArtifactoryConfig;
        let mut ctx = Context::test_fixture();
        ctx.config.artifactories = Some(vec![ArtifactoryConfig {
            name: Some("prod".to_string()),
            target: Some("https://art.example.com/repo/".to_string()),
            required: Some(true),
            ..Default::default()
        }]);
        let publishers = configured_publishers(&ctx);
        let p = publishers
            .iter()
            .find(|p| p.name() == "artifactory")
            .expect("artifactory registered");
        assert!(
            p.required(),
            "artifactories[].required = Some(true) must override the default false"
        );
    }

    #[test]
    fn config_required_override_honored_cloudsmith() {
        use anodizer_core::config::CloudSmithConfig;
        let mut ctx = Context::test_fixture();
        ctx.config.cloudsmiths = Some(vec![CloudSmithConfig {
            organization: Some("acme".to_string()),
            repository: Some("widget".to_string()),
            required: Some(true),
            ..Default::default()
        }]);
        let publishers = configured_publishers(&ctx);
        let p = publishers
            .iter()
            .find(|p| p.name() == "cloudsmith")
            .expect("cloudsmith registered");
        assert!(
            p.required(),
            "cloudsmiths[].required = Some(true) must override the default false"
        );
    }

    #[test]
    fn config_required_false_overrides_default_release() {
        use anodizer_core::config::{ReleaseConfig, ScmRepoConfig};
        let crate_cfg = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ Version }}".to_string(),
            release: Some(ReleaseConfig {
                github: Some(ScmRepoConfig {
                    owner: "acme".to_string(),
                    name: "widget".to_string(),
                }),
                required: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new().crates(vec![crate_cfg]).build();
        let publishers = configured_publishers(&ctx);
        let p = publishers
            .iter()
            .find(|p| p.name() == "github-release")
            .expect("github-release registered");
        assert!(
            !p.required(),
            "release.required = Some(false) must override the default true"
        );
    }

    #[test]
    fn release_optional_warns_when_dependent_publisher_enabled() {
        use anodizer_core::config::{
            HomebrewConfig, ReleaseConfig, RepositoryConfig, ScmRepoConfig,
        };
        use anodizer_core::log::{StageLogger, Verbosity};
        let crate_cfg = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ Version }}".to_string(),
            release: Some(ReleaseConfig {
                github: Some(ScmRepoConfig {
                    owner: "acme".to_string(),
                    name: "widget".to_string(),
                }),
                required: Some(false),
                ..Default::default()
            }),
            publish: Some(PublishConfig {
                homebrew: Some(HomebrewConfig {
                    repository: Some(RepositoryConfig {
                        owner: Some("acme".to_string()),
                        name: Some("homebrew-tap".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new().crates(vec![crate_cfg]).build();
        let (log, cap) = StageLogger::with_capture("publish", Verbosity::Normal);
        warn_release_optional_with_dependent_publisher(&ctx, &log);
        let warns = cap.warn_messages();
        assert!(
            warns
                .iter()
                .any(|m| m.contains("release.required is false") && m.contains("homebrew")),
            "expected a release-optional warning naming homebrew, got: {warns:?}"
        );
    }

    #[test]
    fn release_optional_no_warn_without_dependent_publisher() {
        use anodizer_core::config::{ReleaseConfig, ScmRepoConfig};
        use anodizer_core::log::{StageLogger, Verbosity};
        let crate_cfg = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ Version }}".to_string(),
            release: Some(ReleaseConfig {
                github: Some(ScmRepoConfig {
                    owner: "acme".to_string(),
                    name: "widget".to_string(),
                }),
                required: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new().crates(vec![crate_cfg]).build();
        let (log, cap) = StageLogger::with_capture("publish", Verbosity::Normal);
        warn_release_optional_with_dependent_publisher(&ctx, &log);
        assert_eq!(
            cap.warn_count(),
            0,
            "no dependent publisher → no warning, got: {:?}",
            cap.warn_messages()
        );
    }

    #[test]
    fn release_required_default_none_no_warn_even_with_dependent_publisher() {
        use anodizer_core::config::{
            HomebrewConfig, ReleaseConfig, RepositoryConfig, ScmRepoConfig,
        };
        use anodizer_core::log::{StageLogger, Verbosity};
        // No explicit `required` (None) is not a deliberate opt-out → no warn.
        let crate_cfg = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ Version }}".to_string(),
            release: Some(ReleaseConfig {
                github: Some(ScmRepoConfig {
                    owner: "acme".to_string(),
                    name: "widget".to_string(),
                }),
                ..Default::default()
            }),
            publish: Some(PublishConfig {
                homebrew: Some(HomebrewConfig {
                    repository: Some(RepositoryConfig {
                        owner: Some("acme".to_string()),
                        name: Some("homebrew-tap".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new().crates(vec![crate_cfg]).build();
        let (log, cap) = StageLogger::with_capture("publish", Verbosity::Normal);
        warn_release_optional_with_dependent_publisher(&ctx, &log);
        assert_eq!(cap.warn_count(), 0, "None required is not an opt-out");
    }

    #[test]
    fn config_required_override_honored_mcp() {
        use anodizer_core::config::McpConfig;
        let mut ctx = Context::test_fixture();
        ctx.config.mcp = McpConfig {
            name: Some("io.github.acme/widget".to_string()),
            required: Some(true),
            ..Default::default()
        };
        let publishers = configured_publishers(&ctx);
        let p = publishers
            .iter()
            .find(|p| p.name() == "mcp")
            .expect("mcp registered");
        assert!(
            p.required(),
            "mcp.required = Some(true) must override the default false"
        );
    }

    #[test]
    fn schemastore_registers_in_manager_group_when_schemas_present() {
        use anodizer_core::config::{SchemaEntry, SchemastoreConfig};
        let mut ctx = TestContextBuilder::new().build();
        ctx.config.schemastore = SchemastoreConfig {
            schemas: vec![SchemaEntry {
                name: "Anodizer".into(),
                file_match: vec![".anodizer.yaml".into()],
                url: Some("https://x/s.json".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let publishers = configured_publishers(&ctx);
        // Exactly one schemastore publisher per config block (it iterates its
        // own `schemas` internally), in the Manager group.
        let schemastore: Vec<&Box<dyn Publisher>> = publishers
            .iter()
            .filter(|p| p.name() == "schemastore")
            .collect();
        assert_eq!(
            schemastore.len(),
            1,
            "exactly one schemastore publisher per config block, got {}",
            schemastore.len()
        );
        assert_eq!(schemastore[0].group(), PublisherGroup::Manager);
    }

    #[test]
    fn schemastore_absent_without_schemas() {
        let ctx = Context::test_fixture();
        let publishers = configured_publishers(&ctx);
        let names: Vec<&str> = publishers.iter().map(|p| p.name()).collect();
        assert!(
            !names.contains(&"schemastore"),
            "schemastore must not register with an empty `schemas` block (got {names:?})"
        );
    }
}
