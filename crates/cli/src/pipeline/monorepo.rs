//! Monorepo defaulting: path-prefixing of relative file globs and
//! release-name template defaults when `monorepo.dir` / `monorepo.tag_prefix`
//! are configured.
//!
//! Invoked from [`super::config_loader::load_config`] after per-crate `path`
//! resolution so every relative `extra_files` / `files` glob across archive,
//! release, checksum, docker, nfpm, and publisher surfaces is rewritten
//! relative to `monorepo.dir`.

use anodizer_core::config::Config;

/// Apply monorepo configuration defaults to crate configs.
///
/// When `monorepo.dir` is set:
/// - A crate's `path` is defaulted to `monorepo.dir` when empty or `"."`.
/// - Each crate's release `name_template` defaults to
///   `"{{ ProjectName }} {{ Tag }}"` so the rendered release name carries
///   a project prefix and three sub-projects don't all surface as `v1.2.3`.
/// - Every relative `extra_files` / `files` glob on archive / release /
///   checksum / source / docker / nfpm / publisher subsystems is rewritten
///   to be relative to `monorepo.dir` ("Extra files on the
///   release, archives, Docker builds, etc are prefixed with monorepo.dir"
///   contract.
///
/// When `monorepo.tag_prefix` is set, a warn is emitted when it doesn't end
/// with `/` and isn't a Category-2 short prefix (e.g. `v`).
///
/// Note: `BuildConfig` does not have a `dir` field — builds inherit
/// their working directory from `CrateConfig.path`, which is already
/// defaulted here. `PublisherConfig.dir` and `StructuredHook.dir` are
/// intentionally left alone since they represent explicit overrides.
pub(super) fn apply_monorepo_defaults(config: &mut Config) {
    validate_monorepo_tag_prefix(config);

    let monorepo_dir = config.monorepo_dir().map(|s| s.to_string());

    if let Some(dir) = monorepo_dir {
        for crate_cfg in &mut config.crates {
            apply_monorepo_to_crate(crate_cfg, &dir);
        }
        if let Some(ref mut workspaces) = config.workspaces {
            for ws in workspaces {
                for crate_cfg in &mut ws.crates {
                    apply_monorepo_to_crate(crate_cfg, &dir);
                }
            }
        }
        apply_monorepo_to_top_level(config, &dir);
    }
}

/// Path-prefix every relative file glob on a single crate config so users
/// of a monorepo subproject can write paths relative to the subproject
/// root.
fn apply_monorepo_to_crate(crate_cfg: &mut anodizer_core::config::CrateConfig, dir: &str) {
    if crate_cfg.path.is_empty() || crate_cfg.path == "." {
        crate_cfg.path = dir.to_string();
    }
    // Default release name template to a project-prefixed form when the
    // user has not chosen one ("Release name gets prefixed
    // with `{{ .ProjectName }} ` if empty" rule.
    if let Some(ref mut rel) = crate_cfg.release
        && rel.name_template.is_none()
    {
        rel.name_template = Some("{{ ProjectName }} {{ Tag }}".to_string());
    }

    // Archive configs.
    if let anodizer_core::config::ArchivesConfig::Configs(ref mut archive_cfgs) = crate_cfg.archives
    {
        for ac in archive_cfgs {
            prefix_archive_files(&mut ac.files, dir);
        }
    }
}

/// Apply monorepo prefix to top-level Config fields that carry file globs
/// (release.extra_files / source.files / etc.).
fn apply_monorepo_to_top_level(config: &mut Config, dir: &str) {
    if let Some(ref mut release) = config.release {
        if release.name_template.is_none() {
            release.name_template = Some("{{ ProjectName }} {{ Tag }}".to_string());
        }
        prefix_extra_file_specs(&mut release.extra_files, dir);
        prefix_templated_extra_files(&mut release.templated_extra_files, dir);
    }
    // Top-level checksum / source.
    if let Some(ref mut source) = config.source {
        for f in &mut source.files {
            if let Some(new_src) = anodizer_core::config::prepend_monorepo_dir(&f.src, dir) {
                f.src = new_src;
            }
        }
    }
    // Top-level uploads / blobs are publisher-attached only; their
    // extra_files live under `crate.publish.*` — handled by the per-crate
    // walk below via `prefix_publisher_extras`. Walk every crate's
    // publisher configs. Raw chained walk (not `crate_universe()`): this is
    // a mutation pass and the universe walker only hands out shared
    // borrows; prefixing every entry as written (shadowed ones included)
    // is also correct here since dedup happens at read time.
    let crates_iter = config.crates.iter_mut().chain(
        config
            .workspaces
            .iter_mut()
            .flatten()
            .flat_map(|w| w.crates.iter_mut()),
    );
    for c in crates_iter {
        prefix_publisher_extras(c, dir);
    }
}

