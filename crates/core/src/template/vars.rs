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
        // A key must never live in both the string and structured maps: the
        // structured map wins at Tera-context build time, so a stale
        // structured entry would silently shadow this write (e.g. a test
        // overriding `IsSnapshot` on a fully-constructed `Context`).
        self.structured.remove(key);
        self.vars.insert(key.to_string(), value.to_string());
    }

    /// Set a boolean template variable as a real `tera::Value::Bool` so that
    /// `{% if Var %}` / `not Var` / `and` / `or` evaluate it as a bool while
    /// `{{ Var }}` interpolation still renders `"true"` / `"false"`.
    pub fn set_bool(&mut self, key: &str, value: bool) {
        self.set_structured(key, Value::Bool(value));
    }

    /// Remove a regular template variable. Returns `true` if the key was
    /// present. Use when a value is logically *undefined* for downstream
    /// renders — distinct from `set(key, "")` which keeps the key with an
    /// empty string. Strict-mode template rendering can distinguish defined-
    /// empty from undefined; the latter is the correct shape for per-config
    /// vars (e.g. `BaseImage`) that should not bleed across iterations.
    pub fn unset(&mut self, key: &str) -> bool {
        self.vars.remove(key).is_some()
    }

    /// Remove a structured (non-string) template variable. Mirrors `unset`
    /// for the structured map. Returns `true` if the key was present.
    pub fn unset_structured(&mut self, key: &str) -> bool {
        self.structured.remove(key).is_some()
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
        // Mirror of `set`: evict any string-map entry so a key can never
        // resolve differently depending on which map a reader consults.
        self.vars.remove(key);
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

/// Clear per-target template variables (`Os`, `Arch`, `Target`, `Libc`,
/// `Arm`, `Arm64`, `Amd64`, `Mips`, `I386`) so they don't leak to downstream
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
///
/// The per-artifact template key set
/// (`KeyOS`, `KeyArch`, `KeyAmd64`, `Key386`, `KeyArm`, `KeyArm64`, `KeyMips`,
/// `KeyPpc64`, `KeyRiscv64` plus `target`). Keeping the set in sync keeps
/// templates that branch on `{{ .Ppc64 }}` / `{{ .Riscv64 }}` from raising
/// a Tera "missing key" error in strict-mode rendering.
pub const PER_TARGET_VARS: &[&str] = &[
    "Os", "Arch", "Target", "Libc", "Arm", "Arm64", "Amd64", "Mips", "I386", "Ppc64", "Riscv64",
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

/// Template variables anodizer injects as real `tera::Value::Bool` values.
/// `{% if Var %}` / `not Var` evaluate them as bools; `{{ Var }}` renders
/// `"true"` / `"false"`. Comparing one to a quoted string (`Var == "false"`)
/// never matches — Tera does not coerce `Bool` ↔ `str` — so such compares
/// are rejected by [`find_stale_typed_compare`].
pub const BOOL_FIELDS: &[&str] = &[
    "IsSnapshot",
    "IsNightly",
    "IsHarness",
    "IsDraft",
    "IsRelease",
    "IsSingleTarget",
    "IsMerging",
    "IsGitDirty",
    "IsGitClean",
    "IsPrepare",
];

/// Typed (non-string) injected fields beyond the bools: `NightlyBuild` is a
/// `tera::Value::Number`, so string compares against it never match either.
const TYPED_NON_STRING_FIELDS: &[&str] = &["NightlyBuild"];

/// Regex matching a quoted-string comparison against one of the typed
/// (bool / number) injected fields, in either Tera infix form
/// (`IsSnapshot == "false"`, `"true" != .IsHarness`) or Go template form
/// (`eq .IsSnapshot "false"`, `ne "true" .IsNightly`). Built lazily from
/// `BOOL_FIELDS` + `TYPED_NON_STRING_FIELDS` so the lint can never drift
/// from the injection list.
static STALE_TYPED_COMPARE_RE: LazyLock<Regex> = LazyLock::new(|| {
    let names = BOOL_FIELDS
        .iter()
        .chain(TYPED_NON_STRING_FIELDS)
        .copied()
        .collect::<Vec<_>>()
        .join("|");
    let var = format!(r"\.?(?:{names})\b");
    let quoted = r#"(?:"[^"]*"|'[^']*')"#;
    // The leading `(?:^|[^\w.])` boundary keeps namespaced user vars that
    // merely share a suffix (`Var.IsSnapshot`) from matching; the offending
    // snippet itself is capture group 1.
    crate::util::static_regex(&format!(
        r"(?:^|[^\w.])({var}\s*(?:==|!=)\s*{quoted}|{quoted}\s*(?:==|!=)\s*{var}|(?:eq|ne)\s+(?:{var}\s+{quoted}|{quoted}\s+{var}))"
    ))
});

/// Detect a quoted-string comparison against a typed (bool / number)
/// injected template variable, returning the offending snippet.
///
/// `IsSnapshot` and friends are real bools in the Tera context, so
/// `IsSnapshot == "false"` evaluates to `false` in *every* mode and the
/// guarded stage silently skips. Configs migrated from the era when these
/// were strings (or written against Go template semantics) carry exactly
/// this pattern; rejecting it loudly at evaluation time converts a silent
/// mis-skip into an actionable error. Write `IsSnapshot` / `not IsSnapshot`
/// (or a numeric compare for `NightlyBuild`) instead.
pub fn find_stale_typed_compare(template: &str) -> Option<&str> {
    STALE_TYPED_COMPARE_RE
        .captures(template)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str())
}

/// Regex matching `Env.VARNAME` references in a preprocessed template.
/// Used to discover env var keys referenced by the template so they can be
/// pre-populated with empty strings (missing env vars resolve to "").
pub(super) static ENV_REF_RE: LazyLock<Regex> =
    LazyLock::new(|| crate::util::static_regex(r"Env\.([A-Za-z_][A-Za-z0-9_]*)"));
