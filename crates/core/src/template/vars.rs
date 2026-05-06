use regex::Regex;
use std::collections::HashMap;
use std::sync::LazyLock;
use tera::Value;

#[derive(Clone)]
pub struct TemplateVars {
    pub(super) vars: HashMap<String, String>,
    pub(super) env: HashMap<String, String>,
    /// Env vars explicitly configured by the user (config `env:`, `.env` files,
    /// workspace `env:`).  These are safe to serialize into split contexts and
    /// inject into subprocess commands.  Process-inherited env vars (HOME, PATH,
    /// USER, etc.) live only in `env` for template rendering — they must NOT be
    /// forwarded to subprocesses (which inherit them naturally) or serialized
    /// across platforms (macOS HOME poisons Linux builds).
    pub(super) config_env: HashMap<String, String>,
    /// Custom user-defined variables accessible as {{ .Var.key }}.
    pub(super) custom_vars: HashMap<String, String>,
    /// Pipeline outputs map accessible as {{ .Outputs.key }}.
    /// Stages can populate this and templates can read it.
    /// Similar to `.Var.*` but for pipeline outputs rather than user config.
    /// Concrete stage->key mappings will be added as stages are enhanced
    /// (e.g. build_id, checksum, etc.).
    pub(super) outputs: HashMap<String, String>,
    /// Structured values (arrays, objects) inserted into the Tera context as-is.
    /// Used for complex template variables like `Artifacts` (list of maps) and
    /// `Metadata` (nested map) that cannot be represented as flat strings.
    pub(super) structured: HashMap<String, Value>,
}

impl TemplateVars {
    pub fn new() -> Self {
        Self {
            vars: HashMap::new(),
            env: HashMap::new(),
            config_env: HashMap::new(),
            custom_vars: HashMap::new(),
            outputs: HashMap::new(),
            structured: HashMap::new(),
        }
    }

    pub fn set(&mut self, key: &str, value: &str) {
        self.vars.insert(key.to_string(), value.to_string());
    }

    pub fn get(&self, key: &str) -> Option<&String> {
        self.vars.get(key)
    }

    pub fn set_env(&mut self, key: &str, value: &str) {
        self.env.insert(key.to_string(), value.to_string());
    }

    /// Set an env var that was explicitly configured by the user.
    /// Also adds it to the general env map for template rendering.
    pub fn set_config_env(&mut self, key: &str, value: &str) {
        self.env.insert(key.to_string(), value.to_string());
        self.config_env.insert(key.to_string(), value.to_string());
    }

    pub fn set_custom_var(&mut self, key: &str, value: &str) {
        self.custom_vars.insert(key.to_string(), value.to_string());
    }

    /// Set a pipeline output value accessible as `{{ .Outputs.key }}`.
    ///
    /// Infrastructure: no stage populates Outputs yet. Concrete key mappings
    /// will be added as individual stages are enhanced (e.g. build -> build_id).
    pub fn set_output(&mut self, key: &str, value: &str) {
        self.outputs.insert(key.to_string(), value.to_string());
    }

    /// Get a pipeline output value by key.
    pub fn get_output(&self, key: &str) -> Option<&String> {
        self.outputs.get(key)
    }

    /// Set a structured (non-string) value accessible directly in Tera context.
    /// Used for complex types like arrays of maps (`Artifacts`) or nested maps
    /// (`Metadata`) that cannot be represented as flat key=value strings.
    pub fn set_structured(&mut self, key: &str, value: Value) {
        self.structured.insert(key.to_string(), value);
    }

    /// Return all template variables (excluding env and custom vars).
    pub fn all(&self) -> &HashMap<String, String> {
        &self.vars
    }

    /// Return all environment variables (process + config).
    /// Used for template rendering ({{ .Env.* }}).
    pub fn all_env(&self) -> &HashMap<String, String> {
        &self.env
    }

    /// Return only explicitly configured env vars (config `env:`, `.env` files).
    /// Safe to serialize into split contexts and inject into subprocesses.
    /// Process-inherited vars (HOME, PATH, etc.) are excluded — subprocesses
    /// inherit them naturally, and serializing them across platforms is poison
    /// (macOS HOME=/Users/runner breaks Linux docker builds).
    pub fn all_config_env(&self) -> &HashMap<String, String> {
        &self.config_env
    }

    /// Get a structured (non-string) template variable by key.
    /// Returns `None` if the key does not exist in the structured map.
    pub fn get_structured(&self, key: &str) -> Option<&tera::Value> {
        self.structured.get(key)
    }

    /// Return all structured template variables.
    pub fn all_structured(&self) -> &HashMap<String, Value> {
        &self.structured
    }
}

impl Default for TemplateVars {
    fn default() -> Self {
        Self::new()
    }
}

/// Clear per-target template variables (`Os`, `Arch`, `Target`, `Arm`,
/// `Arm64`, `Amd64`, `Mips`, `I386`) so they don't leak to downstream
/// stages after a packaging stage's per-target loop finishes.
///
/// Packaging stages (flatpak, snapcraft, nfpm, makeself, etc.) iterate
/// over (config × target) tuples and set these vars once per iteration so
/// user templates like `{{ .ProjectName }}_{{ .Version }}_{{ .Os }}_{{ .Arch }}`
/// render correctly. Leaving a stale `Os=linux` value set when a later
/// stage (announce, publish) renders its own templates causes subtle
/// cross-stage leaks — the announcement for a multi-platform release gets
/// tagged with whichever platform finished last.
pub fn clear_per_target_vars(tv: &mut TemplateVars) {
    for key in PER_TARGET_VARS {
        tv.set(key, "");
    }
}

/// The template-variable keys that per-target packaging loops populate
/// and must clear on exit.
pub const PER_TARGET_VARS: &[&str] = &[
    "Os", "Arch", "Target", "Arm", "Arm64", "Amd64", "Mips", "I386",
];

/// Per-artifact template variable keys (set inside per-artifact loops in
/// stage-sbom, stage-sign, stage-checksum). Bundled into a constant so the
/// "set, render, clear" pattern stays in one place — when an additional var
/// gets added (e.g. `ArtifactPath`), every consumer picks it up.
pub const PER_ARTIFACT_VARS: &[&str] = &["ArtifactName", "ArtifactExt", "ArtifactID"];

/// Clear both `PER_TARGET_VARS` and `PER_ARTIFACT_VARS` on exit from a
/// per-artifact loop. Mirrors `clear_per_target_vars` but covers the larger
/// surface that sbom/sign/checksum loops touch — preventing the "stale
/// ArtifactName from sbom run leaking into announce" class of bug.
pub fn clear_per_artifact_vars(tv: &mut TemplateVars) {
    clear_per_target_vars(tv);
    for key in PER_ARTIFACT_VARS {
        tv.set(key, "");
    }
}

/// Known numeric template fields that should be inserted as integers into the
/// Tera context so that numeric comparisons like `{% if Major == 1 %}` work
/// correctly. Without this, they would be strings and `"1" != 1`.
pub(super) const NUMERIC_FIELDS: &[&str] =
    &["Major", "Minor", "Patch", "Timestamp", "CommitTimestamp"];

/// Regex matching `Env.VARNAME` references in a preprocessed template.
/// Used to discover env var keys referenced by the template so they can be
/// pre-populated with empty strings (GoReleaser returns "" for missing env vars).
pub(super) static ENV_REF_RE: LazyLock<Regex> =
    LazyLock::new(|| crate::util::static_regex(r"Env\.([A-Za-z_][A-Za-z0-9_]*)"));
