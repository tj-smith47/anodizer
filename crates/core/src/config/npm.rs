use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{StringOrBool, deserialize_string_or_bool_opt};

/// Binary-distribution strategy for an [`NpmConfig`] entry.
///
/// `optional-deps` (the default for a Rust release) emits npm's native
/// platform-resolution layout: one thin per-platform package whose
/// `os`/`cpu`/`libc` selectors are derived from the built target triples,
/// plus a metapackage that lists every platform package under
/// `optionalDependencies` and ships a `bin` shim resolving the installed
/// one via `require.resolve`. npm installs only the matching platform
/// package; there is no download and no postinstall script. This is the
/// pattern leading Rust CLIs ship binaries through npm with (biome,
/// git-cliff).
///
/// `postinstall` emits a single package carrying a `postinstall.js` shim that
/// downloads + sha256-verifies the OS/arch-matching release archive at
/// `npm install` time — for registries or policies that disallow per-platform
/// packages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum NpmMode {
    /// Emit per-platform packages + a metapackage with `optionalDependencies`
    /// and a `require.resolve` bin shim. npm's native `os`/`cpu`/`libc`
    /// resolution selects the right prebuilt package — no download, no
    /// postinstall. Default.
    #[default]
    OptionalDeps,
    /// Emit a single package with a `postinstall.js` shim that downloads and
    /// sha256-verifies the matching archive at install time.
    Postinstall,
}

