use std::path::Path;

use anodizer_core::artifact::ArtifactKind;
use anodizer_core::config::{
    ArchiveConfig, ArchivesConfig, BuildConfig, ChecksumConfig, Config, CrateConfig, Defaults,
    FormatOverride, InstallScriptConfig,
};
use anodizer_core::context::{Context, ContextOptions};

use super::*;

/// The six lockstep triples the fixture project releases — the standard
/// linux/darwin/windows × amd64/arm64 matrix.
const FIXTURE_TARGETS: &[&str] = &[
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
];

/// Build a `Context` shaped like a real single-flagship-crate release: one
/// crate named `myapp` that builds the `myapp` binary and ships a `tar.gz`
/// archive (`zip` on windows), targeting [`FIXTURE_TARGETS`]. The rebuilt stage
/// derives everything from this configured intent — it never reads produced
/// `Archive` artifacts — so the fixture registers none.
fn install_ctx(dist: &Path, cfg: InstallScriptConfig) -> Context {
    install_ctx_with(dist, cfg, "v{{ Version }}", None)
}

/// [`install_ctx`] with an overridable crate `tag_template` and archive
/// `name_template`.
fn install_ctx_with(
    dist: &Path,
    cfg: InstallScriptConfig,
    tag_template: &str,
    name_template: Option<&str>,
) -> Context {
    let primary = ArchiveConfig {
        id: Some("myapp".to_string()),
        name_template: name_template.map(str::to_string),
        formats: Some(vec!["tar.gz".to_string()]),
        format_overrides: Some(vec![FormatOverride {
            os: "windows".to_string(),
            formats: Some(vec!["zip".to_string()]),
        }]),
        ids: Some(vec!["myapp".to_string()]),
        ..Default::default()
    };
    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: "crates/cli".to_string(),
        tag_template: tag_template.to_string(),
        builds: Some(vec![BuildConfig {
            id: Some("myapp".to_string()),
            binary: Some("myapp".to_string()),
            ..Default::default()
        }]),
        archives: ArchivesConfig::Configs(vec![primary]),
        ..Default::default()
    };
    let config = Config {
        project_name: "myapp".to_string(),
        dist: dist.to_path_buf(),
        install_scripts: vec![cfg],
        defaults: Some(Defaults {
            targets: Some(FIXTURE_TARGETS.iter().map(|s| s.to_string()).collect()),
            ..Default::default()
        }),
        crates: vec![crate_cfg],
        ..Default::default()
    };

    let mut ctx = Context::new(config, ContextOptions::default());
    // Variant derivation consults the process env; seal so a stray exported
    // RUSTFLAGS cannot perturb the fixture asset names.
    ctx.set_env_source(anodizer_core::MapEnvSource::new());
    ctx.template_vars_mut().set("ProjectName", "myapp");
    ctx.template_vars_mut().set("Version", "1.2.3");
    ctx
}

/// Run the stage and return the produced script text for `filename`.
fn run_and_read(dist: &Path, cfg: InstallScriptConfig, filename: &str) -> String {
    let mut ctx = install_ctx(dist, cfg);
    InstallScriptStage.run(&mut ctx).expect("stage run");
    std::fs::read_to_string(dist.join(filename)).expect("read produced script")
}

