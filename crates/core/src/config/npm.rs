use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{StringOrBool, deserialize_string_or_bool_opt};

// ---------------------------------------------------------------------------
// NPM registry publisher config
// ---------------------------------------------------------------------------
//
// Mirrors GoReleaser Pro's `npms:` block (closed-source — config surface
// inferred from `https://goreleaser.com/customization/publish/npm/`). One
// entry per published package; each entry generates a `package.json` +
// `postinstall.js` shim, packs a `.tgz`, and runs `npm publish`.

/// NPM package registry publisher configuration.
///
/// Anodizer generates a `package.json` carrying a `postinstall` script
/// that downloads the matching release archive at `npm i` time. Each
/// `npms[]` entry produces one tarball pushed to the configured
/// registry.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NpmConfig {
    /// Unique identifier for selecting this entry from the CLI (`--id=...`).
    pub id: Option<String>,

    /// Build IDs filter: only include artifacts whose archive `id` is in this list.
    pub ids: Option<Vec<String>>,

    /// NPM package name (required). May be scoped (`@org/foo`) or unscoped (`foo`).
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

    /// Templated `author` field for `package.json`. Falls back to
    /// `metadata.maintainers[0]` when unset.
    pub author: Option<String>,

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
    /// (`tgz`, `tar.gz`, `zip`, `binary`). Default `tgz`.
    pub format: Option<String>,

    /// Override the download URL emitted into the postinstall script
    /// (templated). When unset, anodizer derives the URL from the
    /// release context.
    pub url_template: Option<String>,

    /// Additional files to include in the tarball alongside
    /// `package.json` + the postinstall script. Default
    /// `["README*", "LICENSE*"]` (applied at `Default` pass).
    pub extra_files: Option<Vec<String>>,

    /// Template-rendered file mappings (`src` may be a glob; rendered
    /// contents written to `dst`).
    pub templated_extra_files: Option<Vec<NpmTemplatedExtraFile>>,

    /// Free-form root-level `package.json` fields. Shallow-merged into
    /// the generated `package.json`. Useful for `engines`, `mcpName`,
    /// or other npm metadata fields anodizer does not surface.
    pub extra: Option<HashMap<String, serde_json::Value>>,

    /// Override the registry endpoint (default `https://registry.npmjs.org`).
    pub registry: Option<String>,

    /// Auth token for the registry. Falls back to the `NPM_TOKEN` env var
    /// when unset. Stored in `.npmrc` as `//<registry>/:_authToken=...`
    /// at publish time and never passed via argv.
    pub token: Option<String>,

    /// Skip this publisher. Accepts bool or template string.
    #[serde(default, deserialize_with = "deserialize_string_or_bool_opt")]
    pub skip: Option<StringOrBool>,

    /// Disable this publisher entry. Mirrors GoReleaser Pro
    /// `npms[].disable:`. Accepts bool or template string.
    #[serde(default, deserialize_with = "deserialize_string_or_bool_opt")]
    pub disable: Option<StringOrBool>,

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
    /// skipped. Render failure hard-errors. Mirrors GoReleaser Pro
    /// `npms[].if:`.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
}

/// Template-rendered file mapping for [`NpmConfig::templated_extra_files`].
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NpmTemplatedExtraFile {
    /// Source path (may be a glob; relative to the project root).
    pub src: String,
    /// Destination path inside the published tarball.
    pub dst: String,
}
