use std::process::Command;

use anyhow::{Context as _, Result, bail};
use anodize_core::artifact::{Artifact, ArtifactKind};
use anodize_core::config::PublisherConfig;
use anodize_core::template::{self, TemplateVars};

/// Run all configured publishers against matching artifacts.
///
/// For each publisher, filter the artifact set by `ids` and `artifact_types`,
/// then render and execute the command for each matching artifact.
/// In dry-run mode, the command is logged but not executed.
pub fn run_publishers(
    publishers: &[PublisherConfig],
    artifacts: &[Artifact],
    base_vars: &TemplateVars,
    dry_run: bool,
) -> Result<()> {
    for (i, publisher) in publishers.iter().enumerate() {
        let default_label = format!("publisher[{}]", i);
        let label = publisher
            .name
            .as_deref()
            .unwrap_or(&default_label);

        if publisher.cmd.is_empty() {
            eprintln!("  [publisher] skipping {} — empty cmd", label);
            continue;
        }

        let matching: Vec<&Artifact> = artifacts
            .iter()
            .filter(|a| matches_publisher_filter(a, publisher))
            .collect();

        if matching.is_empty() {
            eprintln!("  [publisher] {} — no matching artifacts", label);
            continue;
        }

        for artifact in &matching {
            let (rendered_cmd, rendered_args) =
                build_publisher_command(&publisher.cmd, publisher.args.as_deref(), artifact, base_vars)
                    .with_context(|| {
                        format!("failed to render publisher command for {}", label)
                    })?;

            if dry_run {
                let full_cmd = format_command_line(&rendered_cmd, &rendered_args);
                eprintln!("  [dry-run] [publisher] {} — {}", label, full_cmd);
            } else {
                eprintln!(
                    "  [publisher] {} — running for {}",
                    label,
                    artifact.path.display()
                );
                let mut cmd = Command::new("sh");
                cmd.arg("-c");

                // Build the full shell command string
                let full_cmd = format_command_line(&rendered_cmd, &rendered_args);
                cmd.arg(&full_cmd);

                // Apply publisher-specific env vars
                if let Some(ref env_map) = publisher.env {
                    for (k, v) in env_map {
                        cmd.env(k, v);
                    }
                }

                let status = cmd
                    .status()
                    .with_context(|| format!("failed to spawn publisher command: {}", full_cmd))?;

                if !status.success() {
                    bail!(
                        "publisher {} failed (exit {}): {}",
                        label,
                        status.code().unwrap_or(-1),
                        full_cmd
                    );
                }
            }
        }
    }
    Ok(())
}

/// Check whether an artifact matches a publisher's filter criteria.
///
/// An artifact matches when:
/// - If `ids` is set, the artifact's metadata `"id"` value must be in the list.
/// - If `artifact_types` is set, the artifact's kind (as snake_case string)
///   must be in the list.
/// - If neither filter is set, all non-metadata artifacts match.
pub fn matches_publisher_filter(artifact: &Artifact, publisher: &PublisherConfig) -> bool {
    // Never publish metadata artifacts through custom publishers
    if artifact.kind == ArtifactKind::Metadata {
        return false;
    }

    // Check ids filter
    if let Some(ref ids) = publisher.ids {
        let artifact_id = artifact.metadata.get("id");
        match artifact_id {
            Some(id) if ids.iter().any(|allowed| allowed == id) => {}
            _ => return false,
        }
    }

    // Check artifact_types filter
    if let Some(ref types) = publisher.artifact_types
        && !types.iter().any(|t| t == artifact.kind.as_str())
    {
        return false;
    }

    true
}

