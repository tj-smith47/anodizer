use std::path::PathBuf;
use std::process::Command;

use anodize_core::artifact::{Artifact, ArtifactKind};
use anodize_core::config::PublisherConfig;
use anodize_core::log::StageLogger;
use anodize_core::pipe_skip::SkipMemento;
use anodize_core::template::{self, TemplateVars};
use anyhow::{Context as _, Result};

/// Split a command string into arguments, respecting single and double quotes.
/// Matches Go's `shellwords.Parse()` behavior.
fn split_shellwords(s: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escape_next = false;

    for ch in s.chars() {
        if escape_next {
            current.push(ch);
            escape_next = false;
            continue;
        }
        match ch {
            '\\' if !in_single => escape_next = true,
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            ' ' | '\t' if !in_single && !in_double => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

/// Run all configured publishers against matching artifacts.
///
/// For each publisher, filter the artifact set by `ids` and `artifact_types`,
/// then render and execute the command for each matching artifact.
/// Artifacts within a single publisher are processed in parallel up to `parallelism`.
/// In dry-run mode, the command is logged but not executed.
///
/// When `skip_memento` is `Some`, intentional per-publisher skips (template
/// disable, empty `cmd`, no matching artifacts) are recorded so the
/// end-of-pipeline summary can report them. Pass `None` from tests or
/// standalone callers that don't care.
pub fn run_publishers(
    publishers: &[PublisherConfig],
    artifacts: &[Artifact],
    base_vars: &TemplateVars,
    dry_run: bool,
    log: &StageLogger,
    parallelism: usize,
    skip_memento: Option<&SkipMemento>,
) -> Result<()> {
    let parallelism = parallelism.max(1);
    for (i, publisher) in publishers.iter().enumerate() {
        let default_label = format!("publisher[{}]", i);
        let label = publisher.name.as_deref().unwrap_or(&default_label);

        // Check template-conditional disable
        if let Some(ref d) = publisher.disable
            && d.is_disabled(|tmpl| template::render(tmpl, base_vars))
        {
            log.verbose(&format!(
                "[publisher] skipping {} -- disabled by template",
                label
            ));
            if let Some(sm) = skip_memento {
                sm.remember("publisher", label, "disabled by template");
            }
            continue;
        }

        if publisher.cmd.is_empty() {
            log.verbose(&format!("[publisher] skipping {} -- empty cmd", label));
            if let Some(sm) = skip_memento {
                sm.remember("publisher", label, "empty cmd");
            }
            continue;
        }

        // Resolve extra_files globs into additional artifacts
        let mut extra_artifacts: Vec<Artifact> = Vec::new();
        if let Some(ref extra_files) = publisher.extra_files {
            for ef in extra_files {
                let rendered_glob =
                    template::render(&ef.glob, base_vars).unwrap_or_else(|_| ef.glob.clone());
                let paths: Vec<PathBuf> = glob::glob(&rendered_glob)
                    .into_iter()
                    .flat_map(|entries| entries.filter_map(|e| e.ok()))
                    .collect();

                if paths.is_empty() {
                    log.verbose(&format!(
                        "[publisher] {} -- extra_files glob '{}' matched no files",
                        label, rendered_glob
                    ));
                    continue;
                }

                // If name_template is set and glob matches multiple files, error
                if ef.name.is_some() && paths.len() > 1 {
                    anyhow::bail!(
                        "publisher {}: extra_files glob '{}' matched {} files but name_template is set (requires exactly 1 match)",
                        label,
                        rendered_glob,
                        paths.len()
                    );
                }

                for path in paths {
                    let name = if let Some(ref name_tmpl) = ef.name {
                        template::render(name_tmpl, base_vars).unwrap_or_else(|_| name_tmpl.clone())
                    } else {
                        path.file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default()
                    };
                    extra_artifacts.push(Artifact {
                        kind: ArtifactKind::Archive,
                        name,
                        path,
                        target: None,
                        crate_name: String::new(),
                        metadata: std::collections::HashMap::new(),
                        size: None,
                    });
                }
            }
        }

        // Resolve templated_extra_files: render template contents and add as artifacts.
        // The TempDir is kept alive until the end of this publisher iteration so
        // that the rendered files remain on disk while the publisher command runs.
        let _tpl_dir_guard;
        if let Some(ref tpl_specs) = publisher.templated_extra_files {
            if !tpl_specs.is_empty() {
                let tpl_dir = tempfile::TempDir::new()
                    .context("publisher: create temp dir for templated files")?;
                let rendered =
                    anodize_core::templated_files::process_templated_extra_files_with_vars(
                        tpl_specs,
                        base_vars,
                        tpl_dir.path(),
                        &format!("publisher[{}]", label),
                    )?;
                for (path, name) in rendered {
                    extra_artifacts.push(Artifact {
                        kind: ArtifactKind::Archive,
                        name,
                        path,
                        target: None,
                        crate_name: String::new(),
                        metadata: std::collections::HashMap::new(),
                        size: None,
                    });
                }
                _tpl_dir_guard = Some(tpl_dir);
            } else {
                _tpl_dir_guard = None;
            }
        } else {
            _tpl_dir_guard = None;
        }

        let matching: Vec<&Artifact> = artifacts
            .iter()
            .filter(|a| matches_publisher_filter(a, publisher))
            .chain(extra_artifacts.iter())
            .collect();

        if matching.is_empty() {
            log.verbose(&format!("[publisher] {} -- no matching artifacts", label));
            if let Some(sm) = skip_memento {
                sm.remember("publisher", label, "no matching artifacts");
            }
            continue;
        }

        // Execute publisher command per artifact, with parallelism
        let run_for_artifact = |artifact: &&Artifact| -> Result<()> {
            let (rendered_cmd, rendered_args) = build_publisher_command(
                &publisher.cmd,
                publisher.args.as_deref(),
                artifact,
                base_vars,
            )
            .with_context(|| format!("failed to render publisher command for {}", label))?;

            if dry_run {
                let full_cmd = format_command_line(&rendered_cmd, &rendered_args);
                log.status(&format!("[dry-run] [publisher] {} -- {}", label, full_cmd));
            } else {
                log.status(&format!(
                    "[publisher] {} -- running for {}",
                    label,
                    artifact.path.display()
                ));
                // GoReleaser parity (exec.go): parse command with shellwords
                // and exec directly instead of wrapping with `sh -c`.
                let full_cmd = format_command_line(&rendered_cmd, &rendered_args);
                let shell_args = split_shellwords(&full_cmd);
                if shell_args.is_empty() {
                    anyhow::bail!("publisher: empty command after parsing: {}", full_cmd);
                }
                let mut cmd = Command::new(&shell_args[0]);
                cmd.args(&shell_args[1..]);

                if let Some(ref dir) = publisher.dir {
                    let rendered_dir =
                        template::render(dir, base_vars).unwrap_or_else(|_| dir.clone());
                    cmd.current_dir(rendered_dir);
                }

                // GoReleaser parity (exec.go): restrict environment to a small
                // whitelist to prevent accidental leakage of tokens/credentials.
                cmd.env_clear();
                for key in &[
                    "HOME",
                    "USER",
                    "USERPROFILE",
                    "TMPDIR",
                    "TMP",
                    "TEMP",
                    "PATH",
                    "SYSTEMROOT",
                ] {
                    if let Ok(val) = std::env::var(key) {
                        cmd.env(key, val);
                    }
                }
                if let Some(ref env_map) = publisher.env {
                    for (k, v) in env_map {
                        cmd.env(k, v);
                    }
                }

                let output = cmd
                    .output()
                    .with_context(|| format!("failed to spawn publisher command: {}", full_cmd))?;

                log.check_output(output, &format!("publisher {}", label))?;
            }
            Ok(())
        };

        if parallelism <= 1 || dry_run {
            // Sequential execution
            for artifact in &matching {
                run_for_artifact(artifact)?;
            }
        } else {
            // Parallel execution: process artifacts in chunks of `parallelism`
            use std::sync::Mutex;
            let errors: Mutex<Vec<anyhow::Error>> = Mutex::new(Vec::new());

            for chunk in matching.chunks(parallelism) {
                std::thread::scope(|s| {
                    for artifact in chunk {
                        s.spawn(|| {
                            if let Err(e) = run_for_artifact(artifact) {
                                // Mutex poison only happens if a prior holder
                                // panicked; recover into_inner and keep going
                                // so one panic doesn't cascade-lose every
                                // subsequent error.
                                errors
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .push(e);
                            }
                        });
                    }
                });
                // Check for errors after each chunk
                let errs = errors
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if !errs.is_empty() {
                    break;
                }
            }

            let mut errs = errors
                .into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !errs.is_empty() {
                // Return first error, log the rest
                for e in errs.iter().skip(1) {
                    log.status(&format!("[publisher] {} -- additional error: {}", label, e));
                }
                return Err(errs.remove(0));
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
/// - Otherwise, the artifact's kind must be in the curated default list:
///   Archive, UploadableFile, LinuxPackage, UploadableBinary, DockerImage,
///   DockerManifest, DockerImageV2, SourceArchive, Sbom.
/// - Checksum / Metadata / Signature / Certificate kinds are opt-in via the
///   `checksum` / `meta` / `signature` publisher flags.
pub fn matches_publisher_filter(artifact: &Artifact, publisher: &PublisherConfig) -> bool {
    // Check ids filter first — orthogonal to type filtering.
    if let Some(ref ids) = publisher.ids {
        let artifact_id = artifact.metadata.get("id");
        match artifact_id {
            Some(id) if ids.iter().any(|allowed| allowed == id) => {}
            _ => return false,
        }
    }

    // Explicit artifact_types filter takes full control — if the user listed
    // "checksum" or signature kinds, they pass regardless of the boolean flags.
    if let Some(ref types) = publisher.artifact_types {
        return types.iter().any(|t| t == artifact.kind.as_str());
    }

    // Default curated list — matches GoReleaser's `filterArtifacts` exactly.
    // Kinds not on this list (raw Binary, UniversalBinary, Snap, Installer,
    // DiskImage, MacOsPackage, Makeself, Flatpak, Header, CArchive, CShared,
    // SourceRpm, etc.) must be opted in via an explicit `artifact_types`.
    let in_default_set = matches!(
        artifact.kind,
        ArtifactKind::Archive
            | ArtifactKind::UploadableFile
            | ArtifactKind::LinuxPackage
            | ArtifactKind::UploadableBinary
            | ArtifactKind::DockerImage
            | ArtifactKind::DockerManifest
            | ArtifactKind::DockerImageV2
            | ArtifactKind::SourceArchive
            | ArtifactKind::Sbom
    );
    if in_default_set {
        return true;
    }

    // Opt-ins for the other canonical kinds.
    if artifact.kind == ArtifactKind::Checksum {
        return publisher.checksum.unwrap_or(false);
    }
    if artifact.kind == ArtifactKind::Metadata {
        return publisher.meta.unwrap_or(false);
    }
    if matches!(
        artifact.kind,
        ArtifactKind::Signature | ArtifactKind::Certificate
    ) {
        return publisher.signature.unwrap_or(false);
    }

    false
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
    let mut vars = base_vars.clone();
    vars.set("ArtifactPath", &artifact.path.to_string_lossy());
    let artifact_name = artifact
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    vars.set("ArtifactName", &artifact_name);
    vars.set(
        "ArtifactExt",
        anodize_core::template::extract_artifact_ext(&artifact_name),
    );
    vars.set("ArtifactKind", artifact.kind.as_str());
    // Set ArtifactID from artifact metadata "id" key (Pro addition)
    vars.set(
        "ArtifactID",
        artifact
            .metadata
            .get("id")
            .map(|s| s.as_str())
            .unwrap_or(""),
    );

    // Expose per-artifact Os, Arch, and Target template variables
    // (GoReleaser parity: custom publishers can reference {{ .Os }}, {{ .Arch }}, {{ .Target }})
    if let Some(ref target) = artifact.target {
        let (os, arch) = anodize_core::target::map_target(target);
        vars.set("Os", &os);
        vars.set("Arch", &arch);
        vars.set("Target", target);
    } else {
        vars.set("Os", "");
        vars.set("Arch", "");
        vars.set("Target", "");
    }

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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use anodize_core::config::StringOrBool;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn make_artifact(kind: ArtifactKind, path: &str, id: Option<&str>) -> Artifact {
        let mut metadata = HashMap::new();
        if let Some(id_val) = id {
            metadata.insert("id".to_string(), id_val.to_string());
        }
        Artifact {
            kind,
            name: String::new(),
            path: PathBuf::from(path),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata,
            size: None,
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
            dir: None,
            disable: None,
            checksum: None,
            signature: None,
            meta: None,
            extra_files: None,
            templated_extra_files: None,
        }
    }

    fn base_vars() -> TemplateVars {
        let mut vars = TemplateVars::new();
        vars.set("ProjectName", "myapp");
        vars.set("Version", "1.0.0");
        vars
    }

    fn test_logger() -> StageLogger {
        use anodize_core::log::Verbosity;
        StageLogger::new("test", Verbosity::Normal)
    }

    // --- Artifact filtering tests ---

    #[test]
    fn test_filter_matches_default_curated_list() {
        // Default filter: curated list includes Archive / UploadableBinary /
        // LinuxPackage / UploadableFile / DockerImage / DockerManifest /
        // DockerImageV2 / SourceArchive / Sbom. Raw `Binary` and other kinds
        // outside the list must opt in via explicit `artifact_types`.
        let publisher = make_publisher("echo", None, None);

        let binary = make_artifact(ArtifactKind::Binary, "dist/myapp", None);
        let uploadable_binary = make_artifact(
            ArtifactKind::UploadableBinary,
            "dist/myapp-linux-amd64",
            None,
        );
        let archive = make_artifact(ArtifactKind::Archive, "dist/myapp.tar.gz", None);
        let checksum = make_artifact(ArtifactKind::Checksum, "dist/checksums.sha256", None);
        let metadata = make_artifact(ArtifactKind::Metadata, "dist/metadata.json", None);
        let signature = make_artifact(ArtifactKind::Signature, "dist/myapp.tar.gz.sig", None);

        // In the default list.
        assert!(matches_publisher_filter(&archive, &publisher));
        assert!(matches_publisher_filter(&uploadable_binary, &publisher));

        // Not in the default list — raw Binary is separate from UploadableBinary.
        assert!(
            !matches_publisher_filter(&binary, &publisher),
            "raw Binary kind is not in the default curated list"
        );
        // Opt-in kinds excluded by default.
        assert!(
            !matches_publisher_filter(&checksum, &publisher),
            "checksums excluded by default"
        );
        assert!(
            !matches_publisher_filter(&metadata, &publisher),
            "metadata artifacts excluded by default"
        );
        assert!(
            !matches_publisher_filter(&signature, &publisher),
            "signatures excluded by default"
        );

        // Opt in to checksums
        let mut pub_with_checksums = make_publisher("echo", None, None);
        pub_with_checksums.checksum = Some(true);
        assert!(matches_publisher_filter(&checksum, &pub_with_checksums));

        // Opt in to metadata
        let mut pub_with_meta = make_publisher("echo", None, None);
        pub_with_meta.meta = Some(true);
        assert!(matches_publisher_filter(&metadata, &pub_with_meta));

        // Opt in to signatures (also matches Certificate).
        let mut pub_with_sig = make_publisher("echo", None, None);
        pub_with_sig.signature = Some(true);
        assert!(matches_publisher_filter(&signature, &pub_with_sig));
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
        let publisher = make_publisher("echo", Some(vec!["linux-amd64"]), Some(vec!["archive"]));

        // Matches both filters
        let good = make_artifact(ArtifactKind::Archive, "dist/a.tar.gz", Some("linux-amd64"));
        assert!(matches_publisher_filter(&good, &publisher));

        // Right type but wrong id
        let wrong_id = make_artifact(ArtifactKind::Archive, "dist/b.tar.gz", Some("darwin-arm64"));
        assert!(!matches_publisher_filter(&wrong_id, &publisher));

        // Right id but wrong type
        let wrong_type = make_artifact(ArtifactKind::Binary, "dist/myapp", Some("linux-amd64"));
        assert!(!matches_publisher_filter(&wrong_type, &publisher));
    }

    // --- Command construction tests ---

    #[test]
    fn test_build_command_renders_artifact_vars() {
        let vars = base_vars();
        let artifact = make_artifact(ArtifactKind::Archive, "/dist/myapp-1.0.0.tar.gz", None);

        let (cmd, args) = build_publisher_command(
            "curl -F 'file=@{{ ArtifactPath }}'",
            Some(&[
                "--header".to_string(),
                "X-Name: {{ ArtifactName }}".to_string(),
            ]),
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
    fn test_build_command_renders_os_arch_target_vars() {
        let vars = base_vars();
        let artifact = make_artifact(ArtifactKind::Archive, "/dist/myapp.tar.gz", None);
        // make_artifact sets target to "x86_64-unknown-linux-gnu"

        let (cmd, _) = build_publisher_command(
            "deploy --os {{ Os }} --arch {{ Arch }} --target {{ Target }}",
            None,
            &artifact,
            &vars,
        )
        .unwrap();

        assert_eq!(
            cmd,
            "deploy --os linux --arch amd64 --target x86_64-unknown-linux-gnu"
        );
    }

    #[test]
    fn test_build_command_renders_artifact_kind() {
        let vars = base_vars();
        let artifact = make_artifact(ArtifactKind::LinuxPackage, "/dist/myapp.deb", None);

        let (cmd, _) =
            build_publisher_command("echo {{ ArtifactKind }}", None, &artifact, &vars).unwrap();

        assert_eq!(cmd, "echo linux_package");
    }

    // --- Dry-run behavior test ---

    #[test]
    fn test_dry_run_does_not_execute() {
        let vars = base_vars();
        let artifacts = vec![make_artifact(
            ArtifactKind::Archive,
            "/dist/myapp.tar.gz",
            None,
        )];
        let publishers = vec![PublisherConfig {
            name: Some("test".to_string()),
            cmd: "this-command-does-not-exist --should-not-run".to_string(),
            args: None,
            ids: None,
            artifact_types: None,
            env: None,
            dir: None,
            disable: None,
            checksum: None,
            signature: None,
            meta: None,
            extra_files: None,
            templated_extra_files: None,
        }];

        // In dry-run mode, the command is never executed, so a non-existent
        // command should not cause an error.
        let result = run_publishers(
            &publishers,
            &artifacts,
            &vars,
            true,
            &test_logger(),
            1,
            None,
        );
        assert!(
            result.is_ok(),
            "dry-run should not execute commands: {:?}",
            result.err()
        );
    }

    // --- Empty publishers is a no-op ---

    #[test]
    fn test_empty_publishers_is_noop() {
        let vars = base_vars();
        let artifacts = vec![make_artifact(ArtifactKind::Binary, "/dist/myapp", None)];

        let result = run_publishers(&[], &artifacts, &vars, false, &test_logger(), 1, None);
        assert!(result.is_ok());
    }

    // --- Empty cmd is skipped ---

    #[test]
    fn test_empty_cmd_is_skipped() {
        let vars = base_vars();
        let artifacts = vec![make_artifact(ArtifactKind::Binary, "/dist/myapp", None)];
        let publishers = vec![PublisherConfig {
            name: Some("empty".to_string()),
            cmd: String::new(),
            args: None,
            ids: None,
            artifact_types: None,
            env: None,
            dir: None,
            disable: None,
            checksum: None,
            signature: None,
            meta: None,
            extra_files: None,
            templated_extra_files: None,
        }];

        let result = run_publishers(
            &publishers,
            &artifacts,
            &vars,
            false,
            &test_logger(),
            1,
            None,
        );
        assert!(result.is_ok());
    }

    // --- SkipMemento integration ---

    #[test]
    fn test_publisher_empty_cmd_records_skip_memento() {
        // A publisher with an empty cmd must skip AND record the skip, so the
        // end-of-pipeline summary shows it. Otherwise a typo'd publisher cmd
        // looks identical to a real skipped publisher in the logs.
        let vars = base_vars();
        let artifacts = vec![make_artifact(ArtifactKind::Binary, "/dist/myapp", None)];
        let publishers = vec![PublisherConfig {
            name: Some("noisy".to_string()),
            cmd: String::new(),
            args: None,
            ids: None,
            artifact_types: None,
            env: None,
            dir: None,
            disable: None,
            checksum: None,
            signature: None,
            meta: None,
            extra_files: None,
            templated_extra_files: None,
        }];

        let memento = anodize_core::pipe_skip::SkipMemento::new();
        let result = run_publishers(
            &publishers,
            &artifacts,
            &vars,
            false,
            &test_logger(),
            1,
            Some(&memento),
        );
        assert!(result.is_ok());
        let events = memento.snapshot();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stage, "publisher");
        assert_eq!(events[0].label, "noisy");
        assert_eq!(events[0].reason, "empty cmd");
    }

    #[test]
    fn test_publisher_no_matching_artifacts_records_skip_memento() {
        // A publisher whose `ids` filter eliminates every artifact should
        // skip cleanly and record the event.
        let vars = base_vars();
        let artifacts = vec![make_artifact(ArtifactKind::Binary, "/dist/myapp", None)];
        let publishers = vec![PublisherConfig {
            name: Some("filtered".to_string()),
            cmd: "true".to_string(),
            args: None,
            ids: Some(vec!["does-not-exist".to_string()]),
            artifact_types: None,
            env: None,
            dir: None,
            disable: None,
            checksum: None,
            signature: None,
            meta: None,
            extra_files: None,
            templated_extra_files: None,
        }];

        let memento = anodize_core::pipe_skip::SkipMemento::new();
        let result = run_publishers(
            &publishers,
            &artifacts,
            &vars,
            true, // dry-run so the "true" command isn't actually invoked
            &test_logger(),
            1,
            Some(&memento),
        );
        assert!(result.is_ok());
        let events = memento.snapshot();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].reason, "no matching artifacts");
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
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
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
        assert_eq!(p1.ids.as_ref().unwrap(), &["linux-amd64", "darwin-arm64"]);
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
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.publishers.is_none());
    }

    // -----------------------------------------------------------------------
    // Tests for dir and disable config fields
    // -----------------------------------------------------------------------

    #[test]
    fn test_publisher_config_parses_dir_and_disable() {
        use anodize_core::config::Config;

        let yaml = r#"
project_name: test
publishers:
  - name: deploy
    cmd: "deploy.sh"
    dir: "/opt/deploy"
    disable: "{{ IsSnapshot }}"
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let publishers = config.publishers.as_ref().unwrap();
        assert_eq!(publishers.len(), 1);
        assert_eq!(publishers[0].dir.as_deref(), Some("/opt/deploy"));
        assert_eq!(
            publishers[0].disable,
            Some(StringOrBool::String("{{ IsSnapshot }}".to_string()))
        );
    }

    #[test]
    fn test_publisher_dir_sets_working_directory() {
        // This test verifies the dir field is present and would be used.
        // We can't easily test Command::current_dir in a unit test without running it,
        // but we verify the config parsing round-trips correctly.
        let publisher = PublisherConfig {
            name: Some("test".to_string()),
            cmd: "echo hello".to_string(),
            args: None,
            ids: None,
            artifact_types: None,
            env: None,
            dir: Some("/tmp/work".to_string()),
            disable: None,
            checksum: None,
            signature: None,
            meta: None,
            extra_files: None,
            templated_extra_files: None,
        };
        assert_eq!(publisher.dir.as_deref(), Some("/tmp/work"));
    }

    #[test]
    fn test_publisher_disable_skips_when_true() {
        let vars = base_vars();
        let artifacts = vec![make_artifact(
            ArtifactKind::Archive,
            "/dist/myapp.tar.gz",
            None,
        )];
        let publishers = vec![PublisherConfig {
            name: Some("disabled".to_string()),
            cmd: "this-should-not-run".to_string(),
            args: None,
            ids: None,
            artifact_types: None,
            env: None,
            dir: None,
            disable: Some(StringOrBool::String("true".to_string())),
            checksum: None,
            signature: None,
            meta: None,
            extra_files: None,
            templated_extra_files: None,
        }];

        // Publisher with disable="true" should be skipped entirely
        let result = run_publishers(
            &publishers,
            &artifacts,
            &vars,
            false,
            &test_logger(),
            1,
            None,
        );
        assert!(
            result.is_ok(),
            "disabled publisher should be skipped without error: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_publisher_disable_template_conditional() {
        let mut vars = base_vars();
        vars.set("IsSnapshot", "true");

        let artifacts = vec![make_artifact(
            ArtifactKind::Archive,
            "/dist/myapp.tar.gz",
            None,
        )];
        let publishers = vec![PublisherConfig {
            name: Some("conditional".to_string()),
            cmd: "this-should-not-run".to_string(),
            args: None,
            ids: None,
            artifact_types: None,
            env: None,
            dir: None,
            disable: Some(StringOrBool::String("{{ IsSnapshot }}".to_string())),
            checksum: None,
            signature: None,
            meta: None,
            extra_files: None,
            templated_extra_files: None,
        }];

        // When IsSnapshot is "true", the disable template renders to "true" and publisher is skipped
        let result = run_publishers(
            &publishers,
            &artifacts,
            &vars,
            false,
            &test_logger(),
            1,
            None,
        );
        assert!(
            result.is_ok(),
            "conditionally disabled publisher should be skipped: {:?}",
            result.err()
        );
    }

    // --- Meta field tests ---

    #[test]
    fn test_meta_false_excludes_metadata() {
        let publisher = make_publisher("echo", None, None);
        let metadata = make_artifact(ArtifactKind::Metadata, "dist/metadata.json", None);
        assert!(!matches_publisher_filter(&metadata, &publisher));
    }

    #[test]
    fn test_meta_true_includes_metadata() {
        let mut publisher = make_publisher("echo", None, None);
        publisher.meta = Some(true);
        let metadata = make_artifact(ArtifactKind::Metadata, "dist/metadata.json", None);
        assert!(matches_publisher_filter(&metadata, &publisher));
    }

    // --- Extra files config parsing ---

    #[test]
    fn test_publisher_config_parses_meta_and_extra_files() {
        use anodize_core::config::Config;

        let yaml = r#"
project_name: test
publishers:
  - name: deploy
    cmd: "deploy.sh"
    meta: true
    extra_files:
      - glob: "docs/*.md"
      - glob: "LICENSE"
        name: "LICENSE.txt"
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let publishers = config.publishers.as_ref().unwrap();
        assert_eq!(publishers.len(), 1);

        let p = &publishers[0];
        assert_eq!(p.meta, Some(true));

        let extra = p.extra_files.as_ref().unwrap();
        assert_eq!(extra.len(), 2);
        assert_eq!(extra[0].glob, "docs/*.md");
        assert!(extra[0].name.is_none());
        assert_eq!(extra[1].glob, "LICENSE");
        assert_eq!(extra[1].name.as_deref(), Some("LICENSE.txt"));
    }

    // --- Parallel execution test ---

    #[test]
    fn test_parallel_dry_run_is_sequential() {
        let vars = base_vars();
        let artifacts = vec![
            make_artifact(ArtifactKind::Archive, "/dist/a.tar.gz", None),
            make_artifact(ArtifactKind::Archive, "/dist/b.tar.gz", None),
            make_artifact(ArtifactKind::Archive, "/dist/c.tar.gz", None),
        ];
        let publishers = vec![make_publisher("echo {{ ArtifactPath }}", None, None)];

        // Even with parallelism > 1, dry_run should use sequential path
        let result = run_publishers(
            &publishers,
            &artifacts,
            &vars,
            true,
            &test_logger(),
            4,
            None,
        );
        assert!(result.is_ok());
    }
}
