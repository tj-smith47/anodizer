use super::*;

impl Context {
    /// Populate template variables from `self.git_info`.
    ///
    /// Must be called after `self.git_info` is set. Sets the following vars:
    /// - `Tag`, `Version`, `RawVersion` — tag and version strings
    /// - `Major`, `Minor`, `Patch` — semver components
    /// - `Prerelease` — prerelease suffix (or empty)
    /// - `BuildMetadata` — build metadata from semver tag (or empty)
    /// - `FullCommit`, `Commit` — full commit SHA (`Commit` is alias for `FullCommit`)
    /// - `ShortCommit` — abbreviated commit SHA
    /// - `Branch` — current git branch
    /// - `CommitDate` — ISO 8601 author date of HEAD commit
    /// - `CommitTimestamp` — unix timestamp of HEAD commit
    /// - `IsGitDirty` — "true"/"false"
    /// - `IsGitClean` — "true"/"false" (inverse of `IsGitDirty`)
    /// - `GitTreeState` — "clean"/"dirty"
    /// - `GitURL` — git remote URL
    /// - `Summary` — git describe summary
    /// - `TagSubject` — annotated tag subject or commit subject
    /// - `TagContents` — full annotated tag message or commit message
    /// - `TagBody` — tag message body or commit message body
    /// - `IsSnapshot` — from context options
    /// - `IsNightly` — from context options
    /// - `IsDraft` — "false" (stages may override to "true")
    /// - `IsSingleTarget` — "true"/"false" based on single_target option
    /// - `PreviousTag` — previous matching tag, stripped in monorepo mode (or empty)
    /// - `PrefixedTag` — full tag with monorepo prefix, or tag_prefix-prepended (Pro addition)
    /// - `PrefixedPreviousTag` — full previous tag with prefix (Pro addition)
    /// - `PrefixedSummary` — full summary with prefix (Pro addition)
    /// - `IsRelease` — "true" if not snapshot and not nightly (Pro addition)
    /// - `IsMerging` — "true" if running with --merge flag (Pro addition)
    ///
    /// **Stage-scoped variables** (NOT set here; set per-artifact during stage execution):
    /// - `Binary` — binary name, set by build stage per binary and archive stage per archive
    /// - `ArtifactName` — output artifact filename, set by archive stage after creating each archive
    /// - `ArtifactPath` — absolute path to artifact, set by archive stage after creating each archive
    /// - `ArtifactExt` — artifact file extension (e.g. `.tar.gz`, `.exe`), set alongside ArtifactName
    /// - `ArtifactID` — build config `id` field, set by build stage per build config
    /// - `Os` — target OS, set by archive/nfpm stages per target
    /// - `Arch` — target architecture, set by archive/nfpm stages per target
    /// - `Target` — full target triple (e.g. `x86_64-unknown-linux-gnu`), set alongside Os/Arch
    /// - `Checksums` — combined checksum file contents, set by checksum stage
    pub fn populate_git_vars(&mut self) {
        if let Some(ref info) = self.git_info {
            // The version-derived var block (Tag/Version/RawVersion/Base/Major/
            // Minor/Patch/Prerelease/BuildMetadata) is factored into
            // `set_version_vars` so `render_template_for_version` can re-derive
            // the SAME block for a promotion's target version without drift.
            // Deriving Version/RawVersion from the parsed `SemVer` struct (not
            // `tag.strip_prefix('v')`) handles monorepo tags like `core-v0.3.2`.
            set_version_vars(&mut self.template_vars, &info.semver, &info.tag);
            self.template_vars.set("FullCommit", &info.commit);
            self.template_vars.set("Commit", &info.commit);
            self.template_vars.set("ShortCommit", &info.short_commit);
            self.template_vars.set("Branch", &info.branch);
            self.template_vars.set("CommitDate", &info.commit_date);
            self.template_vars
                .set("CommitTimestamp", &info.commit_timestamp);
            self.template_vars.set_bool("IsGitDirty", info.dirty);
            self.template_vars.set_bool("IsGitClean", !info.dirty);
            self.template_vars
                .set("GitTreeState", if info.dirty { "dirty" } else { "clean" });
            self.template_vars.set("GitURL", &info.remote_url);
            self.template_vars.set("Summary", &info.summary);
            self.template_vars.set("TagSubject", &info.tag_subject);
            self.template_vars.set("TagContents", &info.tag_contents);
            self.template_vars.set("TagBody", &info.tag_body);
            self.template_vars
                .set("PreviousTag", info.previous_tag.as_deref().unwrap_or(""));
            self.template_vars
                .set("FirstCommit", info.first_commit.as_deref().unwrap_or(""));

            // Pro additions: PrefixedTag, PrefixedPreviousTag, PrefixedSummary
            //
            // When monorepo.tag_prefix is configured, the git tag already
            // contains the prefix (e.g. "subproject1/v1.2.3"). In this case:
            //   - Tag = prefix stripped (e.g. "v1.2.3")
            //   - PrefixedTag = full tag (e.g. "subproject1/v1.2.3")
            //   - PrefixedPreviousTag = full previous tag
            //
            // When monorepo is NOT configured, fall back to the original
            // behavior: prepend tag.tag_prefix to construct PrefixedTag.
            let monorepo_prefix = self.config.monorepo_tag_prefix();

            // monorepo.tag_prefix takes precedence over tag.tag_prefix for
            // PrefixedTag / PrefixedPreviousTag / PrefixedSummary behavior.
            // When monorepo is configured, info.tag and info.summary already
            // contain the prefix from git, so we strip for the base vars and
            // use the raw values for the Prefixed variants.
            if let Some(prefix) = monorepo_prefix {
                // Monorepo mode: the tag in git_info is the FULL prefixed tag.
                // PrefixedTag = full tag (already has prefix).
                self.template_vars.set("PrefixedTag", &info.tag);

                // Tag = prefix stripped. Override the Tag we set above.
                let stripped_tag = crate::git::strip_monorepo_prefix(&info.tag, prefix);
                self.template_vars.set("Tag", stripped_tag);

                // Version: derived from the parsed SemVer struct (same source as
                // the non-monorepo path and the build stage's per-crate
                // re-scoping) so all three stay byte-identical. `info.semver`
                // was parsed from the full prefixed tag, so it already excludes
                // the monorepo prefix — no separate string-strip needed.
                //
                // For a non-semver tag under `--skip=validate`, info.semver is
                // the skip-validate fallback, so this yields "0.0.0" rather than
                // the old raw prefix-stripped string.
                let version = info.semver.version_string();
                self.template_vars.set("Version", &version);

                // PrefixedPreviousTag = full previous tag (already has prefix).
                let prev_tag = info.previous_tag.as_deref().unwrap_or("");
                self.template_vars.set("PrefixedPreviousTag", prev_tag);

                // PreviousTag = prefix stripped, consistent with Tag being stripped.
                let stripped_prev = crate::git::strip_monorepo_prefix(prev_tag, prefix);
                self.template_vars.set("PreviousTag", stripped_prev);

                // PrefixedSummary: info.summary from `git describe` already
                // includes the monorepo prefix (e.g. "subproject1/v1.2.3-0-gabc123d"),
                // so use it as-is for the prefixed variant.
                self.template_vars.set("PrefixedSummary", &info.summary);
                // Summary: strip the monorepo prefix for the base variant.
                let stripped_summary = crate::git::strip_monorepo_prefix(&info.summary, prefix);
                self.template_vars.set("Summary", stripped_summary);
            } else {
                // Non-monorepo: prepend tag.tag_prefix to construct PrefixedTag.
                let tag_prefix = self
                    .config
                    .tag
                    .as_ref()
                    .and_then(|t| t.tag_prefix.as_deref())
                    .unwrap_or("");
                self.template_vars
                    .set("PrefixedTag", &format!("{}{}", tag_prefix, info.tag));
                let prev_tag = info.previous_tag.as_deref().unwrap_or("");
                let prefixed_prev = if prev_tag.is_empty() {
                    String::new()
                } else {
                    format!("{}{}", tag_prefix, prev_tag)
                };
                self.template_vars
                    .set("PrefixedPreviousTag", &prefixed_prev);
                self.template_vars.set(
                    "PrefixedSummary",
                    &format!("{}{}", tag_prefix, info.summary),
                );
            }
        }

        // `NightlyBuild`: stateless per-base-version build counter derived
        // from `git rev-list --count <last-tag>..HEAD`. Resets automatically
        // when a new version tag lands (no state anodizer persists). Set
        // unconditionally (it is just a count), but intended for nightly /
        // snapshot `version_template`s such as
        // `"{{ .Base }}-nightly.{{ .NightlyBuild }}+{{ .ShortCommit }}"`.
        // Defaults to "0" outside a git repo (synthetic snapshot/scratch
        // builds) and on any git error so templates never fail to render.
        //
        // The monorepo prefix constrains the last-tag lookup to the active
        // crate's tags so per-crate workspace runs count since the right
        // tag (not the nearest tag from another subproject).
        let nightly_build = if self.git_info.is_some() {
            let root = self
                .options
                .project_root
                .clone()
                .unwrap_or_else(|| PathBuf::from("."));
            let monorepo_prefix = self.config.monorepo_tag_prefix();
            crate::git::count_commits_since_last_tag_in(&root, monorepo_prefix).unwrap_or(0)
        } else {
            0
        };
        self.template_vars
            .set_structured("NightlyBuild", serde_json::Value::from(nightly_build));

        // Mode flags are injected as real bools (not "true"/"false" strings)
        // so `not IsSnapshot` / `IsSnapshot == false` / bare `{% if … %}`
        // forms all evaluate correctly; `{{ IsSnapshot }}` interpolation
        // still renders "true"/"false".
        self.template_vars
            .set_bool("IsSnapshot", self.options.snapshot);
        self.template_vars
            .set_bool("IsNightly", self.options.nightly);
        // Surfaced to user `if_condition:` templates so stages can
        // selectively run inside the determinism harness even when
        // `not IsSnapshot` would otherwise skip them.
        self.template_vars.set_bool(
            "IsHarness",
            self.env_var("ANODIZER_IN_DETERMINISM_HARNESS").is_some(),
        );
        // Wire IsDraft from `release.draft`.
        let is_draft = self
            .config
            .release
            .as_ref()
            .and_then(|r| r.draft)
            .unwrap_or(false);
        self.template_vars.set_bool("IsDraft", is_draft);
        self.template_vars
            .set_bool("IsSingleTarget", self.options.single_target.is_some());

        // Pro addition: IsRelease — true if this is a regular release (not snapshot, not nightly).
        let is_release = !self.options.snapshot && !self.options.nightly;
        self.template_vars.set_bool("IsRelease", is_release);

        // Pro addition: IsMerging — true if running with --merge flag.
        self.template_vars.set_bool("IsMerging", self.options.merge);
    }