/// Walk every packaging surface attached to a crate and prefix any
/// `extra_files` / `templated_extra_files` paths so monorepo intent is
/// uniform across archive / release / installer / blob surfaces.
fn prefix_publisher_extras(crate_cfg: &mut anodizer_core::config::CrateConfig, dir: &str) {
    // Crate-level release config (mirrors top-level).
    if let Some(ref mut rel) = crate_cfg.release {
        if rel.name_template.is_none() {
            rel.name_template = Some("{{ ProjectName }} {{ Tag }}".to_string());
        }
        prefix_extra_file_specs(&mut rel.extra_files, dir);
        prefix_templated_extra_files(&mut rel.templated_extra_files, dir);
    }
    // Crate-level checksum
    if let Some(ref mut ck) = crate_cfg.checksum {
        prefix_extra_file_specs(&mut ck.extra_files, dir);
        prefix_templated_extra_files(&mut ck.templated_extra_files, dir);
    }
    // Docker V2 build extras (Vec<String> of context files).
    if let Some(ref mut dockers) = crate_cfg.dockers_v2 {
        for d in dockers {
            if let Some(ref mut files) = d.extra_files {
                for s in files {
                    if let Some(new) = anodizer_core::config::prepend_monorepo_dir(s, dir) {
                        *s = new;
                    }
                }
            }
        }
    }
    // nFPM contents (Vec<NfpmContent> with src/dst). Prefix only `src`
    // (the host path); `dst` is the in-package destination and must
    // remain an absolute UNIX path.
    if let Some(ref mut list) = crate_cfg.nfpms {
        for n in list {
            if let Some(ref mut contents) = n.contents {
                for c in contents {
                    if let Some(new) = anodizer_core::config::prepend_monorepo_dir(&c.src, dir) {
                        c.src = new;
                    }
                }
            }
        }
    }
    // MSI (Vec<String> of WiX-context files).
    if let Some(ref mut list) = crate_cfg.msis {
        for m in list {
            if let Some(ref mut files) = m.extra_files {
                for s in files {
                    if let Some(new) = anodizer_core::config::prepend_monorepo_dir(s, dir) {
                        *s = new;
                    }
                }
            }
        }
    }
    if let Some(ref mut list) = crate_cfg.nsis {
        for n in list {
            prefix_extra_file_specs(&mut n.extra_files, dir);
            prefix_templated_extra_files(&mut n.templated_extra_files, dir);
        }
    }
    if let Some(ref mut list) = crate_cfg.dmgs {
        for d in list {
            prefix_extra_file_specs(&mut d.extra_files, dir);
            prefix_templated_extra_files(&mut d.templated_extra_files, dir);
        }
    }
    if let Some(ref mut list) = crate_cfg.app_bundles {
        for a in list {
            prefix_archive_files(&mut a.extra_files, dir);
            prefix_templated_extra_files(&mut a.templated_extra_files, dir);
        }
    }
    if let Some(ref mut list) = crate_cfg.pkgs {
        for p in list {
            prefix_extra_file_specs(&mut p.extra_files, dir);
        }
    }
    if let Some(ref mut list) = crate_cfg.flatpaks {
        for f in list {
            prefix_extra_file_specs(&mut f.extra_files, dir);
        }
    }
    // Blob uploads
    if let Some(ref mut list) = crate_cfg.blobs {
        for b in list {
            prefix_extra_file_specs(&mut b.extra_files, dir);
            prefix_templated_extra_files(&mut b.templated_extra_files, dir);
        }
    }
}

fn prefix_extra_file_specs(
    files: &mut Option<Vec<anodizer_core::config::ExtraFileSpec>>,
    dir: &str,
) {
    let Some(list) = files else { return };
    for spec in list {
        match spec {
            anodizer_core::config::ExtraFileSpec::Glob(s) => {
                if let Some(new) = anodizer_core::config::prepend_monorepo_dir(s, dir) {
                    *s = new;
                }
            }
            anodizer_core::config::ExtraFileSpec::Detailed { glob, .. } => {
                if let Some(new) = anodizer_core::config::prepend_monorepo_dir(glob, dir) {
                    *glob = new;
                }
            }
        }
    }
}

fn prefix_archive_files(
    files: &mut Option<Vec<anodizer_core::config::ArchiveFileSpec>>,
    dir: &str,
) {
    let Some(list) = files else { return };
    for spec in list {
        match spec {
            anodizer_core::config::ArchiveFileSpec::Glob(s) => {
                if let Some(new) = anodizer_core::config::prepend_monorepo_dir(s, dir) {
                    *s = new;
                }
            }
            anodizer_core::config::ArchiveFileSpec::Detailed { src, .. } => {
                if let Some(new) = anodizer_core::config::prepend_monorepo_dir(src, dir) {
                    *src = new;
                }
            }
        }
    }
}

