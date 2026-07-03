use anodizer_core::log::{StageLogger, Verbosity};
use anyhow::Result;
use colored::Colorize;

/// Tool entry with name and description for display.
struct ToolCheck {
    name: &'static str,
    description: &'static str,
}

const TOOLS: &[ToolCheck] = &[
    ToolCheck {
        name: "cargo",
        description: "Rust package manager",
    },
    ToolCheck {
        name: "git",
        description: "Version control",
    },
    ToolCheck {
        name: "docker",
        description: "Container runtime",
    },
    ToolCheck {
        name: "podman",
        description: "Container runtime (Linux-only alt backend)",
    },
    ToolCheck {
        name: "nfpm",
        description: "Linux package builder (deb/rpm/apk)",
    },
    ToolCheck {
        name: "cargo-zigbuild",
        description: "Cross-compilation via Zig",
    },
    ToolCheck {
        name: "zig",
        description: "Zig toolchain (linker/libc behind cargo-zigbuild)",
    },
    ToolCheck {
        name: "cross",
        description: "Cross-compilation via Docker",
    },
    ToolCheck {
        name: "gpg",
        description: "GNU Privacy Guard (signing)",
    },
    ToolCheck {
        name: "cosign",
        description: "Sigstore container signing",
    },
    ToolCheck {
        name: "aws",
        description: "AWS CLI (S3 blob storage)",
    },
    ToolCheck {
        name: "gsutil",
        description: "Google Cloud Storage CLI",
    },
    ToolCheck {
        name: "az",
        description: "Azure CLI (Blob storage)",
    },
];

use anodizer_core::tool_detect::{ToolProbe, runs, tool_version};

pub fn run() -> Result<()> {
    let log = StageLogger::new("healthcheck", Verbosity::Normal);

    log.status(&format!("{}", "Anodizer Environment Health Check".bold()));
    log.status(&"=".repeat(40));

    let mut available_count = 0;
    let mut missing_count = 0;
    let mut unprobeable_count = 0;

    for tool in TOOLS {
        match runs(tool.name) {
            ToolProbe::Available => {
                // No version-looking output (or a failed re-probe) → omit
                // the parenthetical entirely rather than render noise.
                let version = match tool_version(tool.name) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::trace!(tool = tool.name, error = %e, "version probe failed");
                        None
                    }
                };
                let parenthetical = version
                    .map(|v| format!(" ({})", v.dimmed()))
                    .unwrap_or_default();
                log.status(&format!(
                    "{} {:<20} {}{}",
                    "\u{2713}".green().bold(),
                    tool.name,
                    tool.description.dimmed(),
                    parenthetical
                ));
                available_count += 1;
            }
            ToolProbe::Unavailable => {
                log.status(&format!(
                    "{} {:<20} {}",
                    "\u{2717}".red().bold(),
                    tool.name,
                    tool.description.dimmed()
                ));
                missing_count += 1;
            }
            // A broken probe is NOT "missing": presence is unknown, and a
            // health report claiming absence would send the operator to
            // reinstall a tool that may be present. Render it as its own
            // outcome and name the error.
            ToolProbe::ProbeFailed(e) => {
                log.status(&format!(
                    "{} {:<20} {} (probe failed: {})",
                    "?".yellow().bold(),
                    tool.name,
                    tool.description.dimmed(),
                    e
                ));
                unprobeable_count += 1;
            }
        }
    }

    let mut summary = format!(
        "{} available, {} missing",
        available_count.to_string().green().bold(),
        missing_count.to_string().yellow().bold()
    );
    if unprobeable_count > 0 {
        summary.push_str(&format!(
            ", {} unprobeable",
            unprobeable_count.to_string().red().bold()
        ));
    }
    log.status(&summary);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_runs_cargo() {
        // cargo should always be available in a Rust project
        assert!(
            matches!(runs("cargo"), ToolProbe::Available),
            "cargo should be available"
        );
    }

    #[test]
    fn test_runs_nonexistent_is_unavailable() {
        // The NotFound-folds-into-Unavailable decision lives in
        // `tool_detect::runs`; healthcheck renders it as missing.
        assert!(matches!(
            runs("this-tool-does-not-exist-12345"),
            ToolProbe::Unavailable
        ));
    }

    #[test]
    fn test_tool_version_cargo() {
        let version = tool_version("cargo").expect("cargo should spawn");
        assert!(version.is_some(), "cargo --version should produce output");
        assert!(
            version.unwrap().contains("cargo"),
            "cargo version should contain 'cargo'"
        );
    }

    #[test]
    fn test_tool_version_nonexistent_surfaces_error() {
        let res = tool_version("this-tool-does-not-exist-12345");
        assert!(res.is_err(), "nonexistent tool must surface a spawn error");
    }

    #[test]
    fn test_tools_list_is_not_empty() {
        assert!(!TOOLS.is_empty(), "TOOLS list should not be empty");
    }

    #[test]
    fn test_healthcheck_run_succeeds() {
        // healthcheck should never fail -- it just reports status
        let result = run();
        assert!(result.is_ok(), "healthcheck should always succeed");
    }
}