    /// Populate time-related template variables.
    ///
    /// Sets:
    /// - `Date` — UTC time as RFC 3339
    /// - `Timestamp` — unix timestamp as string
    /// - `Now` — UTC time as RFC 3339
    /// - `Year` — four-digit year (e.g. "2026")
    /// - `Month` — zero-padded month (e.g. "03")
    /// - `Day` — zero-padded day (e.g. "30")
    /// - `Hour` — zero-padded hour (e.g. "14")
    /// - `Minute` — zero-padded minute (e.g. "05")
    ///
    /// Time source resolution (first match wins):
    ///
    /// 1. `SOURCE_DATE_EPOCH` env var — the standard reproducibility contract
    ///    (set by the determinism harness on every child release subprocess,
    ///    and the conventional way external CI / packagers signal a fixed
    ///    epoch). This is load-bearing for byte-stability of `metadata.json`
    ///    (which embeds `Date`) and any user template that consumes `Date` /
    ///    `Timestamp` / `Now`. Without this branch, two from-clean runs of
    ///    the same commit emit metadata.json files that differ in the `date`
    ///    field, defeating release-asset idempotency.
    /// 2. `chrono::Utc::now()` — wall-clock fallback. The
    ///    legacy semantics for runs without SDE wired in. Note that the
    ///    template docs explicitly call `.Now` "not deterministic"
    ///    — under SDE-aware reproducible builds we deviate from that
    ///    behavior intentionally.
    pub fn populate_time_vars(&mut self) {
        // Resolution order (SDE first, else wall-clock) is centralized in
        // `crate::sde::resolve_now_with_env` so any caller —
        // `populate_time_vars`, Tera built-ins, stage-srpm's `%changelog`
        // date, nightly `date_str` — sees identical "now" semantics.
        // Routes through the injected `env_source` so tests can inject
        // SOURCE_DATE_EPOCH via TestContextBuilder::env() without
        // mutating the process env.
        let now = crate::sde::resolve_now_with_env(self.env_source());
        self.template_vars.set("Date", &now.to_rfc3339());
        self.template_vars
            .set("Timestamp", &now.timestamp().to_string());
        self.template_vars.set("Now", &now.to_rfc3339());
        self.template_vars
            .set("Year", &now.format("%Y").to_string());
        self.template_vars
            .set("Month", &now.format("%m").to_string());
        self.template_vars.set("Day", &now.format("%d").to_string());
        self.template_vars
            .set("Hour", &now.format("%H").to_string());
        self.template_vars
            .set("Minute", &now.format("%M").to_string());
    }