fn default_cfg() -> InstallScriptConfig {
    InstallScriptConfig {
        repo: Some("acme/tool".to_string()),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Engine-derived case tables
// ---------------------------------------------------------------------------

#[test]
fn bakes_engine_asset_and_detect_cases() {
    let tmp = tempfile::tempdir().unwrap();
    let script = run_and_read(tmp.path(), default_cfg(), "install.sh");

    // Asset arms are the engine's `os-arch)` / `ARCHIVE="..."` shape, carrying
    // the `${version}` placeholder (NOT the concrete 1.2.3) and the default
    // underscore name template — so the same script serves any release the
    // user pins via VERSION=.
    assert!(
        script.contains(r#"ARCHIVE="myapp_${version}_linux_amd64.tar.gz""#),
        "engine linux-amd64 asset arm missing:\n{script}"
    );
    assert!(script.contains(r#"ARCHIVE="myapp_${version}_darwin_arm64.tar.gz""#));
    // windows uses the format_override → zip.
    assert!(script.contains(r#"ARCHIVE="myapp_${version}_windows_amd64.zip""#));
    // The concrete release version must never leak into the asset arms.
    assert!(
        !script.contains("myapp_1.2.3_linux_amd64"),
        "concrete version must be replaced by ${{version}}"
    );

    // Engine `uname` detection arms, not a hand-rolled shell mapping.
    assert!(script.contains(r#"Linux*) echo "linux" ;;"#));
    assert!(script.contains(r#"Darwin*) echo "darwin" ;;"#));
    assert!(script.contains(r#"MINGW*|MSYS*|CYGWIN*) echo "windows" ;;"#));
    assert!(script.contains(r#"x86_64|amd64) echo "amd64" ;;"#));
    assert!(script.contains(r#"aarch64|arm64) echo "arm64" ;;"#));

    // Supported-platform list for the fallthrough error path.
    assert!(script.contains(
        "supported: darwin-amd64 darwin-arm64 linux-amd64 linux-arm64 windows-amd64 windows-arm64"
    ));

    // Derived repo + default checksums filename (version-templated).
    assert!(script.contains(r#"REPO="acme/tool""#));
    assert!(script.contains(r#"CHECKSUMS="myapp_${version}_checksums.txt""#));
    // Default binary list is the project name.
    assert!(script.contains(r#"BINARIES="myapp""#));
    // Default base_url + tag prefix.
    assert!(script.contains(r#"BASE_URL="https://github.com""#));
    assert!(script.contains(r#"tag="v${version}""#));
    assert!(script.contains(r#"VERIFY_CHECKSUM="true""#));
}

#[test]
fn deterministic_across_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let first = run_and_read(tmp.path(), default_cfg(), "install.sh");
    let second = run_and_read(tmp.path(), default_cfg(), "install.sh");
    assert_eq!(first, second, "script must be byte-identical across runs");
}

#[test]
fn honors_filename_install_dir_and_binaries_overrides() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = InstallScriptConfig {
        repo: Some("acme/tool".to_string()),
        filename: Some("get.sh".to_string()),
        install_dir: Some("/opt/acme/bin".to_string()),
        binaries: Some(vec!["acme".to_string(), "acme-helper".to_string()]),
        ..Default::default()
    };
    let script = run_and_read(tmp.path(), cfg, "get.sh");
    assert!(script.contains(r#"INSTALL_DIR="${INSTALL_DIR:-/opt/acme/bin}""#));
    assert!(script.contains(r#"BINARIES="acme acme-helper""#));
    // The default install.sh must NOT exist — only the override filename.
    assert!(!tmp.path().join("install.sh").exists());
}

#[test]
fn base_url_override_appears_in_urls() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = InstallScriptConfig {
        repo: Some("acme/tool".to_string()),
        base_url: Some("https://ghe.example.com".to_string()),
        ..Default::default()
    };
    let script = run_and_read(tmp.path(), cfg, "install.sh");
    assert!(script.contains(r#"BASE_URL="https://ghe.example.com""#));
    // A trailing slash on the configured base_url is trimmed so the runtime
    // `$BASE_URL/$REPO/...` join never doubles up.
    let cfg_slash = InstallScriptConfig {
        repo: Some("acme/tool".to_string()),
        base_url: Some("https://ghe.example.com/".to_string()),
        filename: Some("slash.sh".to_string()),
        ..Default::default()
    };
    let script2 = run_and_read(tmp.path(), cfg_slash, "slash.sh");
    assert!(script2.contains(r#"BASE_URL="https://ghe.example.com""#));
}

#[test]
fn verify_checksum_false_bakes_disabled_gate() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = InstallScriptConfig {
        repo: Some("acme/tool".to_string()),
        verify_checksum: Some(false),
        ..Default::default()
    };
    let script = run_and_read(tmp.path(), cfg, "install.sh");
    assert!(script.contains(r#"VERIFY_CHECKSUM="false""#));
    // The sha256 block is gated on the flag, so with it false the checksum
    // step is skipped at run time (proven end-to-end below).
    assert!(script.contains(r#"if [ "$VERIFY_CHECKSUM" = "true" ]; then"#));
}

#[test]
fn non_v_tag_prefix_is_derived_from_crate_tag_template() {
    let tmp = tempfile::tempdir().unwrap();
    let mut ctx = install_ctx_with(
        tmp.path(),
        InstallScriptConfig {
            repo: Some("acme/tool".to_string()),
            ..Default::default()
        },
        "release-{{ Version }}",
        None,
    );
    InstallScriptStage.run(&mut ctx).expect("stage run");
    let script = std::fs::read_to_string(tmp.path().join("install.sh")).unwrap();
    // The tag prefix comes from the flagship crate's tag_template, not a
    // hardcoded `v`: `tag="${TAG_PREFIX}${version}"` → `release-${version}`.
    assert!(
        script.contains(r#"tag="release-${version}""#),
        "tag must use the crate's non-v prefix:\n{script}"
    );
    // The bare-version derivation strips the SAME prefix from VERSION.
    assert!(script.contains(r#"version="${VERSION#release-}""#));
}

#[test]
fn bare_version_tag_template_yields_empty_prefix() {
    let tmp = tempfile::tempdir().unwrap();
    let mut ctx = install_ctx_with(tmp.path(), default_cfg(), "{{ Version }}", None);
    InstallScriptStage.run(&mut ctx).expect("stage run");
    let script = std::fs::read_to_string(tmp.path().join("install.sh")).unwrap();
    // A bare-version tag template strips to an empty prefix, so the script
    // reconstructs the tag as the bare version.
    assert!(
        script.contains(r#"tag="${version}""#),
        "bare `{{{{ Version }}}}` tag template → empty prefix:\n{script}"
    );
}

/// A version-INFIX tag template (text after the version) cannot be inverted by
/// a `curl | sh` installer that reconstructs the tag from a runtime version, so
/// the stage must fail loudly instead of baking a broken `tag=` line.
#[test]
fn version_infix_tag_template_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let mut ctx = install_ctx_with(tmp.path(), default_cfg(), "{{ Version }}-stable", None);
    let err = InstallScriptStage.run(&mut ctx).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("must end with the version"),
        "expected a version-infix rejection, got: {msg}"
    );
}

/// install-script renders the checksums `name_template` under AMBIENT template
/// vars, exactly as the checksum stage's `write_combined_file` does — it must
/// NOT rebind `CrateName` to the flagship crate, or the filename it bakes would
/// diverge from the file the checksum stage actually writes (installer 404).
#[test]
fn checksums_filename_uses_ambient_crate_name_not_flagship() {
    let tmp = tempfile::tempdir().unwrap();
    let mut ctx = install_ctx(tmp.path(), default_cfg());
    // Flagship crate is named "myapp"; give it a CrateName-referencing template.
    ctx.config.crates[0].checksum = Some(ChecksumConfig {
        name_template: Some("{{ ProjectName }}-{{ CrateName }}-sums.txt".to_string()),
        ..Default::default()
    });
    // Ambient CrateName deliberately differs from the flagship crate name.
    ctx.template_vars_mut().set("CrateName", "ambient-crate");
    InstallScriptStage.run(&mut ctx).expect("stage run");
    let script = std::fs::read_to_string(tmp.path().join("install.sh")).unwrap();
    assert!(
        script.contains(r#"CHECKSUMS="myapp-ambient-crate-sums.txt""#),
        "checksums filename must use the AMBIENT CrateName (matching the checksum \
         stage), not the flagship crate name:\n{script}"
    );
    assert!(
        !script.contains("myapp-myapp-sums.txt"),
        "install-script must NOT bind the flagship crate name for CrateName:\n{script}"
    );
}

/// Free-text metadata (name / description) carrying shell metacharacters and
/// newlines is escaped for its context: the script still passes `sh -n`, the
/// double-quoted `@NAME@` sites get backslash-escaped `"`/`$`, and a multi-line
/// description collapses to a single comment line.
#[cfg(unix)]
#[test]
fn metadata_is_shell_escaped_and_sh_valid() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = InstallScriptConfig {
        repo: Some("acme/tool".to_string()),
        name: Some("my\"app$X".to_string()),
        description: Some("line one\nline two".to_string()),
        ..Default::default()
    };
    let script = run_and_read(tmp.path(), cfg, "install.sh");

    let status = std::process::Command::new("sh")
        .arg("-n")
        .arg(tmp.path().join("install.sh"))
        .status()
        .expect("spawn sh -n");
    assert!(
        status.success(),
        "escaped metadata must keep install.sh valid POSIX sh:\n{script}"
    );
    // `info()` bakes @NAME@ into a double-quoted string — the `"` and `$` must
    // be backslash-escaped so they cannot break out / expand.
    assert!(
        script.contains(r#"my\"app\$X-install: $*"#),
        "name must be shell-dq-escaped inside the info string:\n{script}"
    );
    // The multi-line description must collapse to a single comment line.
    assert!(
        script.contains("# my\\\"app\\$X — line one line two"),
        "description newline must be sanitized to a single comment line:\n{script}"
    );
}

#[test]
fn no_flagship_crate_skips_with_status() {
    // A pure-library workspace: no crate builds the `myapp` binary, so the
    // installer has nothing to fetch and must step aside (no install.sh).
    let tmp = tempfile::tempdir().unwrap();
    let config = Config {
        project_name: "myapp".to_string(),
        dist: tmp.path().to_path_buf(),
        install_scripts: vec![default_cfg()],
        crates: vec![CrateConfig {
            name: "myapp-core".to_string(),
            path: "crates/core".to_string(),
            tag_template: "v{{ Version }}".to_string(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("ProjectName", "myapp");
    ctx.template_vars_mut().set("Version", "1.2.3");
    InstallScriptStage.run(&mut ctx).expect("stage run");
    assert!(!tmp.path().join("install.sh").exists());
    assert!(
        ctx.artifacts
            .by_kind(ArtifactKind::InstallScript)
            .is_empty()
    );
}

// ---------------------------------------------------------------------------
// Shell shape guards
// ---------------------------------------------------------------------------

#[test]
fn starts_with_shebang_and_has_no_bashisms() {
    let tmp = tempfile::tempdir().unwrap();
    let script = run_and_read(tmp.path(), default_cfg(), "install.sh");
    assert!(script.starts_with("#!/bin/sh\nset -eu\n"));
    // No bash-only constructs. `[[ ` / ` ]]` carry a space so they match the
    // bash test keyword, not the POSIX `[[:space:]]` bracket class in sed.
    for bashism in [
        "[[ ",
        " ]]",
        "function ",
        "local ",
        "declare ",
        "pipefail",
        "$RANDOM",
    ] {
        assert!(
            !script.contains(bashism),
            "generated script must not contain bashism {bashism:?}"
        );
    }
    // No wall-clock / nondeterministic tokens.
    assert!(!script.contains("$(date"));
}

/// Regression lock for the `set -u` ordering bug: the checksums filename embeds
/// `${version}`, so its assignment MUST appear only *after* `version` is
/// resolved. If `CHECKSUMS=` ever migrates back above the `version=` line,
/// every `curl | sh` run aborts with "version: parameter not set" — a failure
/// no string-content assertion catches, but a positional one does.
#[test]
fn checksums_assignment_follows_version_resolution() {
    let tmp = tempfile::tempdir().unwrap();
    let script = run_and_read(tmp.path(), default_cfg(), "install.sh");

    let version_pos = script
        .find(r#"version="${VERSION#"#)
        .expect("version resolution present");
    let checksums_pos = script
        .find(r#"CHECKSUMS=""#)
        .expect("checksums assignment present");
    assert!(
        version_pos < checksums_pos,
        "CHECKSUMS= (which references ${{version}}) must be assigned AFTER the \
         version= resolution — under `set -u` an earlier assignment aborts \
         every run with 'version: parameter not set'"
    );
}

/// The generated script must be valid POSIX `sh` — a syntax slip (or a stray
/// bashism the string checks miss) surfaces here as a non-zero `sh -n`.
#[cfg(unix)]
#[test]
fn script_passes_sh_syntax_check() {
    let tmp = tempfile::tempdir().unwrap();
    run_and_read(tmp.path(), default_cfg(), "install.sh");
    let status = std::process::Command::new("sh")
        .arg("-n")
        .arg(tmp.path().join("install.sh"))
        .status()
        .expect("spawn sh -n");
    assert!(status.success(), "generated install.sh failed `sh -n`");
}

// ---------------------------------------------------------------------------
// Artifact + config plumbing
// ---------------------------------------------------------------------------

#[test]
fn registers_install_script_artifact() {
    let tmp = tempfile::tempdir().unwrap();
    let mut ctx = install_ctx(tmp.path(), default_cfg());
    InstallScriptStage.run(&mut ctx).expect("stage run");
    let produced = ctx.artifacts.by_kind(ArtifactKind::InstallScript);
    assert_eq!(produced.len(), 1);
    assert_eq!(produced[0].name, "install.sh");
    assert_eq!(
        produced[0].metadata.get("id").map(String::as_str),
        Some("default")
    );
}

#[test]
fn install_script_is_a_checksummed_and_signed_subject() {
    // W3: the installer must be checksummed AND signed like makeself, so its
    // kind must be a primary subject kind.
    assert!(
        anodizer_core::artifact::primary_subject_kinds().contains(&ArtifactKind::InstallScript),
        "InstallScript must be a checksum/sign subject kind"
    );
}

#[test]
fn empty_config_is_noop() {
    let tmp = tempfile::tempdir().unwrap();
    let config = Config {
        project_name: "myapp".to_string(),
        dist: tmp.path().to_path_buf(),
        // No install_scripts configured.
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.2.3");

    InstallScriptStage.run(&mut ctx).expect("noop run");
    assert!(!tmp.path().join("install.sh").exists());
    assert!(
        ctx.artifacts
            .by_kind(ArtifactKind::InstallScript)
            .is_empty()
    );
}

#[test]
fn derives_repo_from_git_remote() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tempfile::tempdir().unwrap();
    // A minimal git repo with an origin remote for owner/name derivation.
    anodizer_core::test_helpers::init_git_repo_with_commits(repo_dir.path(), &["initial"]);
    assert!(
        anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = std::process::Command::new("git");
                cmd.args(["remote", "add", "origin", "git@github.com:acme/widget.git"])
                    .current_dir(repo_dir.path());
                cmd
            },
            "git",
        )
        .status
        .success(),
        "add remote"
    );

    // No repo: on the config → must derive from the remote.
    let mut ctx = install_ctx(tmp.path(), InstallScriptConfig::default());
    ctx.options.project_root = Some(repo_dir.path().to_path_buf());
    InstallScriptStage.run(&mut ctx).expect("stage run");

    let script = std::fs::read_to_string(tmp.path().join("install.sh")).unwrap();
    assert!(
        script.contains(r#"REPO="acme/widget""#),
        "repo must be derived from git origin remote"
    );
}

#[test]
fn config_roundtrip_single_object() {
    // A single mapping under `install_scripts:` deserializes to a one-element
    // vec via the custom visitor.
    let yaml = "project_name: myapp\ninstall_scripts:\n  id: solo\n  binaries: [acme]\n";
    let config: Config = serde_yaml_ng::from_str(yaml).expect("valid single-object config");
    assert_eq!(config.install_scripts.len(), 1);
    assert_eq!(config.install_scripts[0].id.as_deref(), Some("solo"));
    assert_eq!(
        config.install_scripts[0].binaries.as_deref(),
        Some(["acme".to_string()].as_slice())
    );
}

#[test]
fn config_roundtrip_array() {
    // An array under `install_scripts:` deserializes to a multi-element vec.
    let yaml = "project_name: myapp\ninstall_scripts:\n  - id: a\n  - id: b\n";
    let config: Config = serde_yaml_ng::from_str(yaml).expect("valid array config");
    assert_eq!(config.install_scripts.len(), 2);
    assert_eq!(config.install_scripts[0].id.as_deref(), Some("a"));
    assert_eq!(config.install_scripts[1].id.as_deref(), Some("b"));
}

#[test]
fn config_rejects_unknown_field() {
    // deny_unknown_fields must reject a typo'd key.
    let yaml = "install_scripts:\n  id: x\n  bogus: 1\n";
    assert!(serde_yaml_ng::from_str::<Config>(yaml).is_err());
}

#[test]
fn config_skip_accepts_bool_and_template() {
    let yaml = "install_scripts:\n  id: x\n  skip: true\n";
    let config: Config = serde_yaml_ng::from_str(yaml).expect("bool skip");
    assert!(config.install_scripts[0].skip.is_some());

    let yaml = "install_scripts:\n  id: x\n  skip: \"{{ .IsSnapshot }}\"\n";
    let config: Config = serde_yaml_ng::from_str(yaml).expect("template skip");
    assert!(config.install_scripts[0].skip.is_some());
}

// ---------------------------------------------------------------------------
// End-to-end offline execution
// ---------------------------------------------------------------------------

/// End-to-end execution of the generated installer, fully offline. A fake
/// release (tarball + checksums file) is served through `curl`/`uname` PATH
/// stubs so the script's real download → sha256-verify → extract → install path
/// runs under `set -eu`. Asserts the binary lands installed.
#[cfg(unix)]
#[test]
fn installs_binary_end_to_end_offline() {
    let tmp = tempfile::tempdir().unwrap();
    let dist = tmp.path().join("dist");
    std::fs::create_dir_all(&dist).unwrap();

    run_and_read(&dist, default_cfg(), "install.sh");
    let script = dist.join("install.sh");

    let release = tmp.path().join("release");
    std::fs::create_dir_all(&release).unwrap();
    let asset = "myapp_1.2.3_linux_amd64.tar.gz";
    build_release_tarball(&release, asset, &["myapp"]);

    let sha = sha256_of(&release.join(asset));
    std::fs::write(
        release.join("myapp_1.2.3_checksums.txt"),
        format!("{sha}  {asset}\n"),
    )
    .unwrap();

    let dest = run_installer(&script, tmp.path(), &release, &[]);
    assert_installed(&dest, "myapp", "fake-myapp");
}

/// Multi-binary install: an archive shipping two binaries lands both when the
/// config lists them (the correctness fix over the old singular `binary`).
#[cfg(unix)]
#[test]
fn installs_all_binaries_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let dist = tmp.path().join("dist");
    std::fs::create_dir_all(&dist).unwrap();

    let cfg = InstallScriptConfig {
        repo: Some("acme/tool".to_string()),
        binaries: Some(vec!["myapp".to_string(), "myapp-helper".to_string()]),
        ..Default::default()
    };
    let mut ctx = install_ctx(&dist, cfg);
    InstallScriptStage.run(&mut ctx).expect("stage run");
    let script = dist.join("install.sh");

    let release = tmp.path().join("release");
    std::fs::create_dir_all(&release).unwrap();
    let asset = "myapp_1.2.3_linux_amd64.tar.gz";
    build_release_tarball(&release, asset, &["myapp", "myapp-helper"]);

    let sha = sha256_of(&release.join(asset));
    std::fs::write(
        release.join("myapp_1.2.3_checksums.txt"),
        format!("{sha}  {asset}\n"),
    )
    .unwrap();

    let dest = run_installer(&script, tmp.path(), &release, &[]);
    assert_installed(&dest, "myapp", "fake-myapp");
    assert_installed(&dest, "myapp-helper", "fake-myapp");
}

/// `verify_checksum: false` must install even when NO checksums file is
/// published — the gate is skipped, so the missing sidecar is not fatal.
#[cfg(unix)]
#[test]
fn verify_checksum_false_installs_without_checksums_file() {
    let tmp = tempfile::tempdir().unwrap();
    let dist = tmp.path().join("dist");
    std::fs::create_dir_all(&dist).unwrap();

    let cfg = InstallScriptConfig {
        repo: Some("acme/tool".to_string()),
        verify_checksum: Some(false),
        ..Default::default()
    };
    let mut ctx = install_ctx(&dist, cfg);
    InstallScriptStage.run(&mut ctx).expect("stage run");
    let script = dist.join("install.sh");

    let release = tmp.path().join("release");
    std::fs::create_dir_all(&release).unwrap();
    let asset = "myapp_1.2.3_linux_amd64.tar.gz";
    // Deliberately publish NO checksums file / sidecar.
    build_release_tarball(&release, asset, &["myapp"]);

    let dest = run_installer(&script, tmp.path(), &release, &[]);
    assert_installed(&dest, "myapp", "fake-myapp");
}

// ---------------------------------------------------------------------------
// E2E helpers
// ---------------------------------------------------------------------------

/// Build a gzip tarball named `asset` under `release`, containing each named
/// binary as a `#!/bin/sh` echo stub.
#[cfg(unix)]
fn build_release_tarball(release: &Path, asset: &str, binaries: &[&str]) {
    use std::process::Command;
    let payload = release.join("payload");
    std::fs::create_dir_all(&payload).unwrap();
    for b in binaries {
        std::fs::write(payload.join(b), "#!/bin/sh\necho fake-myapp\n").unwrap();
    }
    let mut cmd = Command::new("tar");
    cmd.args(["-czf"])
        .arg(release.join(asset))
        .arg("-C")
        .arg(&payload);
    for b in binaries {
        cmd.arg(b);
    }
    assert!(
        cmd.status().expect("spawn tar").success(),
        "failed to build fixture tarball"
    );
}

/// Run `install.sh` with `curl`/`uname` PATH stubs that serve `$FAKE_RELEASE`
/// by basename and pin the host to Linux/x86_64. Returns the install dir.
#[cfg(unix)]
fn run_installer(
    script: &Path,
    tmp: &Path,
    release: &Path,
    extra_env: &[(&str, &str)],
) -> std::path::PathBuf {
    use std::process::Command;

    let stub_dir = tmp.join("bin");
    std::fs::create_dir_all(&stub_dir).unwrap();
    write_stub(
        &stub_dir,
        "curl",
        r#"#!/bin/sh
url=""; dest=""
while [ $# -gt 0 ]; do
  case "$1" in
    -o) dest="$2"; shift 2 ;;
    -*) shift ;;
    *) url="$1"; shift ;;
  esac
done
file="$FAKE_RELEASE/$(basename "$url")"
[ -f "$file" ] || exit 22
if [ -n "$dest" ]; then cp "$file" "$dest"; else cat "$file"; fi
"#,
    );
    write_stub(
        &stub_dir,
        "uname",
        "#!/bin/sh\ncase \"$1\" in -s) echo Linux ;; -m) echo x86_64 ;; *) echo Linux ;; esac\n",
    );

    let dest = tmp.join("install-dest");
    std::fs::create_dir_all(&dest).unwrap();

    let real_path = std::env::var("PATH").unwrap_or_default();
    let mut cmd = Command::new("sh");
    cmd.arg(script)
        .env("VERSION", "1.2.3")
        .env("INSTALL_DIR", &dest)
        .env("FAKE_RELEASE", release)
        .env("PATH", format!("{}:{}", stub_dir.display(), real_path));
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let output = cmd.output().expect("run install.sh");
    assert!(
        output.status.success(),
        "installer exited {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    dest
}

/// Assert `name` was installed under `dest` and carries the expected body.
#[cfg(unix)]
fn assert_installed(dest: &Path, name: &str, needle: &str) {
    let installed = dest.join(name);
    assert!(
        installed.is_file(),
        "binary {name} was not installed to {}",
        installed.display()
    );
    let body = std::fs::read_to_string(&installed).unwrap();
    assert!(body.contains(needle), "installed the wrong file for {name}");
}

/// Write an executable `#!/bin/sh` stub named `name` into `dir`.
#[cfg(unix)]
fn write_stub(dir: &Path, name: &str, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
}

/// Compute a file's sha256 via the same tools the installer uses
/// (`sha256sum`, falling back to `shasum -a 256` on macOS).
#[cfg(unix)]
fn sha256_of(path: &Path) -> String {
    use std::process::Command;
    let out = if which("sha256sum") {
        Command::new("sha256sum").arg(path).output()
    } else {
        Command::new("shasum")
            .args(["-a", "256"])
            .arg(path)
            .output()
    }
    .expect("spawn sha256 tool");
    assert!(out.status.success(), "sha256 tool failed");
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .expect("sha256 hash")
        .to_string()
}

/// A structured DATA value that literally contains a later marker's text
/// (`@NAME@` inside `base_url`) must survive the single-pass render verbatim: the
/// substituted value is never re-scanned, so it is NOT rewritten to the actual
/// `name`. Guards against the ordered-replace re-substitution bug.
#[test]
fn marker_text_in_data_value_is_not_re_substituted() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = InstallScriptConfig {
        repo: Some("acme/tool".to_string()),
        name: Some("REALNAME".to_string()),
        base_url: Some("https://example.com/@NAME@".to_string()),
        ..Default::default()
    };
    let script = run_and_read(tmp.path(), cfg, "install.sh");
    // The `@NAME@` embedded in base_url survives as literal text — it must NOT
    // be rewritten to the `name` value.
    assert!(
        script.contains(r#"BASE_URL="https://example.com/@NAME@""#),
        "literal @NAME@ inside base_url must survive verbatim, not be re-substituted:\n{script}"
    );
    assert!(
        !script.contains("https://example.com/REALNAME"),
        "base_url's @NAME@ must not be rewritten to the name value:\n{script}"
    );
}

#[cfg(unix)]
fn which(tool: &str) -> bool {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {tool} >/dev/null 2>&1"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
