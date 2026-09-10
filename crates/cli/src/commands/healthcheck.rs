use anodizer_core::log::{StageLogger, Verbosity};
use anyhow::Result;
use colored::Colorize;

/// Tool entry with name and description for display.
struct ToolCheck {
    name: &'static str,
    description: &'static str,
    /// Lowest version the tool may report and still be usable, when the
    /// tool has a floor at all. `None` means any version that runs is fine.
    min_version: Option<&'static str>,
}

const TOOLS: &[ToolCheck] = &[
    ToolCheck {
        name: "cargo",
        description: "Rust package manager",
        min_version: None,
    },
    ToolCheck {
        name: "git",
        description: "Version control (2.13 or newer)",
        min_version: Some("2.13"),
    },
    ToolCheck {
        name: "docker",
        description: "Container runtime",
        min_version: None,
    },
    ToolCheck {
        name: "podman",
        description: "Container runtime (Linux-only alt backend)",
        min_version: None,
    },
    ToolCheck {
        name: "nfpm",
        description: "Linux package builder (deb/rpm/apk)",
        min_version: None,
    },
    ToolCheck {
        name: "cargo-zigbuild",
        description: "Cross-compilation via Zig",
        min_version: None,
    },
    ToolCheck {
        name: "zig",
        description: "Zig toolchain (linker/libc behind cargo-zigbuild)",
        min_version: None,
    },
    ToolCheck {
        name: "cross",
        description: "Cross-compilation via Docker",
        min_version: None,
    },
    ToolCheck {
        name: "gpg",
        description: "GNU Privacy Guard (signing)",
        min_version: None,
    },
    ToolCheck {
        name: "cosign",
        description: "Sigstore container signing",
        min_version: None,
    },
    ToolCheck {
        name: "aws",
        description: "AWS CLI (S3 blob storage)",
        min_version: None,
    },
    ToolCheck {
        name: "gsutil",
        description: "Google Cloud Storage CLI",
        min_version: None,
    },
    ToolCheck {
        name: "az",
        description: "Azure CLI (Blob storage)",
        min_version: None,
    },
];

use anodizer_core::tool_detect::{ToolProbe, runs, tool_version};

/// The dotted version a probe line reports, as a comparable [`semver::Version`].
///
/// Takes the first whitespace-separated token that starts with a digit and
/// reads its leading `<digits>(.<digits>)*` run, so `git version 2.39.5 (Apple
/// Git-154)` and `git version 2.13.windows.1` both yield `2.39.5` / `2.13.0`.
/// A run shorter than three components is padded with zeros — a tool that
/// reports `2.13` means `2.13.0`, and semver refuses to parse the short form.
/// `None` when no token looks like a version.
fn parse_tool_version(line: &str) -> Option<semver::Version> {
    for token in line.split_whitespace() {
        let run: String = token
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        let mut parts: Vec<&str> = run.trim_end_matches('.').split('.').collect();
        if parts.iter().any(|p| p.is_empty()) {
            continue;
        }
        parts.truncate(3);
        let mut padded: Vec<String> = parts.iter().map(|p| (*p).to_string()).collect();
        while padded.len() < 3 {
            padded.push("0".to_string());
        }
        if let Ok(v) = semver::Version::parse(&padded.join(".")) {
            return Some(v);
        }
    }
    None
}

/// Whether a probed version line puts the tool below its declared floor.
///
/// A tool with no floor is never below one, and neither is a tool whose
/// version line does not parse: an unreadable line is not evidence of an old
/// tool, and reporting one as outdated would send the operator to upgrade
/// something that is already current.
fn below_min_version(min_version: Option<&str>, version_line: Option<&str>) -> bool {
    let (Some(min), Some(line)) = (min_version, version_line) else {
        return false;
    };
    match (parse_tool_version(min), parse_tool_version(line)) {
        (Some(floor), Some(found)) => found < floor,
        _ => false,
    }
}

pub fn run() -> Result<()> {
    let log = StageLogger::new("healthcheck", Verbosity::Normal);

    log.status(&format!("{}", "Anodizer Environment Health Check".bold()));
    log.status(&"=".repeat(40));

    let mut available_count = 0;
    let mut missing_count = 0;
    let mut outdated_count = 0;
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
                // A tool that runs but reports a version below its floor is
                // not available for anodizer's purposes; reporting it with a
                // green tick sends the operator away from the real cause.
                let outdated = below_min_version(tool.min_version, version.as_deref());
                let parenthetical = version
                    .map(|v| format!(" ({})", v.dimmed()))
                    .unwrap_or_default();
                if outdated {
                    log.status(&format!(
                        "{} {:<20} {}{} \u{2014} below the {} floor",
                        "\u{2717}".red().bold(),
                        tool.name,
                        tool.description.dimmed(),
                        parenthetical,
                        tool.min_version.unwrap_or_default()
                    ));
                    outdated_count += 1;
                } else {
                    log.status(&format!(
                        "{} {:<20} {}{}",
                        "\u{2713}".green().bold(),
                        tool.name,
                        tool.description.dimmed(),
                        parenthetical
                    ));
                    available_count += 1;
                }
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
    if outdated_count > 0 {
        summary.push_str(&format!(
            ", {} below floor",
            outdated_count.to_string().red().bold()
        ));
    }
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
    fn a_git_below_the_floor_is_reported_below_it_and_a_newer_one_is_not() {
        // The floor is 2.13 because `git describe --exclude` first shipped
        // there. A 2.9 git runs fine and fails only later, inside previous-tag
        // discovery, so the health report must name it here.
        assert!(
            below_min_version(Some("2.13"), Some("git version 2.9.5")),
            "2.9.5 is below the 2.13 floor"
        );
        assert!(
            !below_min_version(Some("2.13"), Some("git version 2.13.0")),
            "the floor itself is not below the floor"
        );
        assert!(
            !below_min_version(Some("2.13"), Some("git version 2.39.5 (Apple Git-154)")),
            "a trailing vendor suffix must not defeat the comparison"
        );
        assert!(
            !below_min_version(Some("2.13"), Some("git version 2.13.windows.1")),
            "a non-numeric third component reads as 2.13, which meets the floor"
        );
        assert!(
            !below_min_version(None, Some("nfpm version v2.35.3")),
            "a tool with no declared floor is never below one"
        );
        assert!(
            !below_min_version(Some("2.13"), Some("no digits here")),
            "an unparseable line is not evidence of an old tool"
        );
    }

    #[test]
    fn the_git_row_declares_the_floor_its_description_states() {
        let git = TOOLS
            .iter()
            .find(|t| t.name == "git")
            .expect("TOOLS carries a git row");
        assert_eq!(
            git.min_version,
            Some("2.13"),
            "the floor the description spells out must also be declared, or \
             nothing compares against it"
        );
        assert!(
            git.description.contains("2.13"),
            "the description and the declared floor must name the same version; \
             got {}",
            git.description
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