    /// Populate runtime environment variables.
    ///
    /// Sets:
    /// - `RuntimeGoos` — host OS in Go-compatible naming (e.g. "linux", "darwin", "windows")
    /// - `RuntimeGoarch` — host architecture in Go-compatible naming (e.g. "amd64", "arm64")
    /// - `Runtime_Goos` / `Runtime_Goarch` — nested aliases
    /// - `RustcVersion` — host rustc release version (e.g. "1.96.0"), or "" when
    ///   rustc is unavailable
    pub fn populate_runtime_vars(&mut self) {
        let goos = map_os_to_goos(std::env::consts::OS);
        let goarch = map_arch_to_goarch(std::env::consts::ARCH);
        self.template_vars.set("RuntimeGoos", goos);
        self.template_vars.set("RuntimeGoarch", goarch);
        // Runtime.Goos / Runtime.Goarch — after preprocessing
        // the dot becomes an underscore-separated flat key. We expose both forms.
        self.template_vars.set("Runtime_Goos", goos);
        self.template_vars.set("Runtime_Goarch", goarch);
        // RustcVersion is a host-environment fact like OS/arch, so it is set in
        // the same call — keeping it a separate populate step risks a call-site
        // forgetting to invoke the sibling.
        self.populate_rustc_vars();
    }

