//! Config-derived environment preflight: requirement vocabulary + check engine.
//!
//! Before any release stage runs, every enabled publisher and stage declares
//! the environment it needs — CLI tools on PATH, env vars/secrets, endpoint
//! reachability, loadable key material — derived from the **same resolved
//! config its run path reads** (publishers via
//! [`crate::Publisher::requirements`], stages via their crate's
//! `env_requirements` function). The engine checks every requirement in one
//! pass and reports **all** failures together, so one run shows the complete
//! environment state instead of failing on the first missing secret.
//!
//! Failure messages never include env-var *values* — only names, tool names,
//! configured URLs, and structural descriptions of key material.

use serde::Serialize;

// ---------------------------------------------------------------------------
// Requirement vocabulary
// ---------------------------------------------------------------------------

/// Key-material families the preflight can structurally validate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyKind {
    /// OpenSSH / PEM private key (AUR pushes over ssh).
    SshPrivate,
    /// PGP private key — ASCII-armored block or binary keyring packet
    /// (gpg signing, nfpm deb/rpm signatures).
    PgpPrivate,
    /// Cosign / sigstore private key (`COSIGN_KEY`, `env://` refs).
    Cosign,
}

impl KeyKind {
    /// Human label used in failure messages.
    pub fn label(self) -> &'static str {
        match self {
            KeyKind::SshPrivate => "SSH private key",
            KeyKind::PgpPrivate => "PGP private key",
            KeyKind::Cosign => "cosign private key",
        }
    }
}

/// One environment prerequisite a publisher or stage derives from its
/// resolved config.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EnvRequirement {
    /// A CLI tool that must be resolvable on PATH.
    Tool { name: String },
    /// At least one of the listed CLI tools must be resolvable on PATH
    /// (stages with a detection ladder, e.g. dmg's
    /// hdiutil → genisoimage → mkisofs).
    ToolAnyOf { names: Vec<String> },
    /// Every listed env var must be present and non-empty.
    EnvAllOf { vars: Vec<String> },
    /// At least one of the listed env vars must be present and non-empty.
    EnvAnyOf { vars: Vec<String> },
    /// An HTTP(S) endpoint from config that must be reachable.
    Endpoint { url: String },
    /// A reachable docker daemon (`docker info` must succeed).
    DockerDaemon,
    /// An env var that must hold parseable key material of `kind`.
    KeyEnv { kind: KeyKind, var: String },
    /// A file path (already template-rendered) that must hold parseable
    /// key material of `kind`.
    KeyFile { kind: KeyKind, path: String },
}

/// A requirement tagged with the publisher/stage that declared it.
#[derive(Debug, Clone)]
pub struct SourcedRequirement {
    /// Declaring surface, e.g. `"publish:aur"` or `"stage:nfpm"`.
    pub source: String,
    pub requirement: EnvRequirement,
}

impl SourcedRequirement {
    pub fn new(source: impl Into<String>, requirement: EnvRequirement) -> Self {
        Self {
            source: source.into(),
            requirement,
        }
    }
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

/// Classification of a preflight failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    MissingTool,
    MissingEnv,
    EndpointUnreachable,
    DockerUnavailable,
    BadKeyMaterial,
}

/// One failed check, with every publisher/stage that needs it.
#[derive(Debug, Clone, Serialize)]
pub struct EnvCheckFailure {
    pub kind: FailureKind,
    /// Human-readable description. Never contains secret values.
    pub message: String,
    /// Sources (publishers/stages) whose declared requirement failed.
    pub needed_by: Vec<String>,
}

/// Aggregate result of one preflight pass.
#[derive(Debug, Clone, Serialize)]
pub struct EnvPreflightReport {
    /// Distinct checks evaluated (after de-duplication across sources).
    pub checks: usize,
    pub failures: Vec<EnvCheckFailure>,
}

impl EnvPreflightReport {
    pub fn ok(&self) -> bool {
        self.failures.is_empty()
    }
}