/// Render the publisher command and args by substituting template variables
/// plus artifact-specific variables.
///
/// Artifact-scoped variables added:
/// - `ArtifactPath` — absolute path to the artifact file
/// - `ArtifactName` — file name of the artifact
/// - `ArtifactKind` — snake_case artifact kind string
pub fn build_publisher_command(
    cmd: &str,
    args: Option<&[String]>,
    artifact: &Artifact,
    base_vars: &TemplateVars,
    ) -> Result<(String, Vec<String>)> {
    // Clone the base vars and add artifact-scoped variables
    let mut vars = clone_template_vars(base_vars);
    vars.set("ArtifactPath", &artifact.path.to_string_lossy());
    vars.set(
        "ArtifactName",
        artifact
            .path
            .file_name()
            .map(|n| n.to_string_lossy())
            .as_deref()
            .unwrap_or(""),
    );
    vars.set("ArtifactKind", artifact.kind.as_str());

    // Also expose artifact metadata entries as template vars under the same key
    for (k, v) in &artifact.metadata {
        vars.set(k, v);
    }

    let rendered_cmd = template::render(cmd, &vars)
        .with_context(|| format!("failed to render publisher cmd: {}", cmd))?;

    let rendered_args = match args {
        Some(args) => {
            let mut out = Vec::with_capacity(args.len());
            for arg in args {
                let rendered = template::render(arg, &vars)
                    .with_context(|| format!("failed to render publisher arg: {}", arg))?;
                out.push(rendered);
            }
            out
        }
        None => Vec::new(),
    };

    Ok((rendered_cmd, rendered_args))
}

/// Format a command with its arguments into a single shell command string.
fn format_command_line(cmd: &str, args: &[String]) -> String {
    if args.is_empty() {
        cmd.to_string()
    } else {
        format!("{} {}", cmd, args.join(" "))
    }
}

