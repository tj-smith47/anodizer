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
///
/// `advisory` is `true` when EVERY contributing requirement is advisory: the
/// pipeline degrades gracefully without the tool (the canonical case is the
/// build's cross-compile toolchain, which falls back zigbuild → cargo → system
/// gcc). The Action installs advisory tools when it can but must not fail a
/// runner that lacks them; a required tool (`advisory: false`) is mandatory.
/// One hard need anywhere makes the merged group required — matching the
/// preflight engine's pass/fail semantics.
#[derive(serde::Serialize, Debug, PartialEq, Eq)]
struct ToolRequirement {
    any_of: Vec<String>,
    advisory: bool,
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
/// order) and sort for stable output. A merged group is `advisory` only when
/// EVERY contributing requirement is advisory — one hard need makes it
/// required, matching the preflight engine's pass/fail merge.
fn tool_requirements(reqs: &[anodizer_core::SourcedRequirement]) -> Vec<ToolRequirement> {
    let mut out: Vec<ToolRequirement> = Vec::new();
    for sr in reqs {
        let any_of = match &sr.requirement {
            EnvRequirement::Tool { name } => vec![name.clone()],
            EnvRequirement::ToolAnyOf { names } => names.clone(),
            _ => continue,
        };
        match out.iter_mut().find(|r| r.any_of == any_of) {
            Some(existing) => existing.advisory = existing.advisory && sr.advisory,
            None => out.push(ToolRequirement {
                any_of,
                advisory: sr.advisory,
            }),
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
            let suffix = if t.advisory { "  (recommended)" } else { "" };
            println!("{}{}", t.any_of.join(" | "), suffix);
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

    /// End-to-end through `collect_requirements`: a config that cross-compiles
    /// to a non-host glibc-Linux target surfaces `cargo-zigbuild` + `zig` in the
    /// `tools` JSON, so the GitHub Action installs the cross toolchain instead
    /// of re-deriving it in bash. Skipped off x86_64-linux-gnu (Auto routing
    /// is host-dependent).
    #[test]
    fn cross_config_emits_zigbuild_and_zig_in_json() {
        use anodizer_core::test_helpers::TestContextBuilder;

        if anodizer_core::partial::detect_host_target()
            .as_deref()
            .unwrap_or_default()
            != "x86_64-unknown-linux-gnu"
        {
            return;
        }
        let krate: anodizer_core::config::CrateConfig = serde_yaml_ng::from_str(
            r#"
name: app
builds:
  - binary: app
    targets: [aarch64-unknown-linux-gnu]
"#,
        )
        .expect("crate config yaml");
        let ctx = TestContextBuilder::new().crates(vec![krate]).build();

        let reqs = collect_requirements(&ctx, PreflightScope::Full);
        let tools = Tools {
            schema_version: SCHEMA_VERSION,
            tools: tool_requirements(&reqs),
        };
        let json = serde_json::to_string(&tools).unwrap();
        assert!(
            json.contains(r#"["cargo-zigbuild"]"#),
            "tools JSON must list cargo-zigbuild: {json}"
        );
        assert!(
            json.contains(r#"["zig"]"#),
            "tools JSON must list zig: {json}"
        );
        assert!(
            json.contains(r#"["cargo"]"#),
            "tools JSON must still list cargo: {json}"
        );
        // The Action's SSOT must distinguish recommended from required: the
        // cross toolchain is advisory (the build degrades gracefully without
        // it), cargo is required.
        let zig = tools
            .tools
            .iter()
            .find(|t| t.any_of == vec!["zig".to_string()])
            .expect("zig present");
        assert!(zig.advisory, "zig must be advisory in the tools emit");
        let cargo = tools
            .tools
            .iter()
            .find(|t| t.any_of == vec!["cargo".to_string()])
            .expect("cargo present");
        assert!(
            !cargo.advisory,
            "cargo must stay required in the tools emit"
        );
    }

    /// End-to-end through `collect_requirements`: a chocolatey-configured
    /// crate surfaces `xmllint` in the `tools` JSON as REQUIRED, so the GitHub
    /// Action installs it up front instead of the release dying at the strict
    /// prepublish guard (which hard-fails without `xmllint`) after the GitHub
    /// release already shipped.
    #[test]
    fn chocolatey_config_emits_xmllint_in_json() {
        use anodizer_core::test_helpers::TestContextBuilder;

        let krate: anodizer_core::config::CrateConfig = serde_yaml_ng::from_str(
            r#"
name: app
publish:
  chocolatey:
    description: A great tool
"#,
        )
        .expect("crate config yaml");
        let ctx = TestContextBuilder::new().crates(vec![krate]).build();

        let reqs = collect_requirements(&ctx, PreflightScope::Full);
        let tools = Tools {
            schema_version: SCHEMA_VERSION,
            tools: tool_requirements(&reqs),
        };
        let json = serde_json::to_string(&tools).unwrap();
        assert!(
            json.contains(r#"["xmllint"]"#),
            "tools JSON must list xmllint: {json}"
        );
        let xmllint = tools
            .tools
            .iter()
            .find(|t| t.any_of == vec!["xmllint".to_string()])
            .expect("xmllint present");
        assert!(
            !xmllint.advisory,
            "xmllint must be required in the tools emit — the prepublish guard hard-fails without it"
        );
    }

    /// End-to-end through `collect_requirements`: a homebrew-configured crate
    /// surfaces the optional `ruby -c` validator in the `tools` JSON as
    /// ADVISORY (the schema floor warn+skips without it), alongside the hard
    /// `git` requirement — so the GitHub Action provisions the stronger
    /// validation without a missing ruby ever blocking the gate.
    #[test]
    fn homebrew_config_emits_advisory_ruby_in_json() {
        use anodizer_core::test_helpers::TestContextBuilder;

        let krate: anodizer_core::config::CrateConfig = serde_yaml_ng::from_str(
            r#"
name: app
publish:
  homebrew:
    repository:
      owner: acme
      name: homebrew-tap
"#,
        )
        .expect("crate config yaml");
        let ctx = TestContextBuilder::new().crates(vec![krate]).build();

        let reqs = collect_requirements(&ctx, PreflightScope::Full);
        let tools = Tools {
            schema_version: SCHEMA_VERSION,
            tools: tool_requirements(&reqs),
        };
        let find = |name: &str| {
            tools
                .tools
                .iter()
                .find(|t| t.any_of == vec![name.to_string()])
        };
        let ruby = find("ruby").expect("ruby present in tools emit");
        assert!(
            ruby.advisory,
            "ruby must be advisory — the homebrew schema floor degrades gracefully without it"
        );
        let git = find("git").expect("git present in tools emit");
        assert!(!git.advisory, "git must stay required for the tap push");
    }

    #[test]
    fn json_envelope_shape() {
        let tools = Tools {
            schema_version: SCHEMA_VERSION,
            tools: vec![ToolRequirement {
                any_of: vec!["cargo".into()],
                advisory: false,
            }],
        };
        let json = serde_json::to_string(&tools).unwrap();
        assert!(json.contains(r#""schema_version":1"#));
        assert!(json.contains(r#""tools":[{"any_of":["cargo"],"advisory":false}]"#));
    }

    #[test]
    fn advisory_flag_is_anded_across_sources() {
        // A tool needed by an advisory build source stays advisory; the same
        // tool also needed by a hard source flips to required. Mirrors the
        // preflight engine's merge so `anodizer tools` and the gate agree.
        let reqs = vec![
            SourcedRequirement::new_advisory("stage:build", tool("zig")),
            SourcedRequirement::new_advisory("stage:build", tool("cc")),
            SourcedRequirement::new("stage:other", tool("cc")),
        ];
        let tools = tool_requirements(&reqs);
        let advisory_of = |name: &str| {
            tools
                .iter()
                .find(|t| t.any_of == vec![name.to_string()])
                .unwrap_or_else(|| panic!("missing tool {name}"))
                .advisory
        };
        assert!(advisory_of("zig"), "zig: only advisory sources ⇒ advisory");
        assert!(
            !advisory_of("cc"),
            "cc: one hard source ⇒ required despite an advisory source"
        );
    }
}