impl std::fmt::Display for EnvPreflightReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.failures.is_empty() {
            return write!(f, "preflight: {} check(s) passed", self.checks);
        }
        writeln!(
            f,
            "preflight: {} of {} check(s) failed:",
            self.failures.len(),
            self.checks
        )?;
        for failure in &self.failures {
            writeln!(
                f,
                "  ✗ {} [needed by: {}]",
                failure.message,
                failure.needed_by.join(", ")
            )?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Probes — injectable so the engine stays pure and unit-testable
// ---------------------------------------------------------------------------

/// Side-effecting probes injected into [`evaluate`]. Production callers wire
/// these to [`crate::tool_detect`] and [`crate::http`]; tests inject
/// closures.
pub struct EnvProbes<'a> {
    /// `true` when the tool resolves on PATH.
    pub tool: &'a dyn Fn(&str) -> bool,
    /// `Ok(())` when the endpoint answers HTTP; `Err(reason)` otherwise.
    pub endpoint: &'a dyn Fn(&str) -> Result<(), String>,
    /// `true` when a docker daemon answers `docker info`.
    pub docker: &'a dyn Fn() -> bool,
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

/// Evaluate every requirement, de-duplicating identical requirements across
/// sources, and return a collect-all report.
///
/// `env` resolves an env-var name to its value (callers typically merge the
/// template `Env` map — which includes `env_files` entries — with the
/// process environment). Values are only tested for presence/shape; they
/// never appear in the report.
pub fn evaluate(
    requirements: &[SourcedRequirement],
    env: &dyn Fn(&str) -> Option<String>,
    probes: &EnvProbes<'_>,
) -> EnvPreflightReport {
    // De-duplicate while preserving first-seen order; N is small enough
    // that linear scan beats pulling in an ordered-map dependency.
    let mut unique: Vec<(EnvRequirement, Vec<String>)> = Vec::new();
    for sr in requirements {
        match unique.iter_mut().find(|(r, _)| *r == sr.requirement) {
            Some((_, sources)) => {
                if !sources.contains(&sr.source) {
                    sources.push(sr.source.clone());
                }
            }
            None => unique.push((sr.requirement.clone(), vec![sr.source.clone()])),
        }
    }

    let mut failures = Vec::new();
    let checks = unique.len();
    for (req, needed_by) in unique {
        if let Some((kind, message)) = check_one(&req, env, probes) {
            failures.push(EnvCheckFailure {
                kind,
                message,
                needed_by,
            });
        }
    }
    EnvPreflightReport { checks, failures }
}

fn present(env: &dyn Fn(&str) -> Option<String>, var: &str) -> bool {
    env(var).is_some_and(|v| !v.is_empty())
}

fn check_one(
    req: &EnvRequirement,
    env: &dyn Fn(&str) -> Option<String>,
    probes: &EnvProbes<'_>,
) -> Option<(FailureKind, String)> {
    match req {
        EnvRequirement::Tool { name } => (!(probes.tool)(name)).then(|| {
            (
                FailureKind::MissingTool,
                format!("required tool '{name}' not found on PATH"),
            )
        }),
        EnvRequirement::ToolAnyOf { names } => {
            let any = names.iter().any(|n| (probes.tool)(n));
            (!any).then(|| {
                (
                    FailureKind::MissingTool,
                    format!("none of the tool(s) [{}] found on PATH", names.join(", ")),
                )
            })
        }
        EnvRequirement::EnvAllOf { vars } => {
            let missing: Vec<&str> = vars
                .iter()
                .filter(|v| !present(env, v))
                .map(String::as_str)
                .collect();
            (!missing.is_empty()).then(|| {
                (
                    FailureKind::MissingEnv,
                    format!("env var(s) missing or empty: {}", missing.join(", ")),
                )
            })
        }
        EnvRequirement::EnvAnyOf { vars } => {
            let any = vars.iter().any(|v| present(env, v));
            (!any).then(|| {
                (
                    FailureKind::MissingEnv,
                    format!(
                        "none of the env var(s) [{}] is set and non-empty",
                        vars.join(", ")
                    ),
                )
            })
        }
        EnvRequirement::Endpoint { url } => match (probes.endpoint)(url) {
            Ok(()) => None,
            Err(reason) => Some((
                FailureKind::EndpointUnreachable,
                format!("endpoint '{url}' unreachable: {reason}"),
            )),
        },
        EnvRequirement::DockerDaemon => (!(probes.docker)()).then(|| {
            (
                FailureKind::DockerUnavailable,
                "docker daemon unreachable ('docker info' failed)".to_string(),
            )
        }),
        EnvRequirement::KeyEnv { kind, var } => match env(var).filter(|v| !v.is_empty()) {
            None => Some((
                FailureKind::MissingEnv,
                format!("env var(s) missing or empty: {var}"),
            )),
            Some(value) => validate_key_material(*kind, &value).err().map(|reason| {
                (
                    FailureKind::BadKeyMaterial,
                    format!(
                        "env var {var} does not hold a usable {}: {reason}",
                        kind.label()
                    ),
                )
            }),
        },
        EnvRequirement::KeyFile { kind, path } => match std::fs::read(path) {
            Err(e) => Some((
                FailureKind::BadKeyMaterial,
                format!(
                    "key file '{path}' not readable ({}): {}",
                    kind.label(),
                    e.kind()
                ),
            )),
            Ok(bytes) => {
                // Binary (non-UTF-8) content is accepted for PGP keyring
                // exports; the armored validators only apply to text.
                match String::from_utf8(bytes) {
                    Err(_) if *kind == KeyKind::PgpPrivate => None,
                    Err(_) => Some((
                        FailureKind::BadKeyMaterial,
                        format!(
                            "key file '{path}' is not valid UTF-8 (expected {})",
                            kind.label()
                        ),
                    )),
                    Ok(text) => validate_key_material(*kind, &text).err().map(|reason| {
                        (
                            FailureKind::BadKeyMaterial,
                            format!(
                                "key file '{path}' does not hold a usable {}: {reason}",
                                kind.label()
                            ),
                        )
                    }),
                }
            }
        },
    }
}

// ---------------------------------------------------------------------------
// Key-material validation (structural parse — never echoes content)
// ---------------------------------------------------------------------------

/// Structurally validate key material. Returns `Err(reason)` with a
/// description that never includes the key content.
pub fn validate_key_material(kind: KeyKind, content: &str) -> Result<(), String> {
    match kind {
        KeyKind::SshPrivate => validate_ssh_private_key(content),
        KeyKind::PgpPrivate => validate_pgp_private_key(content),
        KeyKind::Cosign => validate_cosign_key(content),
    }
}

fn validate_ssh_private_key(content: &str) -> Result<(), String> {
    let trimmed_start = content.trim_start();
    if !trimmed_start.starts_with("-----BEGIN ") {
        return Err("missing '-----BEGIN ... PRIVATE KEY-----' header".to_string());
    }
    let header_line = trimmed_start.lines().next().unwrap_or("");
    if !header_line.contains("PRIVATE KEY-----") {
        return Err("first line is not a PRIVATE KEY PEM header".to_string());
    }
    let Some(end_pos) = content.rfind("-----END ") else {
        return Err("missing '-----END ... PRIVATE KEY-----' footer".to_string());
    };
    let footer = &content[end_pos..];
    if !footer.contains("PRIVATE KEY-----") {
        return Err("footer is not a PRIVATE KEY PEM footer".to_string());
    }
    // OpenSSH (libcrypto) rejects a key whose END line has no trailing
    // newline — the canonical CI failure is a secret stored via a tool
    // that strips it. Catch that here with a precise message.
    if !content.ends_with('\n') {
        return Err(
            "missing trailing newline after the END marker (OpenSSH rejects such keys; \
             re-store the secret with its final newline intact)"
                .to_string(),
        );
    }
    Ok(())
}

fn validate_pgp_private_key(content: &str) -> Result<(), String> {
    let trimmed = content.trim_start();
    if trimmed.starts_with("-----BEGIN PGP PRIVATE KEY BLOCK-----") {
        if !content.contains("-----END PGP PRIVATE KEY BLOCK-----") {
            return Err("missing '-----END PGP PRIVATE KEY BLOCK-----' footer".to_string());
        }
        return Ok(());
    }
    // Binary OpenPGP packet: first byte has the packet-tag high bit set.
    if trimmed.as_bytes().first().is_some_and(|b| b & 0x80 == 0x80) {
        return Ok(());
    }
    Err(
        "missing '-----BEGIN PGP PRIVATE KEY BLOCK-----' header (not armored or binary OpenPGP)"
            .to_string(),
    )
}

fn validate_cosign_key(content: &str) -> Result<(), String> {
    let trimmed = content.trim_start();
    let known = [
        "-----BEGIN ENCRYPTED SIGSTORE PRIVATE KEY-----",
        "-----BEGIN ENCRYPTED COSIGN PRIVATE KEY-----",
        "-----BEGIN PRIVATE KEY-----",
        "-----BEGIN EC PRIVATE KEY-----",
    ];
    if known.iter().any(|h| trimmed.starts_with(h)) {
        if !content.contains("-----END ") {
            return Err("missing PEM END footer".to_string());
        }
        return Ok(());
    }
    Err("missing sigstore/cosign PEM header".to_string())
}

// ---------------------------------------------------------------------------
// Template env-reference extraction
// ---------------------------------------------------------------------------

/// Extract env-var names referenced as `{{ .Env.NAME }}` / `{{ Env.NAME }}`
/// from a templated config string.
///
/// References inside a template expression that applies a `default(`
/// filter are skipped — a defaulted lookup is satisfiable without the var.
pub fn template_env_refs(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(open) = rest.find("{{") {
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else {
            break;
        };
        let expr = &after[..close];
        if !expr.contains("default(") {
            collect_env_names(expr, &mut out);
        }
        rest = &after[close + 2..];
    }
    out
}

/// The full crate universe of a resolved config: top-level `crates` plus
/// every workspace's crates (first-seen name wins, matching the publish
/// path's `all_crates`). Requirement derivation must union across ALL
/// publishable crates so per-crate workspace mode preflights the same
/// surface the per-crate pipeline will run.
pub fn crate_universe(config: &crate::config::Config) -> Vec<&crate::config::CrateConfig> {
    let mut out: Vec<&crate::config::CrateConfig> = config.crates.iter().collect();
    for ws in config.workspaces.iter().flatten() {
        for c in &ws.crates {
            if !out.iter().any(|e| e.name == c.name) {
                out.push(c);
            }
        }
    }
    out
}

/// When the entire string is a single `{{ [.]Env.NAME }}` expression,
/// return `NAME`. Used to decide whether a templated secret field maps to
/// validatable key material from one env var (vs. a composite template,
/// where only presence of the referenced vars can be required).
pub fn sole_env_ref(s: &str) -> Option<String> {
    let t = s.trim();
    if !(t.starts_with("{{") && t.ends_with("}}")) || t[2..].contains("{{") {
        return None;
    }
    let refs = template_env_refs(t);
    let inner = t[2..t.len() - 2].trim();
    let bare = inner.strip_prefix('.').unwrap_or(inner);
    match refs.as_slice() {
        [only] if bare == format!("Env.{only}") => Some(only.clone()),
        _ => None,
    }
}

/// Extract env-var names referenced via cosign's `env://NAME` key scheme.
pub fn env_scheme_refs(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(pos) = rest.find("env://") {
        let tail = &rest[pos + 6..];
        let name: String = tail
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if !name.is_empty() && !out.contains(&name) {
            out.push(name.clone());
        }
        rest = &tail[name.len()..];
    }
    out
}

fn collect_env_names(expr: &str, out: &mut Vec<String>) {
    let mut rest = expr;
    while let Some(pos) = rest.find("Env.") {
        // Accept both `.Env.NAME` (Go-template style the preprocessor
        // translates) and bare `Env.NAME` (native Tera object lookup), but
        // not identifiers that merely end in `Env.` (e.g. `MyEnv.`).
        let preceded_ok = match rest[..pos].chars().next_back() {
            None => true,
            Some(c) => !(c.is_ascii_alphanumeric() || c == '_'),
        };
        let tail = &rest[pos + 4..];
        let name: String = tail
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if preceded_ok && !name.is_empty() && !out.contains(&name) {
            out.push(name.clone());
        }
        rest = &tail[name.len().min(tail.len())..];
    }
}

/// Convenience: map every env reference in a templated config string
/// (both `{{ .Env.X }}` and `env://X` forms) to an [`EnvRequirement`],
/// tagged with `source`.
pub fn env_ref_requirements(source: &str, value: &str) -> Vec<SourcedRequirement> {
    let mut out = Vec::new();
    let refs = template_env_refs(value);
    if !refs.is_empty() {
        out.push(SourcedRequirement::new(
            source,
            EnvRequirement::EnvAllOf { vars: refs },
        ));
    }
    for var in env_scheme_refs(value) {
        out.push(SourcedRequirement::new(
            source,
            EnvRequirement::EnvAllOf { vars: vec![var] },
        ));
    }
    out
}

/// True when a config entry is statically inactive for this run: its
/// `skip:` / `skip_upload:` evaluates truthy, or its `if:` condition
/// renders falsy. Mirrors the run-path gating for requirement derivation —
/// a `skip: true` entry must not demand tools or credentials from
/// preflight. Anything unrenderable is treated as ACTIVE so preflight
/// over-collects rather than silently under-collecting.
pub fn entry_inactive(
    ctx: &crate::context::Context,
    skip: Option<&crate::config::StringOrBool>,
    skip_upload: Option<&crate::config::StringOrBool>,
    if_condition: Option<&str>,
) -> bool {
    let truthy = |v: &crate::config::StringOrBool| {
        v.try_evaluates_to_true(|t| ctx.render_template(t))
            .unwrap_or(false)
    };
    if skip.is_some_and(truthy) || skip_upload.is_some_and(truthy) {
        return true;
    }
    if_condition.is_some_and(|cond| {
        matches!(
            crate::config::evaluate_if_condition(Some(cond), "preflight", |t| ctx
                .render_template(t)),
            Ok(false)
        )
    })
}

/// Requirement for a templated secret-bearing config value with an env-var
/// fallback: a set value declares its `{{ .Env.X }}` references (a literal
/// declares nothing — the credential is inline); an unset value declares
/// the fallback env var the run path reads instead.
pub fn secret_requirement(
    config_value: Option<&str>,
    fallback_env: &str,
) -> Option<EnvRequirement> {
    match config_value.filter(|v| !v.is_empty()) {
        Some(v) => {
            let refs = template_env_refs(v);
            (!refs.is_empty()).then_some(EnvRequirement::EnvAllOf { vars: refs })
        }
        None => Some(EnvRequirement::EnvAllOf {
            vars: vec![fallback_env.to_string()],
        }),
    }
}

/// The union of build targets this run would compile, mirroring the build
/// stage's resolution: per-build `targets:` (an explicitly empty list means
/// "skip this build"), else `defaults.targets`, else the built-in default
/// matrix; a skipped build (`skip:` truthy) contributes nothing; an
/// unrenderable `skip:` counts as active (over-collect). `--single-target`
/// narrows the union to the requested triple (exact match first, then the
/// same OS/arch alias fallback the build stage applies), so a host-only
/// release never demands cross-platform bundler tools.
pub fn configured_build_targets(ctx: &crate::context::Context) -> Vec<String> {
    let default_targets: Vec<String> = ctx
        .config
        .defaults
        .as_ref()
        .and_then(|d| d.targets.clone())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| {
            crate::target::DEFAULT_TARGETS
                .iter()
                .map(|s| (*s).to_string())
                .collect()
        });
    let mut out: Vec<String> = Vec::new();
    let mut push = |t: &str| {
        if !out.iter().any(|x| x == t) {
            out.push(t.to_string());
        }
    };
    for krate in crate_universe(&ctx.config) {
        match krate.builds.as_ref().filter(|b| !b.is_empty()) {
            Some(builds) => {
                for build in builds {
                    if entry_inactive(ctx, build.skip.as_ref(), None, None) {
                        continue;
                    }
                    match build.targets.as_ref() {
                        Some(targets) => targets.iter().for_each(|t| push(t)),
                        None => default_targets.iter().for_each(|t| push(t)),
                    }
                }
            }
            // No builds configured: the build stage synthesizes a default
            // binary build over the default target matrix.
            None => default_targets.iter().for_each(|t| push(t)),
        }
    }
    if let Some(single) = ctx.options.single_target.as_deref() {
        if out.iter().any(|t| t == single) {
            out.retain(|t| t == single);
        } else {
            out = crate::partial::find_runtime_target(single, &out)
                .into_iter()
                .collect();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn all_pass_probes() -> EnvProbes<'static> {
        EnvProbes {
            tool: &|_| true,
            endpoint: &|_| Ok(()),
            docker: &|| true,
        }
    }

    fn req(source: &str, r: EnvRequirement) -> SourcedRequirement {
        SourcedRequirement::new(source, r)
    }

    #[test]
    fn empty_requirements_pass() {
        let report = evaluate(&[], &no_env, &all_pass_probes());
        assert!(report.ok());
        assert_eq!(report.checks, 0);
    }

    #[test]
    fn tool_any_of_passes_when_one_is_present() {
        let ladder = EnvRequirement::ToolAnyOf {
            names: vec!["hdiutil".into(), "genisoimage".into(), "mkisofs".into()],
        };
        let probes = EnvProbes {
            tool: &|name| name == "mkisofs",
            endpoint: &|_| Ok(()),
            docker: &|| true,
        };
        let report = evaluate(&[req("stage:dmg", ladder.clone())], &no_env, &probes);
        assert!(report.ok(), "one available tool must satisfy the ladder");

        let none_available = EnvProbes {
            tool: &|_| false,
            endpoint: &|_| Ok(()),
            docker: &|| true,
        };
        let report = evaluate(&[req("stage:dmg", ladder)], &no_env, &none_available);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].kind, FailureKind::MissingTool);
        assert!(
            report.failures[0].message.contains("hdiutil")
                && report.failures[0].message.contains("mkisofs"),
            "message must list the whole ladder: {}",
            report.failures[0].message
        );
    }

    #[test]
    fn collects_all_failures_in_one_pass() {
        let reqs = vec![
            req(
                "stage:nfpm",
                EnvRequirement::Tool {
                    name: "nfpm".into(),
                },
            ),
            req(
                "publish:cargo",
                EnvRequirement::EnvAllOf {
                    vars: vec!["CARGO_REGISTRY_TOKEN".into()],
                },
            ),
            req("stage:docker", EnvRequirement::DockerDaemon),
            req(
                "stage:blob",
                EnvRequirement::Endpoint {
                    url: "http://minio.example".into(),
                },
            ),
        ];
        let probes = EnvProbes {
            tool: &|_| false,
            endpoint: &|_| Err("connection refused".into()),
            docker: &|| false,
        };
        let report = evaluate(&reqs, &no_env, &probes);
        assert_eq!(report.checks, 4);
        assert_eq!(
            report.failures.len(),
            4,
            "every failure must be reported in one pass: {report}"
        );
    }

    #[test]
    fn classifies_tool_vs_env_failures() {
        let reqs = vec![
            req(
                "a",
                EnvRequirement::Tool {
                    name: "syft".into(),
                },
            ),
            req(
                "b",
                EnvRequirement::EnvAllOf {
                    vars: vec!["NPM_TOKEN".into()],
                },
            ),
        ];
        let probes = EnvProbes {
            tool: &|_| false,
            endpoint: &|_| Ok(()),
            docker: &|| true,
        };
        let report = evaluate(&reqs, &no_env, &probes);
        assert_eq!(report.failures[0].kind, FailureKind::MissingTool);
        assert_eq!(report.failures[1].kind, FailureKind::MissingEnv);
    }

    #[test]
    fn dedup_merges_sources_for_identical_requirements() {
        let reqs = vec![
            req(
                "publish:homebrew",
                EnvRequirement::Tool { name: "git".into() },
            ),
            req("publish:scoop", EnvRequirement::Tool { name: "git".into() }),
            req(
                "publish:homebrew",
                EnvRequirement::Tool { name: "git".into() },
            ),
        ];
        let probes = EnvProbes {
            tool: &|_| false,
            endpoint: &|_| Ok(()),
            docker: &|| true,
        };
        let report = evaluate(&reqs, &no_env, &probes);
        assert_eq!(report.checks, 1, "identical requirements must merge");
        assert_eq!(
            report.failures[0].needed_by,
            vec!["publish:homebrew".to_string(), "publish:scoop".to_string()]
        );
    }

    #[test]
    fn env_any_of_passes_when_one_var_present() {
        let reqs = vec![req(
            "publish:release",
            EnvRequirement::EnvAnyOf {
                vars: vec!["ANODIZER_GITHUB_TOKEN".into(), "GITHUB_TOKEN".into()],
            },
        )];
        let env = |k: &str| (k == "GITHUB_TOKEN").then(|| "tok".to_string());
        let report = evaluate(&reqs, &env, &all_pass_probes());
        assert!(report.ok(), "{report}");
    }

    #[test]
    fn empty_env_value_counts_as_missing() {
        let reqs = vec![req(
            "s",
            EnvRequirement::EnvAllOf {
                vars: vec!["EMPTY_VAR".into()],
            },
        )];
        let env = |_: &str| Some(String::new());
        let report = evaluate(&reqs, &env, &all_pass_probes());
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].kind, FailureKind::MissingEnv);
    }

    #[test]
    fn report_never_echoes_secret_values() {
        const SECRET: &str = "hunter2-super-secret-value";
        let reqs = vec![
            req(
                "publish:aur",
                EnvRequirement::KeyEnv {
                    kind: KeyKind::SshPrivate,
                    var: "AUR_SSH_KEY".into(),
                },
            ),
            req(
                "stage:sign",
                EnvRequirement::KeyEnv {
                    kind: KeyKind::Cosign,
                    var: "COSIGN_KEY".into(),
                },
            ),
        ];
        // Both vars hold the secret but are structurally invalid keys, so
        // both checks fail — the failure text must never leak the value.
        let env = |_: &str| Some(SECRET.to_string());
        let report = evaluate(&reqs, &env, &all_pass_probes());
        assert_eq!(report.failures.len(), 2);
        let rendered = report.to_string();
        assert!(
            !rendered.contains(SECRET),
            "report leaked a secret value: {rendered}"
        );
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains(SECRET), "json report leaked a secret value");
    }

    #[test]
    fn ssh_key_valid_openssh_block_passes() {
        let key = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaA==\n-----END OPENSSH PRIVATE KEY-----\n";
        assert!(validate_key_material(KeyKind::SshPrivate, key).is_ok());
    }

    #[test]
    fn ssh_key_missing_trailing_newline_is_flagged() {
        let key =
            "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaA==\n-----END OPENSSH PRIVATE KEY-----";
        let err = validate_key_material(KeyKind::SshPrivate, key).unwrap_err();
        assert!(err.contains("trailing newline"), "got: {err}");
    }

    #[test]
    fn ssh_key_garbage_is_rejected_without_echo() {
        let err = validate_key_material(KeyKind::SshPrivate, "not-a-key-material").unwrap_err();
        assert!(!err.contains("not-a-key-material"));
    }

    #[test]
    fn pgp_armored_and_binary_pass_garbage_fails() {
        let armored =
            "-----BEGIN PGP PRIVATE KEY BLOCK-----\nxx\n-----END PGP PRIVATE KEY BLOCK-----\n";
        assert!(validate_key_material(KeyKind::PgpPrivate, armored).is_ok());
        // 0x95 = binary OpenPGP secret-key packet tag byte.
        let binary = "\u{95}binarystuff";
        assert!(validate_key_material(KeyKind::PgpPrivate, binary).is_ok());
        assert!(validate_key_material(KeyKind::PgpPrivate, "plain text").is_err());
    }

    #[test]
    fn cosign_sigstore_header_passes() {
        let key = "-----BEGIN ENCRYPTED SIGSTORE PRIVATE KEY-----\nxx\n-----END ENCRYPTED SIGSTORE PRIVATE KEY-----\n";
        assert!(validate_key_material(KeyKind::Cosign, key).is_ok());
        assert!(validate_key_material(KeyKind::Cosign, "ghp_token").is_err());
    }

    #[test]
    fn key_env_missing_var_classifies_as_missing_env() {
        let reqs = vec![req(
            "publish:aur",
            EnvRequirement::KeyEnv {
                kind: KeyKind::SshPrivate,
                var: "AUR_SSH_KEY".into(),
            },
        )];
        let report = evaluate(&reqs, &no_env, &all_pass_probes());
        assert_eq!(report.failures[0].kind, FailureKind::MissingEnv);
    }

    #[test]
    fn key_file_missing_and_invalid_are_flagged() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("absent.key");
        let invalid = dir.path().join("invalid.key");
        std::fs::write(&invalid, "not a pgp key").unwrap();
        let reqs = vec![
            req(
                "stage:nfpm",
                EnvRequirement::KeyFile {
                    kind: KeyKind::PgpPrivate,
                    path: missing.display().to_string(),
                },
            ),
            req(
                "stage:nfpm",
                EnvRequirement::KeyFile {
                    kind: KeyKind::PgpPrivate,
                    path: invalid.display().to_string(),
                },
            ),
        ];
        let report = evaluate(&reqs, &no_env, &all_pass_probes());
        assert_eq!(report.failures.len(), 2);
        assert!(
            report
                .failures
                .iter()
                .all(|f| f.kind == FailureKind::BadKeyMaterial)
        );
    }

    #[test]
    fn template_env_refs_handles_both_styles_and_default_filter() {
        assert_eq!(
            template_env_refs("{{ .Env.AUR_SSH_KEY }}"),
            vec!["AUR_SSH_KEY"]
        );
        assert_eq!(
            template_env_refs("{{ Env.MINIO_ENDPOINT }}/bucket"),
            vec!["MINIO_ENDPOINT"]
        );
        assert_eq!(
            template_env_refs("{{ Env.A }}-{{ .Env.B }}"),
            vec!["A", "B"]
        );
        assert!(
            template_env_refs(r#"{{ Env.OPTIONAL | default(value="x") }}"#).is_empty(),
            "default()-filtered refs are satisfiable without the var"
        );
        assert!(template_env_refs("no refs here").is_empty());
        assert!(
            template_env_refs("{{ MyEnv.NOT_AN_ENV }}").is_empty(),
            "identifiers ending in 'Env.' must not match"
        );
    }

    #[test]
    fn sole_env_ref_only_matches_whole_single_expressions() {
        assert_eq!(
            sole_env_ref("{{ .Env.AUR_SSH_KEY }}"),
            Some("AUR_SSH_KEY".to_string())
        );
        assert_eq!(
            sole_env_ref("{{ Env.AUR_SSH_KEY }}"),
            Some("AUR_SSH_KEY".to_string())
        );
        assert_eq!(sole_env_ref("prefix {{ .Env.X }}"), None);
        assert_eq!(sole_env_ref("{{ .Env.X }}{{ .Env.Y }}"), None);
        assert_eq!(sole_env_ref("/path/to/key"), None);
    }

    #[test]
    fn env_scheme_refs_extracts_cosign_style() {
        assert_eq!(
            env_scheme_refs("--key=env://COSIGN_KEY"),
            vec!["COSIGN_KEY"]
        );
        assert!(env_scheme_refs("--key=cosign.key").is_empty());
    }

    #[test]
    fn endpoint_failure_includes_url_and_reason() {
        let reqs = vec![req(
            "stage:blob",
            EnvRequirement::Endpoint {
                url: "http://minio.local:9000".into(),
            },
        )];
        let probes = EnvProbes {
            tool: &|_| true,
            endpoint: &|_| Err("timed out".into()),
            docker: &|| true,
        };
        let report = evaluate(&reqs, &no_env, &probes);
        assert!(
            report.failures[0]
                .message
                .contains("http://minio.local:9000")
        );
        assert!(report.failures[0].message.contains("timed out"));
    }

    #[test]
    fn configured_build_targets_mirror_build_resolution() {
        use crate::config::{BuildConfig, Config, CrateConfig, StringOrBool};
        use crate::context::{Context, ContextOptions};

        let krate = |name: &str, builds: Option<Vec<BuildConfig>>| CrateConfig {
            name: name.to_string(),
            builds,
            ..Default::default()
        };

        // No builds anywhere: the default matrix applies.
        let config = Config {
            crates: vec![krate("app", None)],
            ..Default::default()
        };
        let ctx = Context::new(config, ContextOptions::default());
        assert_eq!(
            configured_build_targets(&ctx),
            crate::target::DEFAULT_TARGETS
                .iter()
                .map(|s| (*s).to_string())
                .collect::<Vec<_>>()
        );

        // Explicit per-build targets win; a skipped build and an
        // explicitly-empty target list contribute nothing; the union spans
        // crates.
        let config = Config {
            crates: vec![
                krate(
                    "app",
                    Some(vec![
                        BuildConfig {
                            targets: Some(vec!["x86_64-unknown-linux-gnu".to_string()]),
                            ..Default::default()
                        },
                        BuildConfig {
                            targets: Some(vec!["x86_64-pc-windows-msvc".to_string()]),
                            skip: Some(StringOrBool::Bool(true)),
                            ..Default::default()
                        },
                    ]),
                ),
                krate(
                    "helper",
                    Some(vec![BuildConfig {
                        targets: Some(vec![
                            "aarch64-apple-darwin".to_string(),
                            "x86_64-unknown-linux-gnu".to_string(),
                        ]),
                        ..Default::default()
                    }]),
                ),
            ],
            ..Default::default()
        };
        let ctx = Context::new(config.clone(), ContextOptions::default());
        assert_eq!(
            configured_build_targets(&ctx),
            vec![
                "x86_64-unknown-linux-gnu".to_string(),
                "aarch64-apple-darwin".to_string(),
            ]
        );

        // --single-target narrows the union to the requested triple.
        let ctx = Context::new(
            config,
            ContextOptions {
                single_target: Some("aarch64-apple-darwin".to_string()),
                ..Default::default()
            },
        );
        assert_eq!(
            configured_build_targets(&ctx),
            vec!["aarch64-apple-darwin".to_string()]
        );
    }
}
