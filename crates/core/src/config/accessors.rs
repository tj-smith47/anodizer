use super::*;

impl Config {
    /// The full crate universe: top-level `crates` plus every
    /// `workspaces[].crates` entry, deduplicated by name (first-seen wins,
    /// so a top-level entry shadows a same-named workspace entry).
    ///
    /// Single source of the read-only "all crates that can carry per-crate
    /// config" walk. Publisher registration, required/retain gate
    /// collapsing, per-crate dispatch, requirement derivation,
    /// `--crate`/`--all` selection, tool-need detection, artifact guards,
    /// and default-naming decisions must all resolve through this walker so
    /// a workspace-only crate carrying a publisher block is either visible
    /// everywhere or nowhere — a consumer iterating `config.crates`
    /// directly silently excludes workspace crates and hides their
    /// publishes. Only two shapes may keep a raw chained walk: mutation
    /// passes (`&mut` access — this walker hands out shared borrows) and
    /// validation/diagnostics that must see every entry as written,
    /// including the shadowed duplicates this walker dedups away.
    pub fn crate_universe(&self) -> Vec<&CrateConfig> {
        self.crate_universe_walk().0
    }

    /// Borrow a crate by name from [`Self::crate_universe`] (top-level wins
    /// on a name collision). The single by-name lookup every consumer must
    /// use — a `config.crates.iter().find(...)` cannot see workspace-only
    /// crates.
    pub fn find_crate(&self, name: &str) -> Option<&CrateConfig> {
        self.crate_universe().into_iter().find(|c| c.name == name)
    }

    /// Operator-facing warnings for crate-name collisions in the universe
    /// where the colliding entries disagree on `path` — almost certainly a
    /// config mistake (two distinct crates sharing a name). The legitimate
    /// duplicate (the same crate referenced from both top-level and a
    /// workspace) dedups silently. Emitted by the publish stage at entry so
    /// the warning appears once per run rather than once per universe walk.
    pub fn crate_universe_collision_warnings(&self) -> Vec<String> {
        self.crate_universe_walk().1
    }

    /// The one walk both [`Self::crate_universe`] and
    /// [`Self::crate_universe_collision_warnings`] derive from, so the
    /// merge/dedup policy and its diagnostics cannot diverge.
    fn crate_universe_walk(&self) -> (Vec<&CrateConfig>, Vec<String>) {
        let mut out: Vec<&CrateConfig> = self.crates.iter().collect();
        let mut warnings = Vec::new();
        for ws in self.workspaces.iter().flatten() {
            for c in &ws.crates {
                if let Some(existing) = out.iter().find(|e| e.name == c.name) {
                    if existing.path != c.path {
                        warnings.push(format!(
                            "workspace '{}' crate '{}' path '{}' shadowed by \
                             prior entry with path '{}'; workspace entry dropped (name \
                             collision with different paths — likely a config mistake)",
                            ws.name, c.name, c.path, existing.path
                        ));
                    }
                    continue;
                }
                out.push(c);
            }
        }
        (out, warnings)
    }

    /// Return the monorepo tag prefix, if configured.
    ///
    /// Shorthand for `config.monorepo.as_ref().and_then(|m| m.tag_prefix.as_deref())`.
    pub fn monorepo_tag_prefix(&self) -> Option<&str> {
        self.monorepo.as_ref().and_then(|m| m.tag_prefix.as_deref())
    }

    /// Return the monorepo working directory, if configured.
    ///
    /// Shorthand for `config.monorepo.as_ref().and_then(|m| m.dir.as_deref())`.
    pub fn monorepo_dir(&self) -> Option<&str> {
        self.monorepo.as_ref().and_then(|m| m.dir.as_deref())
    }

