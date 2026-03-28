use std::fs;
use std::path::Path;
use tera::{Context, Tera};

/// Run doc generation. If `check` is true, compare output against existing files
/// and return an error if they differ.
pub fn run(check: bool) -> Result<(), String> {
    let project_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .ok_or("cannot find project root")?;

    let docs_dir = project_root.join("docs/site/content/docs");
    let templates_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("templates");

    let tera = Tera::new(
        templates_dir
            .join("*.tera")
            .to_str()
            .ok_or("invalid template path")?,
    )
    .map_err(|e| format!("failed to load templates: {e}"))?;

    let cli_content = generate_cli_reference(&tera)?;
    let config_content = generate_config_reference(&tera)?;

    let cli_path = docs_dir.join("reference/cli.md");
    let config_path = docs_dir.join("reference/configuration.md");

    if check {
        let existing_cli = fs::read_to_string(&cli_path)
            .map_err(|e| format!("cannot read {}: {e}", cli_path.display()))?;
        let existing_config = fs::read_to_string(&config_path)
            .map_err(|e| format!("cannot read {}: {e}", config_path.display()))?;

        let mut stale = false;
        if existing_cli != cli_content {
            eprintln!("STALE: {}", cli_path.display());
            stale = true;
        }
        if existing_config != config_content {
            eprintln!("STALE: {}", config_path.display());
            stale = true;
        }
        if stale {
            return Err("generated docs are out of date — run `cargo xtask gen-docs`".into());
        }
        eprintln!("docs are up to date");
        return Ok(());
    }

    fs::write(&cli_path, &cli_content)
        .map_err(|e| format!("cannot write {}: {e}", cli_path.display()))?;
    eprintln!("wrote {}", cli_path.display());

    fs::write(&config_path, &config_content)
        .map_err(|e| format!("cannot write {}: {e}", config_path.display()))?;
    eprintln!("wrote {}", config_path.display());

    Ok(())
}

#[derive(serde::Serialize)]
struct ArgInfo {
    long: String,
    short: String,
    default: String,
    help: String,
}

#[derive(serde::Serialize)]
struct CmdInfo {
    name: String,
    about: String,
    args: Vec<ArgInfo>,
}

fn generate_cli_reference(tera: &Tera) -> Result<String, String> {
    let cmd = anodize_cli::build_cli();

    let about = cmd.get_about().map(|a| a.to_string()).unwrap_or_default();

    let global_args: Vec<ArgInfo> = cmd
        .get_arguments()
        .filter(|a| a.is_global_set())
        .map(|a| {
            if a.get_help().is_none()
                && let Some(long) = a.get_long()
            {
                eprintln!("warning: global flag --{long} has no help text");
            }
            ArgInfo {
                long: a.get_long().map(|l| format!("`--{l}`")).unwrap_or_default(),
                short: a
                    .get_short()
                    .map(|s| format!("`-{s}`"))
                    .unwrap_or_else(|| "\u{2014}".into()),
                default: "\u{2014}".into(),
                help: a.get_help().map(|h| h.to_string()).unwrap_or_default(),
            }
        })
        .collect();

    let commands: Vec<CmdInfo> = cmd
        .get_subcommands()
        .map(|sub| {
            let args = sub
                .get_arguments()
                .filter(|a| !a.is_global_set() && a.get_id() != "help" && a.get_id() != "version")
                .map(|a| {
                    if a.get_help().is_none() {
                        let flag = a.get_long().unwrap_or_else(|| a.get_id().as_str());
                        eprintln!("warning: {}.--{flag} has no help text", sub.get_name());
                    }
                    ArgInfo {
                        long: a
                            .get_long()
                            .map(|l| format!("`--{l}`"))
                            .unwrap_or_else(|| format!("`<{}>`", a.get_id())),
                        short: a
                            .get_short()
                            .map(|s| format!("`-{s}`"))
                            .unwrap_or_else(|| "\u{2014}".into()),
                        default: a
                            .get_default_values()
                            .first()
                            .map(|d| format!("`{}`", d.to_string_lossy()))
                            .unwrap_or_else(|| "\u{2014}".into()),
                        help: a.get_help().map(|h| h.to_string()).unwrap_or_default(),
                    }
                })
                .collect();
            CmdInfo {
                name: sub.get_name().to_string(),
                about: sub.get_about().map(|a| a.to_string()).unwrap_or_default(),
                args,
            }
        })
        .collect();

    let mut ctx = Context::new();
    ctx.insert("about", &about);
    ctx.insert("global_args", &global_args);
    ctx.insert("commands", &commands);

    tera.render("cli.md.tera", &ctx)
        .map_err(|e| format!("failed to render cli.md: {e}"))
}

