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
];

fn tool_available(name: &str) -> bool {
    std::process::Command::new(name)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn tool_version(name: &str) -> Option<String> {
    let output = std::process::Command::new(name)
        .arg("--version")
        .output()
        .ok()?;
    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        // Take the first line and trim it
        Some(stdout.lines().next().unwrap_or("").trim().to_string())
    } else {
        None
    }
}

pub fn run() -> Result<()> {
    eprintln!("{}", "Anodize Environment Health Check".bold());
    eprintln!("{}", "=".repeat(40));
    eprintln!();

    let mut available_count = 0;
    let mut missing_count = 0;

    for tool in TOOLS {
        if tool_available(tool.name) {
            let version = tool_version(tool.name).unwrap_or_else(|| "unknown version".to_string());
            eprintln!(
                "  {} {:<20} {} ({})",
                "\u{2713}".green().bold(),
                tool.name,
                tool.description.dimmed(),
                version.dimmed()
            );
            available_count += 1;
        } else {
            eprintln!(
                "  {} {:<20} {}",
                "\u{2717}".red().bold(),
                tool.name,
                tool.description.dimmed()
            );
            missing_count += 1;
        }
    }

    eprintln!();
    eprintln!(
        "  {} available, {} missing",
        available_count.to_string().green().bold(),
        missing_count.to_string().yellow().bold()
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_available_cargo() {
        // cargo should always be available in a Rust project
        assert!(tool_available("cargo"), "cargo should be available");
    }

    #[test]
    fn test_tool_available_nonexistent() {
        assert!(
            !tool_available("this-tool-does-not-exist-12345"),
            "nonexistent tool should not be available"
        );
    }

    #[test]
    fn test_tool_version_cargo() {
        let version = tool_version("cargo");
        assert!(version.is_some(), "cargo --version should produce output");
        assert!(
            version.unwrap().contains("cargo"),
            "cargo version should contain 'cargo'"
        );
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