    /// The build targets compiled when neither a per-build `targets` nor
    /// `defaults.targets` is set: `defaults.targets` (when non-empty), else the
    /// canonical `DEFAULT_TARGETS`. Single source of truth for the target-set
    /// fallback — every target enumeration MUST resolve through this rather than
    /// re-deriving the fallback, so they never diverge.
    pub fn effective_default_targets(&self) -> Vec<String> {
        self.defaults
            .as_ref()
            .and_then(|d| d.targets.clone())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| {
                crate::target::DEFAULT_TARGETS
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect()
            })
    }

    /// The cross-compilation strategy applied to a crate that does not set its
    /// own `cross:` — `defaults.cross`, else `Auto`. SSOT for the per-crate
    /// strategy fallback.
    pub fn default_cross_strategy(&self) -> CrossStrategy {
        self.defaults
            .as_ref()
            .and_then(|d| d.cross.clone())
            .unwrap_or(CrossStrategy::Auto)
    }

    // --- Project metadata defaulting helpers ---
    //
    // Publishers that expose homepage/license/description/maintainer fields
    // fall back to these when their own field is unset, so a project only
    // needs to declare metadata once. Resolution precedence (highest first):
    //
    //   1. the per-publisher override (the publisher's own config field)
    //   2. a hand-written top-level `metadata:` YAML field
    //   3. the value derived from the crate's `Cargo.toml [package]` table
    //      (populated by `populate_derived_metadata`)
    //
    // Steps 1 is enforced by the publisher's `or_else(|| cfg.meta_*_for(..))`
    // chain; steps 2-3 are enforced inside the `meta_*_for` accessors. A
    // publisher that knows which crate it is publishing for should call the
    // crate-aware `meta_*_for(crate_name)` variant so workspace/per-crate
    // configs resolve each crate's OWN Cargo.toml metadata. The crate-agnostic
    // `meta_*` variants resolve the top-level `metadata:` block only (no
    // Cargo.toml fallback) and exist for truly project-level callers.

    /// Per-crate derived metadata for `crate_name`, if `Cargo.toml` supplied any.
    fn derived_for(&self, crate_name: &str) -> Option<&MetadataConfig> {
        self.derived_metadata.get(crate_name)
    }

    /// Name of the primary crate (first declared `crates:` entry, else the
    /// first workspace crate). Used as the metadata-derivation source and
    /// crate-name fallback for project-level publishers (e.g. top-level
    /// `homebrew_casks:`, `npms:`) that are not bound to a single crate.
    pub fn primary_crate_name(&self) -> Option<&str> {
        self.crate_universe().first().map(|c| c.name.as_str())
    }

    /// Project homepage: top-level `metadata.homepage` wins, else the primary
    /// crate's `Cargo.toml`-derived homepage. For project-level publishers
    /// (top-level casks) with no owning crate.
    pub fn meta_homepage_project(&self) -> Option<&str> {
        self.meta_homepage()
            .or_else(|| self.meta_homepage_for(self.primary_crate_name()?))
    }

    /// Project description: top-level `metadata.description` wins, else the
    /// primary crate's `Cargo.toml`-derived description.
    pub fn meta_description_project(&self) -> Option<&str> {
        self.meta_description()
            .or_else(|| self.meta_description_for(self.primary_crate_name()?))
    }

    /// Project source-repository URL: top-level `metadata.repository` wins, else
    /// the primary crate's `Cargo.toml`-derived repository. Backs the
    /// `{{ Metadata.Repository }}` template var.
    pub fn meta_repository_project(&self) -> Option<&str> {
        self.meta_repository()
            .or_else(|| self.meta_repository_for(self.primary_crate_name()?))
    }

    /// Project license: top-level `metadata.license` wins, else the primary
    /// crate's `Cargo.toml`-derived license. For the `{{ Metadata.License }}`
    /// template var and project-level publishers with no owning crate.
    pub fn meta_license_project(&self) -> Option<&str> {
        self.meta_license()
            .or_else(|| self.meta_license_for(self.primary_crate_name()?))
    }

    /// Project documentation URL: top-level `metadata.documentation` wins, else
    /// the primary crate's `Cargo.toml`-derived documentation URL.
    pub fn meta_documentation_project(&self) -> Option<&str> {
        self.meta_documentation()
            .or_else(|| self.meta_documentation_for(self.primary_crate_name()?))
    }

    /// Project homepage from `metadata.homepage` (top-level YAML only).
    pub fn meta_homepage(&self) -> Option<&str> {
        self.metadata.as_ref().and_then(|m| m.homepage.as_deref())
    }

    /// Project license from `metadata.license` (top-level YAML only).
    pub fn meta_license(&self) -> Option<&str> {
        self.metadata.as_ref().and_then(|m| m.license.as_deref())
    }

    /// Project source-repository URL from `metadata.repository` (top-level YAML only).
    pub fn meta_repository(&self) -> Option<&str> {
        self.metadata.as_ref().and_then(|m| m.repository.as_deref())
    }

    /// Project description from `metadata.description` (top-level YAML only).
    pub fn meta_description(&self) -> Option<&str> {
        self.metadata
            .as_ref()
            .and_then(|m| m.description.as_deref())
    }

    /// Project documentation URL from `metadata.documentation` (top-level YAML only).
    pub fn meta_documentation(&self) -> Option<&str> {
        self.metadata
            .as_ref()
            .and_then(|m| m.documentation.as_deref())
    }

    /// Project maintainers from `metadata.maintainers` (top-level YAML only).
    pub fn meta_maintainers(&self) -> &[String] {
        self.metadata
            .as_ref()
            .and_then(|m| m.maintainers.as_deref())
            .unwrap_or(&[])
    }

    /// First maintainer as "Name <email>" or just "Name" (publisher convention).
    /// Returns None when no maintainers are configured.
    pub fn meta_first_maintainer(&self) -> Option<&str> {
        self.meta_maintainers().first().map(|s| s.as_str())
    }

    /// Homepage for `crate_name`: top-level `metadata.homepage` wins, else the
    /// value derived from the crate's `Cargo.toml [package]`.
    pub fn meta_homepage_for(&self, crate_name: &str) -> Option<&str> {
        self.meta_homepage()
            .or_else(|| self.derived_for(crate_name)?.homepage.as_deref())
    }

    /// License for `crate_name`: top-level `metadata.license` wins, else the
    /// crate's `Cargo.toml [package].license` (never synthesised from
    /// `license-file`).
    pub fn meta_license_for(&self, crate_name: &str) -> Option<&str> {
        self.meta_license()
            .or_else(|| self.derived_for(crate_name)?.license.as_deref())
    }

    /// Source-repository URL for `crate_name`: top-level `metadata.repository`
    /// wins, else the crate's `Cargo.toml [package].repository`. Feeds the npm
    /// `package.json` `repository` field so npm provenance validation (which
    /// matches it against the OIDC-claimed repository) passes without requiring
    /// the operator to restate the URL in the publisher config.
    pub fn meta_repository_for(&self, crate_name: &str) -> Option<&str> {
        self.meta_repository()
            .or_else(|| self.derived_for(crate_name)?.repository.as_deref())
    }

    /// Description for `crate_name`: top-level `metadata.description` wins, else
    /// the crate's `Cargo.toml [package].description`.
    pub fn meta_description_for(&self, crate_name: &str) -> Option<&str> {
        self.meta_description()
            .or_else(|| self.derived_for(crate_name)?.description.as_deref())
    }

    /// Documentation URL for `crate_name`: top-level `metadata.documentation`
    /// wins, else the crate's `Cargo.toml [package].documentation`.
    pub fn meta_documentation_for(&self, crate_name: &str) -> Option<&str> {
        self.meta_documentation()
            .or_else(|| self.derived_for(crate_name)?.documentation.as_deref())
    }

    /// Maintainers for `crate_name`: top-level `metadata.maintainers` wins
    /// (when non-empty), else the crate's `Cargo.toml [package].authors`.
    pub fn meta_maintainers_for(&self, crate_name: &str) -> &[String] {
        let top = self.meta_maintainers();
        if !top.is_empty() {
            return top;
        }
        self.derived_for(crate_name)
            .and_then(|m| m.maintainers.as_deref())
            .unwrap_or(&[])
    }

    /// First maintainer for `crate_name` as "Name <email>" or just "Name".
    pub fn meta_first_maintainer_for(&self, crate_name: &str) -> Option<&str> {
        self.meta_maintainers_for(crate_name)
            .first()
            .map(|s| s.as_str())
    }

    /// Vendor / distributing-entity name for `crate_name`: the first
    /// maintainer with any `<email>` suffix stripped (e.g.
    /// `"Ada Lovelace <ada@x>"` → `"Ada Lovelace"`). `None` when no maintainer
    /// is derivable or the result is empty, so a Vendor field is never emitted
    /// blank. Reused by the rpm/deb Vendor and the OCI image `vendor` label.
    pub fn meta_vendor_for(&self, crate_name: &str) -> Option<String> {
        self.meta_first_maintainer_for(crate_name)
            .and_then(maintainer_name_only)
    }

    /// Populate [`Config::derived_metadata`] by reading each crate's
    /// `Cargo.toml [package]` table (description / license / homepage /
    /// authors), so publishers resolve a plain Rust project's metadata without
    /// requiring a top-level `metadata:` YAML block.
    ///
    /// Covers every crate the config knows about: top-level `crates:` plus
    /// every `workspaces[].crates[]`, so single-crate, workspace-lockstep, and
    /// per-crate configs all populate. Each crate is read from
    /// `<crate.path>/Cargo.toml` relative to `base_dir` (the directory the
    /// config was loaded from / the monorepo working directory).
    ///
    /// Idempotent and non-destructive: only fills entries; existing
    /// `derived_metadata` keys are overwritten with a fresh read. Crates whose
    /// `Cargo.toml` is missing or supplies nothing contribute an all-`None`
    /// entry (harmless — the accessors treat it as "no value").
    pub fn populate_derived_metadata(&mut self, base_dir: &std::path::Path) {
        let crate_paths: Vec<(String, String)> = self
            .crate_universe()
            .into_iter()
            .map(|c| (c.name.clone(), c.path.clone()))
            .collect();
        for (name, path) in crate_paths {
            let crate_dir = base_dir.join(&path);
            let derived = derive_metadata_from_cargo_toml(&crate_dir);
            self.derived_metadata.insert(name, derived);
        }
    }

    /// Populate `depends_on` for every crate entry that OMITS it, by reading
    /// the crate's `Cargo.toml` dependency tables (`[dependencies]`,
    /// `[build-dependencies]`, and every `[target.'cfg(...)'.dependencies]`)
    /// and matching against the real on-disk Cargo workspace's member names
    /// ([`discover_cargo_workspace_member_names`]) — the same derivation
    /// `anodizer init` performs at scaffold time
    /// ([`derive_depends_on_from_cargo_toml`]), now re-run at every
    /// config-load so a hand-maintained `crates:` list can never drift stale
    /// behind the crate's real Cargo.toml dependencies.
    /// An explicit `depends_on` (`Some(_)`) is a user override and is never
    /// overwritten.
    ///
    /// Covers every crate the config knows about (top-level `crates:` plus
    /// every `workspaces[].crates[]`), mirroring
    /// [`Self::populate_derived_metadata`]. A single-crate project (no
    /// `crates:` at all) has an empty crate universe, so this is a no-op.
    /// A project with no on-disk Cargo workspace (no root `Cargo.toml`, or
    /// a plain single-package `Cargo.toml`) has only itself as a "member",
    /// so derivation naturally yields no deps.
    pub fn populate_derived_depends_on(&mut self, base_dir: &std::path::Path) {
        let member_names = discover_cargo_workspace_member_names(base_dir);
        if member_names.is_empty() {
            return;
        }

        let derived: HashMap<String, Vec<String>> = self
            .crate_universe()
            .into_iter()
            .filter(|c| c.depends_on.is_none())
            .map(|c| {
                let crate_dir = base_dir.join(&c.path);
                let deps = derive_depends_on_from_cargo_toml(&crate_dir, &member_names);
                (c.name.clone(), deps)
            })
            .collect();

        if derived.is_empty() {
            return;
        }

        for c in self.crates.iter_mut().chain(
            self.workspaces
                .iter_mut()
                .flatten()
                .flat_map(|ws| ws.crates.iter_mut()),
        ) {
            if c.depends_on.is_none()
                && let Some(deps) = derived.get(&c.name)
            {
                c.depends_on = Some(deps.clone());
            }
        }
    }
}
