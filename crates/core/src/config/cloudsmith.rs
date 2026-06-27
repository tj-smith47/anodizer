use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{StringOrBool, deserialize_string_or_bool_opt};

// ---------------------------------------------------------------------------
// CloudSmith publisher
// ---------------------------------------------------------------------------

/// Per-format distribution value. Accepts either a single distribution string
/// (`deb: "ubuntu/focal"`) or an array of distribution slugs
/// (`deb: ["ubuntu/focal", "ubuntu/jammy"]`) — the array form causes the
/// publisher to issue one upload per distribution slug.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum CloudSmithDistributions {
    /// Single distribution slug (`"ubuntu/focal"`).
    Single(String),
    /// Multiple distribution slugs; the publisher uploads once per entry.
    Multiple(Vec<String>),
}

impl CloudSmithDistributions {
    /// Materialize as a `Vec<&str>` regardless of which YAML form the user
    /// wrote. A `Single` value yields a one-element vec so the caller can
    /// always iterate.
    pub fn to_str_vec(&self) -> Vec<&str> {
        match self {
            Self::Single(s) => vec![s.as_str()],
            Self::Multiple(v) => v.iter().map(String::as_str).collect(),
        }
    }
}

/// CloudSmith publisher configuration.
/// Pushes packages to CloudSmith repositories.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CloudSmithConfig {
    /// CloudSmith organization slug.
    pub organization: Option<String>,
    /// CloudSmith repository slug.
    pub repository: Option<String>,
    /// Build IDs filter: only publish artifacts from builds whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// Glob patterns matched against each artifact's file name; anodizer drops
    /// any artifact whose name matches at least one glob from THIS CloudSmith
    /// target only. Use it to keep heavy sidecars off a given repository while
    /// packages still upload. Composes with `ids:` and `formats:` (all filters
    /// apply). `None`/empty keeps everything.
    ///
    /// ```yaml
    /// cloudsmiths:
    ///   - organization: my-org
    ///     repository: my-repo
    ///     exclude: ["*.sha256", "*.sig", "*.cdx.json"]
    /// ```
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<Vec<String>>,
    /// Package format filter: only publish artifacts matching these formats.
    pub formats: Option<Vec<String>>,
    /// Distribution mapping per format. Each entry accepts either a single
    /// slug (`deb: "ubuntu/focal"`) or an array of slugs
    /// (`deb: ["ubuntu/focal", "ubuntu/jammy"]`); the array form issues one
    /// upload per entry.
    pub distributions: Option<HashMap<String, CloudSmithDistributions>>,
    /// Debian component name (e.g. "main").
    pub component: Option<String>,
    /// Environment variable name containing the CloudSmith API key.
    pub secret_name: Option<String>,
    /// Template-conditional skip: if rendered result is `"true"`, skip this publisher.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// When true, allow republishing over existing package versions.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub republish: Option<StringOrBool>,
    /// Override whether this publisher failing should fail the overall release.
    ///
    /// Default: `false` — a failure here is logged but does not abort the release.
    /// Set to `true` to fail the release on any error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    /// Template-conditional gate: when the rendered result is falsy
    /// (`"false"` / `"0"` / `"no"` / empty), the CloudSmith publisher is
    /// skipped. Render failure hard-errors. Config key: `cloudsmiths[].if:`.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
    /// When `true`, a triggered rollback leaves this publisher's work in
    /// place rather than attempting to undo it. Default `false`.
    pub retain_on_rollback: Option<bool>,
    /// Retain only the `N` most-recent release versions of each published
    /// package, pruning older ones from the CloudSmith repository after a
    /// successful upload.
    ///
    /// This is **opt-in** and **destructive**: leaving it unset (the default)
    /// prunes nothing. When set, after the just-uploaded artifacts are
    /// confirmed present the publisher lists every version of *this* package
    /// in the repository, ranks the distinct release versions by SemVer
    /// (newest first), keeps the top `N` — which always includes the version
    /// just published — and issues `DELETE` for every artifact (all formats
    /// and architectures) belonging to versions ranked beyond `N`. Other
    /// packages sharing the repository are never touched.
    ///
    /// All package formats of one release are treated as the same version:
    /// the deb/rpm epoch (`1:0.9.1-1`) and apk revision (`0.9.1-r1`) suffixes
    /// are normalized to the base SemVer (`0.9.1`) before ranking, so
    /// keeping `2` versions keeps every `.deb`/`.rpm`/`.apk` of the two newest
    /// releases.
    ///
    /// Pruning is **best-effort**: it runs only after the upload (the real
    /// work) has already succeeded, is skipped entirely in dry-run and
    /// snapshot mode, and a list/delete failure emits a prominent warning and
    /// continues rather than failing the release or rolling anything back.
    /// `keep_versions: 0` is rejected — anodizer never prunes every version.
    ///
    /// Primarily a remedy for storage-capped repositories (e.g. the
    /// CloudSmith free plan's 500 MB limit, which offers no server-side
    /// retention policy).
    ///
    /// ```yaml
    /// cloudsmiths:
    ///   - organization: acme
    ///     repository: tools
    ///     keep_versions: 3   # keep the 3 newest releases, prune older ones
    /// ```
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_versions: Option<u32>,
}
