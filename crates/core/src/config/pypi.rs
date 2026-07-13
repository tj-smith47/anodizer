use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{Amd64Variant, StringOrBool, deserialize_string_or_bool_opt};

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

    /// Target-triple allowlist: restrict the wheels to a subset of the built
    /// targets. When unset (the default), every built target becomes a wheel.
    /// When set, only binaries whose target triple appears in this list are
    /// built into wheels; the rest are silently skipped. Orthogonal to `ids:`:
    /// both filters apply (a binary must pass the `ids` filter AND, when this
    /// is set, be listed here). A listed triple that no selected build
    /// produces is a config error, and an explicit empty list (`targets: []`)
    /// is rejected — omit the field to publish every built target. A common use
    /// is excluding `x86_64-pc-windows-gnu` so it does not collide with the
    /// `x86_64-pc-windows-msvc` wheel on the shared `win_amd64` platform tag.
    /// Example: `targets: [x86_64-unknown-linux-gnu, x86_64-pc-windows-msvc]`.
    pub targets: Option<Vec<String>>,

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

    /// Templated twine upload endpoint URL. Default
    /// `https://upload.pypi.org/legacy/` (the production PyPI upload API).
    /// Point it at TestPyPI to rehearse a release:
    ///
    /// ```yaml
    /// pypis:
    ///   - index_url: "https://test.pypi.org/legacy/"
    /// ```
    ///
    /// This is the twine *upload* target, not a `{owner, name}` source
    /// repository — the name `index_url` keeps it distinct from the reserved
    /// `repository` meaning every git-based publisher uses. The legacy
    /// `repository:` spelling is still accepted via serde alias.
    #[serde(alias = "repository")]
    pub index_url: Option<String>,

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

    /// `Description-Content-Type` for the long-description body — how PyPI
    /// renders it (`text/markdown`, `text/x-rst`, `text/plain`). When a
    /// `description` is present and this is unset, defaults to
    /// `text/markdown` (the modern norm); without the header PyPI renders the
    /// body as raw plaintext. Omitted entirely when there is no description.
    pub description_content_type: Option<String>,

    /// Package author, emitted as the METADATA `Author` header.
    pub author: Option<String>,

    /// Package author email, emitted as the METADATA `Author-email` header.
    pub author_email: Option<String>,

    /// Arbitrary `Project-URL` label → URL map, one
    /// `Project-URL: <label>, <url>` METADATA header each (the PyPI sidebar
    /// links). Emitted in addition to the `Homepage` link derived from
    /// `homepage`; use this for `Repository`, `Documentation`, `Changelog`,
    /// `Funding`, etc. Rendered in sorted label order for a byte-stable wheel.
    ///
    /// ```yaml
    /// pypis:
    ///   - project_urls:
    ///       Repository: "https://github.com/me/my-tool"
    ///       Documentation: "https://docs.example.com"
    /// ```
    pub project_urls: Option<BTreeMap<String, String>>,

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

    /// Per-target-triple wheel platform-tag overrides: `<target triple>` →
    /// explicit wheel platform tag. When a built target has an entry, its tag
    /// is used *verbatim* — binary inspection (the glibc floor for
    /// `manylinux`, the Mach-O deployment target for `macosx`) is skipped for
    /// that target. Every target without an entry keeps the auto-detected tag.
    ///
    /// The escape hatch for toolchains whose emitted glibc floor is stricter
    /// than the compatibility a project wants to advertise — e.g. pinning
    /// `aarch64-unknown-linux-gnu` to `manylinux_2_28` to match a
    /// `maturin`/PyO3 build environment rather than shipping the higher floor
    /// the binary's symbols imply.
    ///
    /// ```yaml
    /// pypis:
    ///   - platform_tag_overrides:
    ///       aarch64-unknown-linux-gnu: manylinux_2_28_aarch64
    /// ```
    pub platform_tag_overrides: Option<BTreeMap<String, String>>,

    /// `x86_64` micro-architecture variant selector — `v1` (baseline), `v2`,
    /// `v3` (AVX2), or `v4`. When set, an amd64 binary carrying
    /// `amd64_variant` metadata becomes the `win_amd64`/`manylinux…x86_64`
    /// wheel only when its variant matches; a binary with no variant metadata
    /// still matches (the baseline build). Default: `v1`. Typed as
    /// [`Amd64Variant`], so any value outside `v1`..`v4` is rejected at parse
    /// time.
    pub amd64_variant: Option<Amd64Variant>,

    /// ARM version selector (e.g. `"6"`, `"7"`). When set, a 32-bit ARM binary
    /// carrying `arm_variant` metadata becomes the wheel only when its variant
    /// matches; a binary with no variant metadata still matches. Does not
    /// affect `aarch64`/`arm64` (64-bit ARM has no sub-variant).
    pub arm_variant: Option<String>,

    /// Whether the upload authenticates with a long-lived API token or with
    /// GitHub Actions OIDC (PyPI Trusted Publishing). Default [`Auto`]:
    /// a token when one is available, otherwise a Trusted-Publishing exchange
    /// when an OIDC context is present.
    ///
    /// [`Auto`]: PypiAuthMode::Auto
    pub auth: PypiAuthMode,

    /// API token for the upload (templated). Falls back to the `PYPI_TOKEN`
    /// env var, then `MATURIN_PYPI_TOKEN`, when unset. Sent as HTTP Basic
    /// auth with the literal username `__token__` and NEVER logged. Unused
    /// when `auth: oidc` (Trusted Publishing mints its own short-lived token).
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
            targets: None,
            name: None,
            sdist: false,
            sdist_manifest: None,
            index_url: None,
            skip_existing: true,
            requires_python: None,
            summary: None,
            description: None,
            description_content_type: None,
            author: None,
            author_email: None,
            project_urls: None,
            homepage: None,
            license: None,
            keywords: None,
            classifiers: None,
            platform_tag_overrides: None,
            amd64_variant: None,
            arm_variant: None,
            auth: PypiAuthMode::default(),
            token: None,
            skip: None,
            required: None,
            if_condition: None,
            retain_on_rollback: None,
        }
    }
}

/// How a `pypis[]` entry authenticates its upload: a long-lived API token, or
/// GitHub Actions OIDC (PyPI Trusted Publishing, which mints a short-lived
/// upload token per run — no stored secret).
///
/// Unlike npm, PyPI is uploaded directly over HTTP rather than through a CLI,
/// so the Trusted-Publishing exchange (Actions id-token → PyPI mint-token) is
/// performed by anodizer itself. Trusted Publishing also creates brand-new
/// projects when a *pending* publisher is configured on PyPI, so there is no
/// per-package "must already exist" caveat as there is for npm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum PypiAuthMode {
    /// Use a token when one is available (`cfg.token` / `PYPI_TOKEN` /
    /// `MATURIN_PYPI_TOKEN`); otherwise, when an OIDC context is present, mint
    /// a Trusted-Publishing token. Errors only when neither is available.
    #[default]
    Auto,
    /// Always authenticate with the token; never attempt OIDC. Errors if no
    /// token is available. This is anodizer's historical behaviour.
    Token,
    /// Always authenticate with OIDC (Trusted Publishing); never fall back to
    /// the token. Errors if the GitHub Actions OIDC request env
    /// (`ACTIONS_ID_TOKEN_REQUEST_URL` / `_TOKEN`) is absent, so a misconfigured
    /// Trusted Publisher fails the release loudly instead of silently falling
    /// back to a token.
    Oidc,
}
