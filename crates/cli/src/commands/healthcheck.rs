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
        name: "nfpm",
        description: "Linux package builder (deb/rpm/apk)",
    },
    ToolCheck {
        name: "cargo-zigbuild",
        description: "Cross-compilation via Zig",
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

use anodizer_core::tool_detect::{tool_available, tool_version};

pub fn run() -> Result<()> {
    let log = StageLogger::new("healthcheck", Verbosity::Normal);

    log.status(&format!("{}", "Anodizer Environment Health Check".bold()));
    log.status(&"=".repeat(40));

    let mut available_count = 0;
    let mut missing_count = 0;

    for tool in TOOLS {
        // tool_available returns Err only when the spawn itself fails (typically
        // ENOENT — the binary is not on PATH). That is the same observable
        // outcome as "tool missing" for this surface, so we collapse Err into
        // the missing branch and log the underlying io::Error at trace level
        // for verbose-mode debugging.
        let available = match tool_available(tool.name) {
            Ok(b) => b,
            Err(e) => {
                tracing::trace!(tool = tool.name, error = %e, "probe failed");
                false
            }
        };
        if available {
            let version = match tool_version(tool.name) {
                Ok(Some(v)) => v,
                Ok(None) => "unknown version".to_string(),
                Err(e) => {
                    tracing::trace!(tool = tool.name, error = %e, "version probe failed");
                    "unknown version".to_string()
                }
            };
            log.status(&format!(
                "{} {:<20} {} ({})",
                "\u{2713}".green().bold(),
                tool.name,
                tool.description.dimmed(),
                version.dimmed()
            ));
            available_count += 1;
        } else {
            log.status(&format!(
                "{} {:<20} {}",
                "\u{2717}".red().bold(),
                tool.name,
                tool.description.dimmed()
            ));
            missing_count += 1;
        }
    }

    log.status(&format!(
        "{} available, {} missing",
        available_count.to_string().green().bold(),
        missing_count.to_string().yellow().bold()
    ));

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_available_cargo() {
        // cargo should always be available in a Rust project
        assert!(
            tool_available("cargo").expect("cargo should spawn"),
            "cargo should be available"
        );
    }

    #[test]
    fn test_tool_available_nonexistent() {
        // Lifted: the missing-binary case now surfaces as Err(NotFound) instead
        // of a silently swallowed bool — the regression-test signature is the
        // whole point of the lift. Callers can collapse Err to "treat as
        // missing" but the io::Error must reach them.
        let res = tool_available("this-tool-does-not-exist-12345");
        assert!(res.is_err(), "nonexistent tool must surface a spawn error");
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