    /// Populate the `RustcVersion` built-in template variable.
    ///
    /// Probes `rustc -vV` and extracts the `release:` line (e.g. `"1.96.0"`).
    /// Sets `RustcVersion` to the extracted string, or to `""` when rustc is
    /// unavailable or the line is absent — templates that reference
    /// `{{ .RustcVersion }}` degrade to an empty value rather than erroring.
    fn populate_rustc_vars(&mut self) {
        let ver = crate::partial::detect_rustc_version().unwrap_or_default();
        self.template_vars.set("RustcVersion", &ver);
    }

    /// Populate the `ReleaseNotes` template variable from stored changelogs.
    ///
    /// Should be called after the changelog stage has run and populated
    /// `self.stage_outputs.changelogs`. Uses the first crate (by crate
    /// universe order — top-level `crates:` then every `workspaces[].crates`
    /// entry) whose changelog is present, or an empty string if no
    /// changelogs exist. Universe order is deterministic, unlike HashMap
    /// iteration order.
    pub fn populate_release_notes_var(&mut self) {
        // Look up changelogs in universe order for determinism. The universe
        // walk (not `config.crates`) is what lets a pure-`workspaces:` config
        // resolve a non-empty `ReleaseNotes` — its crates carry the
        // changelogs but never appear in the top-level list.
        let notes = self
            .config
            .crate_universe()
            .into_iter()
            .find_map(|c| self.stage_outputs.changelogs.get(&c.name))
            .cloned()
            .unwrap_or_default();
        self.template_vars.set("ReleaseNotes", &notes);
    }