fn prefix_templated_extra_files(
    files: &mut Option<Vec<anodizer_core::config::TemplatedExtraFile>>,
    dir: &str,
) {
    let Some(list) = files else { return };
    for f in list {
        if let Some(new) = anodizer_core::config::prepend_monorepo_dir(&f.src, dir) {
            f.src = new;
        }
    }
}

/// Predicate that returns `true` when a `monorepo.tag_prefix` value
/// looks like a typo (no trailing slash, not a Category-2 short letter
/// prefix). Used by `validate_monorepo_tag_prefix` to gate a warning;
/// exposed for testing because `tracing::warn!` capture in unit tests
/// requires a subscriber wiring that this crate does not own.
fn monorepo_tag_prefix_is_suspicious(prefix: &str) -> bool {
    if prefix.is_empty() {
        return false;
    }
    if prefix.ends_with('/') {
        return false;
    }
    // Single-char Category-2 prefix like `v` is the canonical example.
    if prefix.len() <= 2 && prefix.chars().all(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    true
}

/// Emit a `tracing::warn!` for monorepo tag-prefix shapes that almost
/// certainly indicate a typo. The docs strongly imply either a
/// trailing-slash prefix (Category 1 — `subproject1/`) or a tiny
/// well-known prefix (Category 2 — `v`).
fn validate_monorepo_tag_prefix(config: &Config) {
    let Some(prefix) = config.monorepo_tag_prefix() else {
        return;
    };
    if !monorepo_tag_prefix_is_suspicious(prefix) {
        return;
    }
    tracing::warn!(
        "monorepo.tag_prefix = '{}' is missing a trailing slash. \
         GoReleaser convention is `subproject1/` (Category 1) or a short \
         letter prefix like `v` (Category 2). Tags will be matched as \
         `{}<version>` — verify this is intentional.",
        prefix,
        prefix
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // monorepo defaults
    // -----------------------------------------------------------------------

    fn monorepo_config_with_archive_files(extra: &str) -> anodizer_core::config::Config {
        let yaml = format!(
            r#"
project_name: myapp
monorepo:
  tag_prefix: "subproj1/"
  dir: subproj1
crates:
  - name: myapp
    path: ""
    tag_template: "subproj1/v{{{{ Version }}}}"
    archives:
      - files:
          - LICENSE
          - README.md
          - src: "VERSION"
            dst: "version.txt"
{extra}
"#,
            extra = extra,
        );
        serde_yaml_ng::from_str(&yaml).expect("yaml parses")
    }

    #[test]
    fn monorepo_extra_files_auto_prefixed_on_archive() {
        let mut config = monorepo_config_with_archive_files("");
        apply_monorepo_defaults(&mut config);

        // Crate path picks up monorepo.dir.
        assert_eq!(config.crates[0].path, "subproj1");

        // Archive `files:` entries get the monorepo prefix.
        if let anodizer_core::config::ArchivesConfig::Configs(ref cfgs) = config.crates[0].archives
        {
            let files = cfgs[0].files.as_ref().unwrap();
            // LICENSE -> subproj1/LICENSE
            assert_eq!(files[0], "subproj1/LICENSE");
            // README.md -> subproj1/README.md
            assert_eq!(files[1], "subproj1/README.md");
            // Detailed.src -> subproj1/VERSION
            if let anodizer_core::config::ArchiveFileSpec::Detailed { src, dst, .. } = &files[2] {
                assert_eq!(src, "subproj1/VERSION");
                // dst is the in-archive path; must not be rewritten.
                assert_eq!(dst.as_deref(), Some("version.txt"));
            } else {
                panic!("expected Detailed variant");
            }
        } else {
            panic!("expected Configs variant");
        }
    }

    #[test]
    fn monorepo_release_name_defaults_to_project_prefix() {
        let yaml = r#"
project_name: myapp
monorepo:
  tag_prefix: "subproj1/"
  dir: subproj1
release: {}
crates:
  - name: myapp
    path: "."
    tag_template: "subproj1/v{{ Version }}"
    release: {}
"#;
        let mut config: anodizer_core::config::Config =
            serde_yaml_ng::from_str(yaml).expect("yaml parses");
        apply_monorepo_defaults(&mut config);

        // Top-level release.name_template defaults.
        let rel = config.release.as_ref().unwrap();
        assert_eq!(
            rel.name_template.as_deref(),
            Some("{{ ProjectName }} {{ Tag }}")
        );

        // Per-crate release.name_template defaults too.
        let crate_rel = config.crates[0].release.as_ref().unwrap();
        assert_eq!(
            crate_rel.name_template.as_deref(),
            Some("{{ ProjectName }} {{ Tag }}")
        );
    }

    #[test]
    fn monorepo_release_name_explicit_template_is_preserved() {
        let yaml = r#"
project_name: myapp
monorepo:
  tag_prefix: "subproj1/"
  dir: subproj1
release:
  name_template: "Release {{ Tag }}"
crates:
  - name: myapp
    path: "."
    tag_template: "subproj1/v{{ Version }}"
"#;
        let mut config: anodizer_core::config::Config =
            serde_yaml_ng::from_str(yaml).expect("yaml parses");
        apply_monorepo_defaults(&mut config);

        // User-set name_template must not be overwritten.
        let rel = config.release.as_ref().unwrap();
        assert_eq!(rel.name_template.as_deref(), Some("Release {{ Tag }}"));
    }

    #[test]
    fn monorepo_extra_files_already_prefixed_not_double_prefixed() {
        let mut config = monorepo_config_with_archive_files("");
        // Manually pre-prefix one entry.
        if let anodizer_core::config::ArchivesConfig::Configs(ref mut cfgs) =
            config.crates[0].archives
            && let Some(ref mut files) = cfgs[0].files
            && let anodizer_core::config::ArchiveFileSpec::Glob(ref mut s) = files[0]
        {
            *s = "subproj1/LICENSE".to_string();
        }
        apply_monorepo_defaults(&mut config);

        if let anodizer_core::config::ArchivesConfig::Configs(ref cfgs) = config.crates[0].archives
        {
            let files = cfgs[0].files.as_ref().unwrap();
            // Already prefixed; no double-prefix.
            assert_eq!(files[0], "subproj1/LICENSE");
        }
    }

    #[test]
    fn monorepo_release_extra_files_prefixed() {
        let yaml = r#"
project_name: myapp
monorepo:
  tag_prefix: "subproj1/"
  dir: subproj1
release:
  extra_files:
    - glob: "CHANGELOG.md"
    - "*.sig"
crates:
  - name: myapp
    path: "."
    tag_template: "subproj1/v{{ Version }}"
"#;
        let mut config: anodizer_core::config::Config =
            serde_yaml_ng::from_str(yaml).expect("yaml parses");
        apply_monorepo_defaults(&mut config);

        let rel = config.release.as_ref().unwrap();
        let extras = rel.extra_files.as_ref().unwrap();
        match &extras[0] {
            anodizer_core::config::ExtraFileSpec::Detailed { glob, .. } => {
                assert_eq!(glob, "subproj1/CHANGELOG.md");
            }
            other => panic!("expected Detailed; got {other:?}"),
        }
        match &extras[1] {
            anodizer_core::config::ExtraFileSpec::Glob(s) => {
                assert_eq!(s, "subproj1/*.sig");
            }
            other => panic!("expected Glob; got {other:?}"),
        }
    }

    #[test]
    fn monorepo_tag_prefix_missing_slash_is_suspicious() {
        // Trailing slash → fine (Category 1).
        assert!(!monorepo_tag_prefix_is_suspicious("subproject1/"));
        // Short letter prefix → fine (Category 2).
        assert!(!monorepo_tag_prefix_is_suspicious("v"));
        // Two-letter alpha prefix → fine.
        assert!(!monorepo_tag_prefix_is_suspicious("rc"));
        // Empty → no-op.
        assert!(!monorepo_tag_prefix_is_suspicious(""));
        // Missing trailing slash AND not a Category-2 short letter prefix
        // → suspicious (would produce `subproject1v1.2.3`).
        assert!(monorepo_tag_prefix_is_suspicious("subproject1"));
        // Mixed-letter-and-digit without slash → suspicious.
        assert!(monorepo_tag_prefix_is_suspicious("foo1"));
        // Single-digit without slash → suspicious (not Category-2 alpha).
        assert!(monorepo_tag_prefix_is_suspicious("1"));
    }

    #[test]
    fn monorepo_no_op_when_unconfigured() {
        let yaml = r#"
project_name: myapp
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ Version }}"
    release:
      name_template: "{{ Tag }}"
    archives:
      - files:
          - LICENSE
"#;
        let mut config: anodizer_core::config::Config =
            serde_yaml_ng::from_str(yaml).expect("yaml parses");
        apply_monorepo_defaults(&mut config);
        // No monorepo → no path mutation.
        assert_eq!(config.crates[0].path, ".");
        if let anodizer_core::config::ArchivesConfig::Configs(ref cfgs) = config.crates[0].archives
        {
            let files = cfgs[0].files.as_ref().unwrap();
            assert_eq!(files[0], "LICENSE");
        }
    }
}
