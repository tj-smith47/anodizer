//! `anodizer tools` — emit the external CLI tools the resolved config's
//! pipeline will invoke, derived from the SAME `env_requirements` /
//! `Publisher::requirements` SSOT the preflight engine consumes.
//!
//! The GitHub Action consumes this to decide what to install on a runner
//! instead of re-grepping the config in shell (which drifts from anodizer's
//! truth). Because the tool set is filtered out of
//! [`super::preflight::collect_requirements`], adding a tool-bearing stage or
//! publisher updates this emit automatically — there is no hand-maintained
//! list.

use std::path::PathBuf;

use anodizer_core::EnvRequirement;
use anodizer_core::log::{StageLogger, Verbosity};
use anyhow::Result;

use super::preflight::{PreflightScope, collect_requirements};

/// One tool requirement. `any_of` lists interchangeable binaries — install
/// ANY one to satisfy it (e.g. the dmg stage accepts
/// `hdiutil` / `genisoimage` / `mkisofs`). A plain single-tool requirement is
/// a one-element `any_of`, so consumers iterate one uniform shape.
#[derive(serde::Serialize, Debug, PartialEq, Eq)]
struct ToolRequirement {
    any_of: Vec<String>,
}

/// Stable JSON envelope for `anodizer tools --json`. `schema_version` bumps
/// only on a breaking shape change (additive tool growth keeps it stable).
#[derive(serde::Serialize, Debug)]
struct Tools {
    schema_version: u32,
    tools: Vec<ToolRequirement>,
}

/// Current `tools` JSON schema version.
const SCHEMA_VERSION: u32 = 1;

pub struct ToolsOpts {
    pub config_override: Option<PathBuf>,
    pub json: bool,
    pub publish_only: bool,
    pub skip: Vec<String>,
    pub publishers: Vec<String>,
    pub quiet: bool,
    pub verbose: bool,
    pub debug: bool,
}

/// Extract the tool requirements from a collected requirement set: keep only
/// the [`EnvRequirement::Tool`] / [`EnvRequirement::ToolAnyOf`] kinds, fold
/// each to a uniform `any_of` group, then de-duplicate (preserving first-seen
/// order) and sort for stable output.
fn tool_requirements(reqs: &[anodizer_core::SourcedRequirement]) -> Vec<ToolRequirement> {
    let mut out: Vec<ToolRequirement> = Vec::new();
    for sr in reqs {
        let any_of = match &sr.requirement {
            EnvRequirement::Tool { name } => vec![name.clone()],
            EnvRequirement::ToolAnyOf { names } => names.clone(),
            _ => continue,
        };
        let req = ToolRequirement { any_of };
        if !out.contains(&req) {
            out.push(req);
        }
    }
    out.sort_by(|a, b| a.any_of.cmp(&b.any_of));
    out
}

pub fn run(opts: ToolsOpts) -> Result<()> {
    let log = StageLogger::new(
        "tools",
        Verbosity::from_flags(opts.quiet, opts.verbose, opts.debug),
    );

    // Observational context, matching `preflight`: `dry_run` keeps init from
    // bailing on guards the emit does not depend on (a missing token must not
    // abort tool enumeration), and `snapshot` lets it run from any commit —
    // requirement derivation never depends on HEAD being tagged.
    let ctx_opts = anodizer_core::context::ContextOptions {
        skip_stages: opts.skip.clone(),
        publisher_allowlist: opts.publishers.clone(),
        quiet: opts.quiet,
        verbose: opts.verbose,
        debug: opts.debug,
        dry_run: true,
        snapshot: true,
        ..Default::default()
    };
    let (_config, ctx) =
        super::helpers::init_merge_stage_ctx(opts.config_override.as_deref(), ctx_opts, &log)?;

    let scope = if opts.publish_only {
        PreflightScope::PublishOnly
    } else {
        PreflightScope::Full
    };
    let requirements = collect_requirements(&ctx, scope);
    let tools = Tools {
        schema_version: SCHEMA_VERSION,
        tools: tool_requirements(&requirements),
    };

    if opts.json {
        println!("{}", serde_json::to_string(&tools)?);
    } else if tools.tools.is_empty() {
        println!("(no external tools required by this config)");
    } else {
        for t in &tools.tools {
            println!("{}", t.any_of.join(" | "));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::{KeyKind, SourcedRequirement};

    fn tool(name: &str) -> EnvRequirement {
        EnvRequirement::Tool {
            name: name.to_string(),
        }
    }

    #[test]
    fn keeps_tool_and_tool_any_of_drops_others() {
        let reqs = vec![
            SourcedRequirement::new("stage:build", tool("cargo")),
            SourcedRequirement::new("stage:nfpm", tool("nfpm")),
            SourcedRequirement::new(
                "stage:dmg",
                EnvRequirement::ToolAnyOf {
                    names: vec!["genisoimage".into(), "mkisofs".into()],
                },
            ),
            // Non-tool requirements must not appear in the tool emit.
            SourcedRequirement::new(
                "publish:cargo",
                EnvRequirement::EnvAllOf {
                    vars: vec!["CARGO_REGISTRY_TOKEN".into()],
                },
            ),
            SourcedRequirement::new(
                "stage:sign",
                EnvRequirement::KeyEnv {
                    kind: KeyKind::Cosign,
                    var: "COSIGN_KEY".into(),
                },
            ),
        ];
        let tools = tool_requirements(&reqs);
        let flat: Vec<&str> = tools
            .iter()
            .flat_map(|t| t.any_of.iter().map(|s| s.as_str()))
            .collect();
        assert!(flat.contains(&"cargo"));
        assert!(flat.contains(&"nfpm"));
        assert!(flat.contains(&"genisoimage"));
        assert!(flat.contains(&"mkisofs"));
        // No env-var / key-material names leaked into the tool list.
        assert!(!flat.contains(&"CARGO_REGISTRY_TOKEN"));
        assert!(!flat.contains(&"COSIGN_KEY"));
    }

    #[test]
    fn any_of_group_is_preserved_as_one_requirement() {
        let reqs = vec![SourcedRequirement::new(
            "stage:dmg",
            EnvRequirement::ToolAnyOf {
                names: vec!["hdiutil".into(), "genisoimage".into(), "mkisofs".into()],
            },
        )];
        let tools = tool_requirements(&reqs);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].any_of, vec!["hdiutil", "genisoimage", "mkisofs"]);
    }

    #[test]
    fn duplicate_tool_requirements_are_deduped() {
        let reqs = vec![
            SourcedRequirement::new("stage:build", tool("cargo")),
            SourcedRequirement::new("publish:cargo", tool("cargo")),
        ];
        let tools = tool_requirements(&reqs);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].any_of, vec!["cargo"]);
    }

    #[test]
    fn output_is_sorted_for_stability() {
        let reqs = vec![
            SourcedRequirement::new("s", tool("syft")),
            SourcedRequirement::new("s", tool("cosign")),
            SourcedRequirement::new("s", tool("cargo")),
        ];
        let tools = tool_requirements(&reqs);
        let names: Vec<&str> = tools.iter().map(|t| t.any_of[0].as_str()).collect();
        assert_eq!(names, vec!["cargo", "cosign", "syft"]);
    }

    #[test]
    fn json_envelope_shape() {
        let tools = Tools {
            schema_version: SCHEMA_VERSION,
            tools: vec![ToolRequirement {
                any_of: vec!["cargo".into()],
            }],
        };
        let json = serde_json::to_string(&tools).unwrap();
        assert!(json.contains(r#""schema_version":1"#));
        assert!(json.contains(r#""tools":[{"any_of":["cargo"]}]"#));
    }
}