    /// Refresh the `Artifacts` structured template variable from the current
    /// artifact registry. Should be called before rendering release body and
    /// announce templates so they can iterate over all artifacts.
    ///
    /// Each artifact is serialized as a map with keys: `name`, `path`, `target`,
    /// `kind`, `crate_name`, and `metadata`.
    ///
    /// **Known metadata keys** (populated by individual stages):
    /// - `format` — archive format (e.g. `"tar.gz"`, `"zip"`), set by archive stage
    /// - `extra_file` — `"true"` when artifact is an extra file, set by checksum stage
    /// - `extra_name_template` — name template override for extra files, set by checksum stage
    /// - `digest` — docker image digest (e.g. `sha256:abc123...`), set by docker stage
    /// - `id` — artifact ID from config, set by docker and build stages
    /// - `binary` — binary name, set by build stage
    pub fn refresh_artifacts_var(&mut self) {
        // CSV metadata keys we expose as JSON arrays for template iteration.
        // Storage remains HashMap<String,String> (flat); only the
        // template-exposed view is expanded. The
        // ExtraBinaries / ExtraFiles list semantics.
        const CSV_LIST_KEYS: &[&str] = &["extra_binaries", "extra_files"];
        // JSON-encoded list metadata keys: stored as a JSON-array string in
        // `HashMap<String,String>`, exposed as a real array on the template
        // side so `{% for p in .Artifacts[0].metadata.Platforms %}` works.
        // `Platforms` is the platform-list slice on
        // `DockerImageV2` artifacts.
        const JSON_LIST_KEYS: &[&str] = &["Platforms"];

        let artifacts_value: Vec<serde_json::Value> = self
            .artifacts
            .all()
            .iter()
            .map(|a| {
                // Rebuild metadata map converting known CSV keys into arrays.
                let mut metadata_map = serde_json::Map::with_capacity(a.metadata.len());
                for (k, v) in &a.metadata {
                    if CSV_LIST_KEYS.contains(&k.as_str()) {
                        let items: Vec<serde_json::Value> = if v.is_empty() {
                            Vec::new()
                        } else {
                            v.split(',')
                                .map(|s| serde_json::Value::String(s.to_string()))
                                .collect()
                        };
                        metadata_map.insert(k.clone(), serde_json::Value::Array(items));
                    } else if JSON_LIST_KEYS.contains(&k.as_str()) {
                        // Decode JSON-array string into a real Value::Array;
                        // a malformed value falls back to the raw string so
                        // custom publishers can still inspect it.
                        let parsed = serde_json::from_str::<serde_json::Value>(v)
                            .unwrap_or_else(|_| serde_json::Value::String(v.clone()));
                        metadata_map.insert(k.clone(), parsed);
                    } else {
                        metadata_map.insert(k.clone(), serde_json::Value::String(v.clone()));
                    }
                }
                serde_json::json!({
                    "name": a.name,
                    "path": a.path.to_string_lossy(),
                    "target": a.target.as_deref().unwrap_or(""),
                    "kind": a.kind.as_str(),
                    "crate_name": a.crate_name,
                    "metadata": serde_json::Value::Object(metadata_map),
                })
            })
            .collect();
        self.template_vars
            .set_structured("Artifacts", serde_json::Value::Array(artifacts_value));
    }

