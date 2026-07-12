use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{StringOrBool, deserialize_string_or_bool_opt};

/// PyPI publisher configuration.
///
/// Publishes the project's prebuilt binaries as native Python wheels — one
/// `py3-none-<platform>` wheel per built target, with the platform tag
/// derived by inspecting each binary (glibc floor for `manylinux`, Mach-O
/// deployment target for `macosx`) — and uploads them via PyPI's legacy
/// (twine-protocol) upload API. Optionally also builds and uploads a source
/// distribution via `maturin sdist`. Each `pypis[]` entry produces one
/// publish.
///
/// ```yaml
/// pypis:
///   - name: my-tool
///     requires_python: ">=3.7"
///     sdist: true
///     sdist_manifest: "pypi/"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PypiConfig {
    /// Unique identifier for selecting this entry from the CLI (`--id=...`).
    pub id: Option<String>,

    /// Build IDs filter: only include binaries whose crate is in this list.
    pub ids: Option<Vec<String>>,

    /// PyPI project name. May use any PEP 508 name form (`My.Tool`,
    /// `my_tool`); PyPI normalizes it per PEP 503 for index lookups and the
    /// wheel filename escapes it per PEP 427. Falls back to the crate name
    /// when unset.
    pub name: Option<String>,

    /// Also build and upload a source distribution via `maturin sdist`.
    /// Default `false`. Requires `sdist_manifest` to point at the directory
    /// containing the project's `pyproject.toml`, and `maturin` on `PATH`.
    ///
    /// ```yaml
    /// pypis:
    ///   - sdist: true
    ///     sdist_manifest: "pypi/"
    /// ```
    pub sdist: bool,

    /// Templated directory containing the `pyproject.toml` that `maturin
    /// sdist` builds from, relative to the project root (e.g. `"pypi/"`).
    /// Required when `sdist: true`; unused otherwise.
    pub sdist_manifest: Option<String>,

    /// Templated upload endpoint URL. Default
    /// `https://upload.pypi.org/legacy/` (the production PyPI upload API).
    /// Point it at TestPyPI to rehearse a release:
    ///
    /// ```yaml
    /// pypis:
    ///   - repository: "https://test.pypi.org/legacy/"
    /// ```
    pub repository: Option<String>,

    /// Tolerate the index rejecting a file that already exists (the
    /// twine `--skip-existing` semantics). Default `true` so a re-run of an
    /// already-published tag skips previously-uploaded files instead of
    /// failing the release. Set to `false` to make a duplicate upload a hard
    /// error.
    pub skip_existing: bool,

    /// `Requires-Python` version specifier written into each wheel's
    /// METADATA (e.g. `">=3.7"`). Purely declarative for a binary wheel —
    /// the shipped executable does not import Python — but pip honors it
    /// during resolution. Omitted when unset.
    pub requires_python: Option<String>,

    /// Templated one-line `Summary` for the package METADATA. Falls back to
    /// the project-level `metadata.description` (and then the crate's
    /// `Cargo.toml [package].description`) when unset.
    pub summary: Option<String>,

    /// Templated long description written as the METADATA body (rendered on
    /// the PyPI project page). Falls back to the summary when unset.
    pub description: Option<String>,

    /// Templated homepage URL, emitted as `Project-URL: Homepage`. Falls
    /// back to `metadata.homepage` (then `Cargo.toml [package].homepage`)
    /// when unset.
    pub homepage: Option<String>,

    /// Templated license expression (e.g. `MIT`, `Apache-2.0`), emitted as
    /// the METADATA `License` field. Falls back to `metadata.license` (then
    /// `Cargo.toml [package].license`) when unset.
    pub license: Option<String>,

    /// Keywords list, emitted comma-separated in METADATA.
    pub keywords: Option<Vec<String>>,

    /// Trove classifier lines (e.g.
    /// `"Programming Language :: Rust"`), one `Classifier:` METADATA header
    /// each.
    pub classifiers: Option<Vec<String>>,

    /// API token for the upload (templated). Falls back to the `PYPI_TOKEN`
    /// env var, then `MATURIN_PYPI_TOKEN`, when unset. Sent as HTTP Basic
    /// auth with the literal username `__token__` and NEVER logged.
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
    /// Default: `true` — PyPI is a Manager-group publisher whose uploads are
    /// one-way (a published filename can never be re-uploaded, even after
    /// deletion), so a failed publish aborts by default to avoid surprising
    /// the operator with a half-released version. Set to `false` to log
    /// failures but continue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,

    /// Template-conditional gate: when the rendered result is falsy
    /// (`"false"` / `"0"` / `"no"` / empty), the PyPI publisher entry is
    /// skipped. Render failure hard-errors.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,

    /// When `true`, a triggered rollback leaves this publisher's work in
    /// place rather than attempting to undo it. Default `false`. (PyPI has
    /// no programmatic delete path anyway — rollback is warn-only — but the
    /// flag suppresses even that warning.)
    pub retain_on_rollback: Option<bool>,
}

impl Default for PypiConfig {
    fn default() -> Self {
        Self {
            id: None,
            ids: None,
            name: None,
            sdist: false,
            sdist_manifest: None,
            repository: None,
            skip_existing: true,
            requires_python: None,
            summary: None,
            description: None,
            homepage: None,
            license: None,
            keywords: None,
            classifiers: None,
            token: None,
            skip: None,
            required: None,
            if_condition: None,
            retain_on_rollback: None,
        }
    }
}