#[derive(serde::Serialize)]
struct ConfigField {
    name: String,
    field_type: String,
    default: String,
    description: String,
}

#[derive(serde::Serialize)]
struct SectionLink {
    title: String,
    path: String,
    config_path: String,
}

fn generate_config_reference(tera: &Tera) -> Result<String, String> {
    // Build the config field list from the actual Config struct's fields.
    // This uses the known field names from anodize_core::Config.
    // When a field is added to Config, it must be added here too.
    // The --check flag in CI will catch drift (the generated output changes
    // when the CLI types change, even though config fields are manual).
    let top_level_fields = vec![
        ConfigField {
            name: "version".into(),
            field_type: "integer".into(),
            default: "none".into(),
            description: "Schema version (currently 1 or 2)".into(),
        },
        ConfigField {
            name: "project_name".into(),
            field_type: "string".into(),
            default: "`\"\"`".into(),
            description: "Project name used in templates".into(),
        },
        ConfigField {
            name: "dist".into(),
            field_type: "string".into(),
            default: "`./dist`".into(),
            description: "Output directory for artifacts".into(),
        },
        ConfigField {
            name: "includes".into(),
            field_type: "list".into(),
            default: "none".into(),
            description: "Config files to merge (deep merge, sequences concatenate)".into(),
        },
        ConfigField {
            name: "env_files".into(),
            field_type: "list".into(),
            default: "none".into(),
            description: "List of .env files to load before template expansion".into(),
        },
        ConfigField {
            name: "env".into(),
            field_type: "map".into(),
            default: "none".into(),
            description: "Environment variables for templates and commands".into(),
        },
        ConfigField {
            name: "report_sizes".into(),
            field_type: "bool".into(),
            default: "none".into(),
            description: "Print artifact sizes after build".into(),
        },
        ConfigField {
            name: "crates".into(),
            field_type: "list".into(),
            default: "`[]`".into(),
            description: "Crate configurations (see below)".into(),
        },
        ConfigField {
            name: "defaults".into(),
            field_type: "object".into(),
            default: "none".into(),
            description: "Default build/archive/checksum settings".into(),
        },
        ConfigField {
            name: "changelog".into(),
            field_type: "object".into(),
            default: "none".into(),
            description: "Changelog generation settings".into(),
        },
        ConfigField {
            name: "signs".into(),
            field_type: "list".into(),
            default: "`[]`".into(),
            description: "Signing configurations".into(),
        },
        ConfigField {
            name: "docker_signs".into(),
            field_type: "list".into(),
            default: "none".into(),
            description: "Docker image signing configs".into(),
        },
        ConfigField {
            name: "upx".into(),
            field_type: "list".into(),
            default: "`[]`".into(),
            description: "UPX binary compression configurations".into(),
        },
        ConfigField {
            name: "snapshot".into(),
            field_type: "object".into(),
            default: "none".into(),
            description: "Snapshot mode settings".into(),
        },
        ConfigField {
            name: "nightly".into(),
            field_type: "object".into(),
            default: "none".into(),
            description: "Nightly build settings".into(),
        },
        ConfigField {
            name: "announce".into(),
            field_type: "object".into(),
            default: "none".into(),
            description: "Announcement channels".into(),
        },
        ConfigField {
            name: "publishers".into(),
            field_type: "list".into(),
            default: "none".into(),
            description: "Custom publisher definitions".into(),
        },
        ConfigField {
            name: "tag".into(),
            field_type: "object".into(),
            default: "none".into(),
            description: "Auto-tagging configuration".into(),
        },
        ConfigField {
            name: "before".into(),
            field_type: "object".into(),
            default: "none".into(),
            description: "Pre-pipeline hooks".into(),
        },
        ConfigField {
            name: "after".into(),
            field_type: "object".into(),
            default: "none".into(),
            description: "Post-pipeline hooks".into(),
        },
        ConfigField {
            name: "workspaces".into(),
            field_type: "list".into(),
            default: "none".into(),
            description: "Monorepo workspace definitions".into(),
        },
        ConfigField {
            name: "source".into(),
            field_type: "object".into(),
            default: "none".into(),
            description: "Source archive generation settings".into(),
        },
        ConfigField {
            name: "sbom".into(),
            field_type: "object".into(),
            default: "none".into(),
            description: "Software Bill of Materials (SBOM) generation settings".into(),
        },
    ];

    let section_links = vec![
        SectionLink {
            title: "Rust Builds".into(),
            path: "@/docs/builds/rust.md".into(),
            config_path: "crates[].builds".into(),
        },
        SectionLink {
            title: "Archives".into(),
            path: "@/docs/packages/archives.md".into(),
            config_path: "crates[].archives".into(),
        },
        SectionLink {
            title: "Checksums".into(),
            path: "@/docs/packages/checksums.md".into(),
            config_path: "defaults.checksum / crates[].checksum".into(),
        },
        SectionLink {
            title: "GitHub Releases".into(),
            path: "@/docs/publish/github.md".into(),
            config_path: "crates[].release".into(),
        },
        SectionLink {
            title: "Homebrew".into(),
            path: "@/docs/publish/homebrew.md".into(),
            config_path: "crates[].publish.homebrew".into(),
        },
        SectionLink {
            title: "Scoop".into(),
            path: "@/docs/publish/scoop.md".into(),
            config_path: "crates[].publish.scoop".into(),
        },
        SectionLink {
            title: "crates.io".into(),
            path: "@/docs/publish/crates-io.md".into(),
            config_path: "crates[].publish.crates".into(),
        },
        SectionLink {
            title: "Docker".into(),
            path: "@/docs/packages/docker.md".into(),
            config_path: "crates[].docker".into(),
        },
        SectionLink {
            title: "nFPM".into(),
            path: "@/docs/packages/nfpm.md".into(),
            config_path: "crates[].nfpm".into(),
        },
        SectionLink {
            title: "Signing".into(),
            path: "@/docs/sign/binaries-archives.md".into(),
            config_path: "signs".into(),
        },
        SectionLink {
            title: "Changelog".into(),
            path: "@/docs/more/changelog.md".into(),
            config_path: "changelog".into(),
        },
        SectionLink {
            title: "Announce".into(),
            path: "@/docs/announce/discord.md".into(),
            config_path: "announce".into(),
        },
        SectionLink {
            title: "Auto-Tagging".into(),
            path: "@/docs/advanced/auto-tagging.md".into(),
            config_path: "tag".into(),
        },
        SectionLink {
            title: "Global Hooks".into(),
            path: "@/docs/general/hooks.md".into(),
            config_path: "before / after".into(),
        },
    ];

    let mut ctx = Context::new();
    ctx.insert("top_level_fields", &top_level_fields);
    ctx.insert("section_links", &section_links);

    tera.render("configuration.md.tera", &ctx)
        .map_err(|e| format!("failed to render configuration.md: {e}"))
}