    /// Populate the `Metadata` structured template variable from config.metadata.
    ///
    /// Exposes the project metadata block as a nested map with PascalCase keys
    /// the `.Metadata.*` namespace:
    /// `Description`, `Homepage`, `Documentation`, `License`, `Repository`,
    /// `Maintainers`, `ModTimestamp`, `FullDescription` (resolved),
    /// `CommitAuthor.{Name,Email}`.
    /// Missing fields default to empty strings / empty arrays.
    ///
    /// `full_description` supports `Inline`, `FromFile` (template-rendered
    /// path, read from disk), and `FromUrl` (template-rendered URL +
    /// headers, fetched through [`crate::content_source::resolve`] which
    /// applies retries, body caps, and CR/LF header-injection guards).
    pub fn populate_metadata_var(&mut self) -> anyhow::Result<()> {
        // Clone the small scalar fields so we don't hold a borrow on self.config
        // across the render_template calls below.
        let (
            description,
            homepage,
            documentation,
            license,
            repository,
            maintainers,
            mod_timestamp,
            full_desc_src,
            commit_author,
        ) = {
            let meta = self.config.metadata.as_ref();
            // Description / homepage / documentation / license resolve through
            // the project-level fallback: top-level `metadata.*` wins, else the
            // primary crate's `Cargo.toml`-derived value. This keeps
            // `{{ Metadata.* }}` single-sourced with the per-publisher
            // `meta_*_for` resolvers, so dropping a redundant `metadata.license`
            // (derivable from Cargo.toml) does not silently empty the var.
            let description = self
                .config
                .meta_description_project()
                .unwrap_or("")
                .to_string();
            let homepage = self
                .config
                .meta_homepage_project()
                .unwrap_or("")
                .to_string();
            let documentation = self
                .config
                .meta_documentation_project()
                .unwrap_or("")
                .to_string();
            let license = self.config.meta_license_project().unwrap_or("").to_string();
            let repository = self
                .config
                .meta_repository_project()
                .unwrap_or("")
                .to_string();
            let maintainers: Vec<String> = meta
                .and_then(|m| m.maintainers.as_ref())
                .cloned()
                .unwrap_or_default();
            let mod_timestamp = meta
                .and_then(|m| m.mod_timestamp.as_deref())
                .unwrap_or("")
                .to_string();
            let full_desc_src = meta.and_then(|m| m.full_description.clone());
            let commit_author = meta.and_then(|m| m.commit_author.clone());
            (
                description,
                homepage,
                documentation,
                license,
                repository,
                maintainers,
                mod_timestamp,
                full_desc_src,
                commit_author,
            )
        };

        // Resolve full_description through the shared ContentSource resolver
        // so Inline, FromFile (template-rendered path), and FromUrl
        // (template-rendered URL + headers, retried HTTP fetch with
        // body cap and CR/LF guard) all behave the same as the release
        // header/footer fields.
        let full_description = match full_desc_src {
            None => String::new(),
            Some(src) => crate::content_source::resolve(
                &src,
                "metadata.full_description",
                self,
                &self.logger("metadata"),
            )?,
        };

        let commit_author_map = serde_json::json!({
            "Name": commit_author.as_ref().and_then(|c| c.name.clone()).unwrap_or_default(),
            "Email": commit_author.as_ref().and_then(|c| c.email.clone()).unwrap_or_default(),
        });

        let meta_map = serde_json::json!({
            "Description": description,
            "Homepage": homepage,
            "Documentation": documentation,
            "License": license,
            "Repository": repository,
            "Maintainers": maintainers,
            "ModTimestamp": mod_timestamp,
            "FullDescription": full_description,
            "CommitAuthor": commit_author_map,
        });
        self.template_vars.set_structured("Metadata", meta_map);
        Ok(())
    }
}

/// Map Rust's `std::env::consts::OS` to Go-compatible GOOS naming.
/// Templates expect Go runtime names (e.g. "darwin" not "macos").
pub fn map_os_to_goos(os: &str) -> &str {
    match os {
        "macos" => "darwin",
        other => other, // linux, windows, freebsd, etc. already match
    }
}

/// Map Rust's `std::env::consts::ARCH` to Go-compatible GOARCH naming.
/// Templates expect Go runtime names (e.g. "amd64" not "x86_64").
///
/// Delegates to the shared [`crate::target::rust_arch_to_goarch`] table so a
/// host-derived `{{ .Runtime.Goarch }}` can never disagree with the
/// triple-derived arch tokens in asset names. `ARCH` doesn't encode
/// endianness, so the host's own compile-time endianness disambiguates
/// `powerpc64`/`mips64`. Tokens outside the table (`arm` — GOARCH really is
/// "arm" — plus exotics) pass through unchanged.
pub fn map_arch_to_goarch(arch: &str) -> &str {
    crate::target::rust_arch_to_goarch(arch, cfg!(target_endian = "little")).unwrap_or(arch)
}