/// NPM package registry publisher configuration.
///
/// In the default `optional-deps` mode anodizer emits one thin npm package
/// per built platform (with `os`/`cpu`/`libc` selectors derived from the
/// target triple) plus a metapackage whose `optionalDependencies` lists
/// every platform package; npm's native resolution installs only the one
/// matching the host. In `postinstall` mode a single package carries a
/// `postinstall` script that downloads the matching release archive at
/// `npm install` time. Each `npms[]` entry produces one publish.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NpmConfig {
    /// Unique identifier for selecting this entry from the CLI (`--id=...`).
    pub id: Option<String>,

    /// Build IDs filter: only include artifacts whose archive `id` is in this list.
    pub ids: Option<Vec<String>>,

    /// Binary-distribution strategy. `optional-deps` (default) emits npm's
    /// native per-platform packages; `postinstall` emits a download shim.
    #[serde(default)]
    pub mode: NpmMode,

    /// npm scope for the per-platform packages emitted in `optional-deps`
    /// mode (e.g. `@biomejs`). The per-platform packages are named
    /// `<scope>/<bin>-<os>-<cpu>[-<libc>]`. Required for `optional-deps`
    /// mode; ignored in `postinstall` mode.
    pub scope: Option<String>,

    /// Metapackage name for `optional-deps` mode (e.g. `biome`). This is the
    /// package users `npm install`; it lists every per-platform package under
    /// `optionalDependencies` and ships the `bin` shim. Falls back to `name`
    /// (or the crate name) when unset.
    pub metapackage: Option<String>,

    /// Command name installed by the metapackage's `bin` map (`optional-deps`
    /// mode). Falls back to the metapackage basename when unset.
    pub bin: Option<String>,

    /// In `optional-deps` mode, emit separate per-platform packages for linux
    /// `musl` vs `glibc` (distinguished by the npm `libc` selector). When
    /// `false`, a single linux package per cpu is emitted with no `libc`
    /// selector. Default `true` — musl and glibc binaries are not
    /// interchangeable, so collapsing them risks installing the wrong one.
    #[serde(default = "default_libc_aware")]
    pub libc_aware: bool,

    /// NPM package name (the metapackage / postinstall package). May be scoped
    /// (`@org/foo`) or unscoped (`foo`). Falls back to the crate name when unset.
    pub name: Option<String>,

    /// Templated package description. Falls back to the project-level
    /// `metadata.description` when unset.
    pub description: Option<String>,

    /// Templated homepage URL. Falls back to `metadata.homepage` when unset.
    pub homepage: Option<String>,

    /// NPM `keywords` list.
    pub keywords: Option<Vec<String>>,

    /// Templated SPDX license identifier (e.g. `MIT`, `Apache-2.0`).
    /// Falls back to `metadata.license` when unset.
    pub license: Option<String>,

    /// Templated `author` field for `package.json`. Falls back to the
    /// project's `metadata.maintainers[0]`, and then to the crate's
    /// `Cargo.toml [package].authors[0]`, when unset.
    pub author: Option<String>,

    /// npm `engines` constraint map written verbatim into `package.json`
    /// (e.g. `{ node: ">=18" }`). When unset, anodizer emits a sensible
    /// default of `{ node: ">=18" }` — the floor every leading native-CLI
    /// wrapper (esbuild, biome, swc) declares. Set to an empty map to
    /// suppress the field entirely.
    pub engines: Option<std::collections::BTreeMap<String, String>>,

    /// Explicit npm `files` allowlist written into `package.json`. When
    /// unset, anodizer derives it from what each package actually ships
    /// (the per-platform binary, the metapackage `shim.js`, or the
    /// postinstall launcher/script) plus any `extra_files` basenames. Set
    /// to an empty list to suppress the field (npm then falls back to its
    /// implicit inclusion rules).
    pub files: Option<Vec<String>>,

    /// npm `publishConfig.provenance` flag. When unset, anodizer emits
    /// `true` — the npm supply-chain norm that biome and swc both set,
    /// pairing with anodizer's signing story. Set to `false` to disable.
    pub provenance: Option<bool>,

    /// Templated repository URL. Emitted as `repository.url` in
    /// `package.json` with `type: git`.
    pub repository: Option<String>,

    /// Templated bug tracker URL. Emitted as `bugs.url` in `package.json`.
    pub bugs: Option<String>,

    /// NPM access level for scoped packages. Accepts `"public"` /
    /// `"restricted"`. Scoped packages on npmjs.org default to
    /// `restricted` unless this is set to `public`.
    pub access: Option<String>,

    /// NPM dist-tag for the publish (default `latest`). Templated.
    pub tag: Option<String>,

    /// Archive format the `postinstall` script downloads
    /// (`tgz`, `tar.gz`, `tar`, `zip`, `binary`). Default `tgz`. Only consulted
    /// in `postinstall` mode.
    pub format: Option<String>,

    /// Override the download URL emitted into the postinstall script
    /// (templated). When unset, anodizer derives the URL from the
    /// release context. Only consulted in `postinstall` mode.
    pub url_template: Option<String>,

    /// Additional files to include in the published package alongside the
    /// generated metadata. Default `["README*", "LICENSE*"]` (applied at the
    /// `Default` pass).
    pub extra_files: Option<Vec<String>>,

    /// Template-rendered file mappings (`src` may be a glob; rendered
    /// contents written to `dst`).
    pub templated_extra_files: Option<Vec<NpmTemplatedExtraFile>>,

    /// Free-form root-level `package.json` fields. Shallow-merged into
    /// the generated `package.json` (config keys win over generated ones).
    /// Useful for `mcpName`, `funding`, or other npm metadata fields
    /// anodizer does not surface as first-class options.
    pub extra: Option<HashMap<String, serde_json::Value>>,

    /// Override the registry endpoint (default `https://registry.npmjs.org`).
    pub registry: Option<String>,

    /// Auth token for the registry. Falls back to the `NPM_TOKEN` env var
    /// when unset. Stored in `.npmrc` as `//<registry>/:_authToken=...`
    /// at publish time and never passed via argv.
    pub token: Option<String>,

    /// Skip this publisher. Accepts bool or template string.
    /// Accepts the legacy `disable:` spelling via serde alias for back-compat.
    #[serde(
        default,
        alias = "disable",
        deserialize_with = "deserialize_string_or_bool_opt"
    )]
    pub skip: Option<StringOrBool>,

    /// Override whether this publisher failing should fail the overall release.
    ///
    /// Default: `true` — NPM is a Manager-group publisher (one-way
    /// 72-hour unpublish window), so a failed publish aborts by default
    /// to avoid surprising the operator with a half-released version.
    /// Set to `false` to log failures but continue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,

    /// Template-conditional gate: when the rendered result is falsy
    /// (`"false"` / `"0"` / `"no"` / empty), the NPM publisher entry is
    /// skipped. Render failure hard-errors.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
    /// When `true`, a triggered rollback leaves this publisher's work in
    /// place rather than attempting to undo it. Default `false`.
    pub retain_on_rollback: Option<bool>,
}

/// Default for [`NpmConfig::libc_aware`] — emit musl and glibc linux
/// packages separately.
fn default_libc_aware() -> bool {
    true
}

impl Default for NpmConfig {
    fn default() -> Self {
        Self {
            id: None,
            ids: None,
            mode: NpmMode::default(),
            scope: None,
            metapackage: None,
            bin: None,
            libc_aware: default_libc_aware(),
            name: None,
            description: None,
            homepage: None,
            keywords: None,
            license: None,
            author: None,
            engines: None,
            files: None,
            provenance: None,
            repository: None,
            bugs: None,
            access: None,
            tag: None,
            format: None,
            url_template: None,
            extra_files: None,
            templated_extra_files: None,
            extra: None,
            registry: None,
            token: None,
            skip: None,
            required: None,
            if_condition: None,
            retain_on_rollback: None,
        }
    }
}

/// Template-rendered file mapping for [`NpmConfig::templated_extra_files`].
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NpmTemplatedExtraFile {
    /// Source path (may be a glob; relative to the project root).
    pub src: String,
    /// Destination path inside the published package.
    pub dst: String,
}