/// Create a new `TemplateVars` from an existing one by copying all accessible
/// variables. Since TemplateVars doesn't expose iteration, we build a new
/// instance with the known keys. In practice the base_vars are from the
/// Context, which sets a known set of keys.
///
/// We use a simpler approach: create a fresh TemplateVars. The caller adds
/// artifact-specific vars on top. The base vars from the context's template
/// engine are passed through as-is by using the render function that already
/// has access to them.
///
/// Actually, since we need to use `template::render` with the merged vars,
/// and TemplateVars doesn't support cloning, we take a different approach:
/// we accept the base_vars by reference and create a new TemplateVars that
/// the caller populates.
fn clone_template_vars(base: &TemplateVars) -> TemplateVars {
    // TemplateVars doesn't expose iteration, but we know the common keys.
    // We'll copy known keys that are typically set by Context.
    let mut vars = TemplateVars::new();
    let known_keys = [
        "ProjectName",
        "Tag",
        "Version",
        "RawVersion",
        "Major",
        "Minor",
        "Patch",
        "Prerelease",
        "FullCommit",
        "Commit",
        "ShortCommit",
        "Branch",
        "CommitDate",
        "CommitTimestamp",
        "IsGitDirty",
        "GitTreeState",
        "IsSnapshot",
        "IsDraft",
        "PreviousTag",
        "Date",
        "Timestamp",
        "Now",
    ];
    for key in &known_keys {
        if let Some(val) = base.get(key) {
            vars.set(key, val);
        }
    }
    vars
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn make_artifact(kind: ArtifactKind, path: &str, id: Option<&str>) -> Artifact {
        let mut metadata = HashMap::new();
        if let Some(id_val) = id {
            metadata.insert("id".to_string(), id_val.to_string());
        }
        Artifact {
            kind,
            path: PathBuf::from(path),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata,
        }
    }

    fn make_publisher(
        cmd: &str,
        ids: Option<Vec<&str>>,
        artifact_types: Option<Vec<&str>>,
    ) -> PublisherConfig {
        PublisherConfig {
            name: Some("test-publisher".to_string()),
            cmd: cmd.to_string(),
            args: None,
            ids: ids.map(|v| v.into_iter().map(|s| s.to_string()).collect()),
            artifact_types: artifact_types.map(|v| v.into_iter().map(|s| s.to_string()).collect()),
            env: None,
        }
    }

    fn base_vars() -> TemplateVars {
        let mut vars = TemplateVars::new();
        vars.set("ProjectName", "myapp");
        vars.set("Version", "1.0.0");
        vars
    }

    // --- Artifact filtering tests ---

    #[test]
    fn test_filter_matches_all_non_metadata_when_no_filters() {
        let publisher = make_publisher("echo", None, None);

        let binary = make_artifact(ArtifactKind::Binary, "dist/myapp", None);
        let archive = make_artifact(ArtifactKind::Archive, "dist/myapp.tar.gz", None);
        let checksum = make_artifact(ArtifactKind::Checksum, "dist/checksums.sha256", None);
        let metadata = make_artifact(ArtifactKind::Metadata, "dist/metadata.json", None);

        assert!(matches_publisher_filter(&binary, &publisher));
        assert!(matches_publisher_filter(&archive, &publisher));
        assert!(matches_publisher_filter(&checksum, &publisher));
        assert!(
            !matches_publisher_filter(&metadata, &publisher),
            "metadata artifacts should never match"
        );
    }

    #[test]
    fn test_filter_by_ids() {
        let publisher = make_publisher("echo", Some(vec!["linux-amd64"]), None);

        let matching = make_artifact(ArtifactKind::Archive, "dist/a.tar.gz", Some("linux-amd64"));
        let non_matching =
            make_artifact(ArtifactKind::Archive, "dist/b.tar.gz", Some("darwin-arm64"));
        let no_id = make_artifact(ArtifactKind::Archive, "dist/c.tar.gz", None);

        assert!(matches_publisher_filter(&matching, &publisher));
        assert!(!matches_publisher_filter(&non_matching, &publisher));
        assert!(
            !matches_publisher_filter(&no_id, &publisher),
            "artifact without id should not match when ids filter is set"
        );
    }

    #[test]
    fn test_filter_by_artifact_types() {
        let publisher = make_publisher("echo", None, Some(vec!["archive", "checksum"]));

        let archive = make_artifact(ArtifactKind::Archive, "dist/a.tar.gz", None);
        let checksum = make_artifact(ArtifactKind::Checksum, "dist/checksums.sha256", None);
        let binary = make_artifact(ArtifactKind::Binary, "dist/myapp", None);
        let docker = make_artifact(ArtifactKind::DockerImage, "myapp:latest", None);

        assert!(matches_publisher_filter(&archive, &publisher));
        assert!(matches_publisher_filter(&checksum, &publisher));
        assert!(!matches_publisher_filter(&binary, &publisher));
        assert!(!matches_publisher_filter(&docker, &publisher));
    }

    #[test]
    fn test_filter_by_ids_and_artifact_types_combined() {
        let publisher = make_publisher(
            "echo",
            Some(vec!["linux-amd64"]),
            Some(vec!["archive"]),
        );

        // Matches both filters
        let good =
            make_artifact(ArtifactKind::Archive, "dist/a.tar.gz", Some("linux-amd64"));
        assert!(matches_publisher_filter(&good, &publisher));

        // Right type but wrong id
        let wrong_id =
            make_artifact(ArtifactKind::Archive, "dist/b.tar.gz", Some("darwin-arm64"));
        assert!(!matches_publisher_filter(&wrong_id, &publisher));

        // Right id but wrong type
        let wrong_type =
            make_artifact(ArtifactKind::Binary, "dist/myapp", Some("linux-amd64"));
        assert!(!matches_publisher_filter(&wrong_type, &publisher));
    }

    // --- Command construction tests ---

    #[test]
    fn test_build_command_renders_artifact_vars() {
        let vars = base_vars();
        let artifact = make_artifact(ArtifactKind::Archive, "/dist/myapp-1.0.0.tar.gz", None);

        let (cmd, args) = build_publisher_command(
            "curl -F 'file=@{{ ArtifactPath }}'",
            Some(&["--header".to_string(), "X-Name: {{ ArtifactName }}".to_string()]),
            &artifact,
            &vars,
        )
        .unwrap();

        assert_eq!(cmd, "curl -F 'file=@/dist/myapp-1.0.0.tar.gz'");
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], "--header");
        assert_eq!(args[1], "X-Name: myapp-1.0.0.tar.gz");
    }

    #[test]
    fn test_build_command_renders_project_vars() {
        let vars = base_vars();
        let artifact = make_artifact(ArtifactKind::Binary, "/dist/myapp", None);

        let (cmd, _) = build_publisher_command(
            "upload --project {{ ProjectName }} --version {{ Version }} {{ ArtifactPath }}",
            None,
            &artifact,
            &vars,
        )
        .unwrap();

        assert_eq!(cmd, "upload --project myapp --version 1.0.0 /dist/myapp");
    }

    #[test]
    fn test_build_command_renders_artifact_kind() {
        let vars = base_vars();
        let artifact = make_artifact(ArtifactKind::LinuxPackage, "/dist/myapp.deb", None);

        let (cmd, _) = build_publisher_command(
            "echo {{ ArtifactKind }}",
            None,
            &artifact,
            &vars,
        )
        .unwrap();

        assert_eq!(cmd, "echo linux_package");
    }

    // --- Dry-run behavior test ---

    #[test]
    fn test_dry_run_does_not_execute() {
        let vars = base_vars();
        let artifacts = vec![
            make_artifact(ArtifactKind::Archive, "/dist/myapp.tar.gz", None),
        ];
        let publishers = vec![PublisherConfig {
            name: Some("test".to_string()),
            cmd: "this-command-does-not-exist --should-not-run".to_string(),
            args: None,
            ids: None,
            artifact_types: None,
            env: None,
        }];

        // In dry-run mode, the command is never executed, so a non-existent
        // command should not cause an error.
        let result = run_publishers(&publishers, &artifacts, &vars, true);
        assert!(result.is_ok(), "dry-run should not execute commands: {:?}", result.err());
    }

    // --- Empty publishers is a no-op ---

    #[test]
    fn test_empty_publishers_is_noop() {
        let vars = base_vars();
        let artifacts = vec![
            make_artifact(ArtifactKind::Binary, "/dist/myapp", None),
        ];

        let result = run_publishers(&[], &artifacts, &vars, false);
        assert!(result.is_ok());
    }

    // --- Empty cmd is skipped ---

    #[test]
    fn test_empty_cmd_is_skipped() {
        let vars = base_vars();
        let artifacts = vec![
            make_artifact(ArtifactKind::Binary, "/dist/myapp", None),
        ];
        let publishers = vec![PublisherConfig {
            name: Some("empty".to_string()),
            cmd: String::new(),
            args: None,
            ids: None,
            artifact_types: None,
            env: None,
        }];

        let result = run_publishers(&publishers, &artifacts, &vars, false);
        assert!(result.is_ok());
    }

    // --- format_command_line tests ---

    #[test]
    fn test_format_command_line_no_args() {
        assert_eq!(format_command_line("echo hello", &[]), "echo hello");
    }

    #[test]
    fn test_format_command_line_with_args() {
        let args = vec!["--flag".to_string(), "value".to_string()];
        assert_eq!(format_command_line("cmd", &args), "cmd --flag value");
    }

    // --- Config parsing test ---

    #[test]
    fn test_publisher_config_yaml_parsing() {
        use anodize_core::config::Config;

        let yaml = r#"
project_name: test
publishers:
  - name: upload-s3
    cmd: "aws s3 cp {{ ArtifactPath }} s3://my-bucket/"
    artifact_types:
      - archive
      - checksum
    env:
      AWS_REGION: us-east-1
  - name: notify
    cmd: "curl -X POST https://hooks.example.com/release"
    ids:
      - linux-amd64
      - darwin-arm64
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let publishers = config.publishers.as_ref().unwrap();
        assert_eq!(publishers.len(), 2);

        let p0 = &publishers[0];
        assert_eq!(p0.name, Some("upload-s3".to_string()));
        assert!(p0.cmd.contains("aws s3 cp"));
        assert_eq!(
            p0.artifact_types.as_ref().unwrap(),
            &["archive", "checksum"]
        );
        assert_eq!(
            p0.env.as_ref().unwrap().get("AWS_REGION").unwrap(),
            "us-east-1"
        );
        assert!(p0.ids.is_none());

        let p1 = &publishers[1];
        assert_eq!(p1.name, Some("notify".to_string()));
        assert_eq!(
            p1.ids.as_ref().unwrap(),
            &["linux-amd64", "darwin-arm64"]
        );
        assert!(p1.artifact_types.is_none());
    }

    #[test]
    fn test_publisher_config_toml_parsing() {
        use anodize_core::config::Config;

        let toml_str = r#"
project_name = "test"

[[publishers]]
name = "upload"
cmd = "upload {{ ArtifactPath }}"
artifact_types = ["archive"]

[[publishers]]
name = "notify"
cmd = "notify"
ids = ["linux-amd64"]

[[crates]]
name = "a"
path = "."
tag_template = "v{{ .Version }}"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let publishers = config.publishers.as_ref().unwrap();
        assert_eq!(publishers.len(), 2);
        assert_eq!(publishers[0].name, Some("upload".to_string()));
        assert_eq!(publishers[1].ids.as_ref().unwrap(), &["linux-amd64"]);
    }

    #[test]
    fn test_publishers_omitted_is_none() {
        use anodize_core::config::Config;

        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(config.publishers.is_none());
    }
}
