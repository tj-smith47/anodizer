//! AUR PKGBUILD + `.SRCINFO` structural and syntax validation.
//!
//! An Arch package has no JSON/YAML schema: a PKGBUILD is a Bash script that
//! `makepkg` sources, and the `.SRCINFO` is a flat `key = value` metadata
//! sidecar. `makepkg` builds, and the AUR accepts an upload, only when the
//! PKGBUILD is syntactically-valid Bash carrying the load-bearing variables
//! (`pkgname` / `pkgver` / `pkgrel`, an `arch=(…)` array, at least one
//! `source=`/`source_<arch>=` entry, a matching checksum array, and a
//! `package()` function) and the `.SRCINFO` mirrors the package identity
//! (`pkgbase` / `pkgname` / `pkgver` / `pkgrel` / `arch`). anodizer renders
//! that pair per binary-AUR crate, per source-AUR crate, and per top-level
//! `aur_sources:` entry; this validator renders the exact artifacts a live
//! publish would push — via the same render path — and checks them two ways: an
//! always-on structural floor (pure-Rust line scanning) and, when `bash` is on
//! `PATH`, a real `bash -n` syntax check of the PKGBUILD. A structural defect (a
//! missing required variable, an unbalanced template that produced broken Bash)
//! surfaces in the snapshot/dry-run pass rather than after a pushed PKGBUILD
//! fails `makepkg` for every installer.

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};

use super::{PublisherSchemaValidator, SchemaFinding};
use crate::aur::{
    AurRendered, crate_has_aur_linux_archive, is_aur_per_crate_configured,
    render_aur_pkgbuild_and_srcinfo_for_crate,
};
use crate::aur_source::{
    is_aur_source_per_crate_configured, render_aur_source_pkgbuild_and_srcinfo_for_crate,
    render_top_level_aur_source,
};

/// Validates anodizer's rendered AUR PKGBUILD + `.SRCINFO` artifacts.
pub(crate) struct AurSchemaValidator;

impl PublisherSchemaValidator for AurSchemaValidator {
    fn publisher(&self) -> &'static str {
        "aur"
    }

    fn validate(&self, ctx: &Context) -> Result<Vec<SchemaFinding>> {
        let log = ctx.logger("publish");
        let mut findings = Vec::new();

        // BINARY AUR (`publish.aur`). Walk exactly the crate set the live
        // binary-AUR publisher iterates (honoring `--crate` selection, else
        // every aur-configured crate) so the validated set equals the
        // published set in all config modes.
        let selected_bin =
            crate::publisher_helpers::effective_publish_crates(ctx, is_aur_per_crate_configured);
        for crate_name in &selected_bin {
            if !is_aur_per_crate_configured(ctx, crate_name) {
                continue;
            }

            let aur_cfg = crate::util::all_crates(ctx)
                .into_iter()
                .find(|c| &c.name == crate_name)
                .and_then(|c| c.publish)
                .and_then(|p| p.aur);

            // A real release always builds at least one Linux archive the
            // PKGBUILD points at, but a sharded / single-target snapshot may
            // build none for this crate. The probe distinguishes ABSENCE from
            // ERROR: a clean `Ok(false)` (no artifact matched) skips, while a
            // matched-but-broken artifact (missing sha256) propagates as `Err`
            // via the `?` — the same defect the live publish path bails on —
            // rather than being silently skipped.
            if let Some(aur_cfg) = aur_cfg.as_ref()
                && !crate_has_aur_linux_archive(ctx, aur_cfg, crate_name)?
            {
                log.verbose(&format!(
                    "aur: crate '{}' produced no linux archive in this snapshot shard; \
                     skipping binary PKGBUILD schema validation",
                    crate_name
                ));
                continue;
            }

            if let Some(rendered) =
                render_aur_pkgbuild_and_srcinfo_for_crate(ctx, crate_name, &log)?
            {
                validate_rendered(&mut findings, &rendered, &log)?;
            }
        }

        // SOURCE AUR per-crate (`publish.aur_source`). No built-artifact gate —
        // a source package builds from the upstream tarball, so nothing in this
        // shard could be absent; the only skip is the skip/`if` gate, evaluated
        // inside the render fn.
        let selected_src = crate::publisher_helpers::effective_publish_crates(
            ctx,
            is_aur_source_per_crate_configured,
        );
        for crate_name in &selected_src {
            if !is_aur_source_per_crate_configured(ctx, crate_name) {
                continue;
            }
            if let Some(rendered) =
                render_aur_source_pkgbuild_and_srcinfo_for_crate(ctx, crate_name, &log)?
            {
                validate_rendered(&mut findings, &rendered, &log)?;
            }
        }

        // Top-level `aur_sources:` array (not per-crate). Empty when unset or
        // every entry is skipped.
        for rendered in render_top_level_aur_source(ctx, &log)? {
            validate_rendered(&mut findings, &rendered, &log)?;
        }

        Ok(findings)
    }
}

/// Run both layers (structural floor + `bash -n`) over one rendered
/// PKGBUILD/.SRCINFO pair, appending each finding to `findings`.
fn validate_rendered(
    findings: &mut Vec<SchemaFinding>,
    rendered: &AurRendered,
    log: &StageLogger,
) -> Result<()> {
    findings.extend(validate_pkgbuild_structural(&rendered.pkgbuild));
    findings.extend(validate_srcinfo_structural(&rendered.srcinfo));
    findings.extend(validate_pkgbuild_syntax(&rendered.pkgbuild, log)?);
    Ok(())
}

fn finding(field: &str, expected: &str) -> SchemaFinding {
    SchemaFinding {
        publisher: "aur".to_string(),
        field: field.to_string(),
        expected: expected.to_string(),
    }
}

/// True when `text` carries a line that, after trimming leading whitespace,
/// begins with `<key>=` — the shape of a PKGBUILD scalar/array assignment. The
/// `=` anchor (not a bare substring) keeps a `pkgver` mention inside a comment
/// or a `pkgdesc` string from satisfying the `pkgver=` requirement.
fn has_assignment(text: &str, key: &str) -> bool {
    let prefix = format!("{key}=");
    text.lines()
        .any(|line| line.trim_start().starts_with(&prefix))
}

/// True when `text` carries a non-empty scalar assignment `<key>=<value>` whose
/// value (the run up to the first whitespace) is non-empty — used for the
/// `pkgname` / `pkgver` / `pkgrel` variables `makepkg` aborts on when empty.
/// Tolerates the `name='value'` and `name=value` quoting both renderers emit.
fn has_nonempty_assignment(text: &str, key: &str) -> bool {
    let prefix = format!("{key}=");
    text.lines().any(|line| {
        line.trim_start()
            .strip_prefix(&prefix)
            .map(|rest| {
                let value = rest.split_whitespace().next().unwrap_or("");
                let value = value.trim_matches(|c| c == '\'' || c == '"');
                !value.is_empty()
            })
            .unwrap_or(false)
    })
}

/// The checksum-array variable names `makepkg` recognizes — a base form
/// (`sha256sums`) or any `<algo>sums_<arch>` per-arch form. At least one must
/// appear, or the PKGBUILD has no integrity-check input and `makepkg` rejects
/// it (a missing checksum array is a hard error, not a `namcap` advisory).
const CHECKSUM_PREFIXES: &[&str] = &[
    "md5sums",
    "sha1sums",
    "sha224sums",
    "sha256sums",
    "sha384sums",
    "sha512sums",
    "b2sums",
    "cksums",
];

/// The always-on, hermetic structural floor for a rendered PKGBUILD: scan for
/// the variables and the `package()` function `makepkg` HARD-requires, and
/// report one [`SchemaFinding`] per missing one. An empty Vec means the script
/// clears the floor. Runs with no external tools, so it holds even where `bash`
/// is absent.
///
/// The rendered output is template-controlled, so targeted line scanning is
/// sufficient — this is deliberately NOT a Bash parser. It is LENIENT about
/// optional variables: `license` / `depends` / `optdepends` / `provides` /
/// `conflicts` / `url` are all optional per the PKGBUILD spec
/// (<https://wiki.archlinux.org/title/PKGBUILD>), so requiring them would
/// false-reject valid output — itself a defect.
pub(crate) fn validate_pkgbuild_structural(text: &str) -> Vec<SchemaFinding> {
    let mut findings = Vec::new();

    // `pkgname` / `pkgver` / `pkgrel` — the package identity makepkg aborts on
    // when any is missing or empty.
    for key in ["pkgname", "pkgver", "pkgrel"] {
        if !has_nonempty_assignment(text, key) {
            findings.push(finding(
                key,
                &format!("a PKGBUILD must carry a non-empty `{key}=` assignment"),
            ));
        }
    }

    // `arch=(…)` — the architecture array. `makepkg` requires it to know which
    // hosts the package targets.
    if !has_assignment(text, "arch") {
        findings.push(finding(
            "arch",
            "a PKGBUILD must declare an `arch=(…)` array",
        ));
    }

    // At least one `source=` / `source_<arch>=` entry — without a source there
    // is nothing for `makepkg` to fetch and build.
    let has_source = text.lines().any(|line| {
        let t = line.trim_start();
        t.starts_with("source=") || t.starts_with("source_")
    });
    if !has_source {
        findings.push(finding(
            "source",
            "a PKGBUILD must declare at least one `source=`/`source_<arch>=` entry",
        ));
    }

    // A checksum array (`sha256sums` / `b2sums` / `sha256sums_<arch>` / …) — the
    // integrity-check input `makepkg` requires alongside the sources.
    let has_checksum = text.lines().any(|line| {
        let t = line.trim_start();
        CHECKSUM_PREFIXES.iter().any(|p| {
            t.strip_prefix(p)
                .is_some_and(|rest| rest.starts_with('=') || rest.starts_with('_'))
        })
    });
    if !has_checksum {
        findings.push(finding(
            "sha256sums",
            "a PKGBUILD must declare a checksum array (sha256sums/sha512sums/b2sums/…) \
             matching its sources",
        ));
    }

    // `package()` — the function `makepkg` calls to stage files into `$pkgdir`.
    // Without it the package installs nothing.
    let has_package_fn = text.lines().any(|line| {
        let t = line.trim_start();
        t.starts_with("package()") || t.starts_with("package ()")
    });
    if !has_package_fn {
        findings.push(finding(
            "package",
            "a PKGBUILD must define a `package()` function",
        ));
    }

    findings
}

/// `.SRCINFO` keys the AUR requires — the package identity it indexes a
/// submission by. Each is matched as a `<key> =` assignment (the `.SRCINFO`
/// `key = value` shape, with `pkgbase`/`pkgname` flush-left and the per-pkg
/// keys tab-indented).
const REQUIRED_SRCINFO_KEYS: &[&str] = &["pkgbase", "pkgname", "pkgver", "pkgrel", "arch"];

/// The always-on, hermetic structural floor for a rendered `.SRCINFO`: assert
/// each AUR-required identity key is present as a `<key> = …` assignment.
/// Returns one [`SchemaFinding`] per missing key. Lenient about optional keys
/// (`url` / `license` / `depends` / … are optional) — requiring them would
/// false-reject valid metadata.
pub(crate) fn validate_srcinfo_structural(text: &str) -> Vec<SchemaFinding> {
    let mut findings = Vec::new();
    for &key in REQUIRED_SRCINFO_KEYS {
        // `.SRCINFO` assignments are `<key> = <value>`; match the key followed
        // by ` =` after trimming the leading tab so a `pkgver` substring inside
        // a value (e.g. a `pkgdesc` mention) does not satisfy `pkgver`.
        let prefix = format!("{key} =");
        let present = text
            .lines()
            .any(|line| line.trim_start().starts_with(&prefix));
        if !present {
            findings.push(finding(
                key,
                &format!(".SRCINFO must carry a `{key} = …` line"),
            ));
        }
    }
    findings
}

/// The gated layer: when `bash` is on `PATH`, write the rendered PKGBUILD to a
/// tempfile and run `bash -n <file>`. A non-zero exit means a Bash syntax error
/// in the generated PKGBUILD — parse each `<file>: line N: <message>` stderr
/// line into a [`SchemaFinding`]. A non-zero exit with no parseable line still
/// yields a `(root)` finding (never silent-pass). When `bash` is absent, log a
/// visible skip marker and return no findings — the structural floor stands; a
/// missing tool is never a manifest defect.
fn validate_pkgbuild_syntax(pkgbuild: &str, log: &StageLogger) -> Result<Vec<SchemaFinding>> {
    if !anodizer_core::tool_detect::tool_available("bash").unwrap_or(false) {
        log.verbose(
            "aur: bash not on PATH; relying on the structural PKGBUILD floor for \
             syntax validation",
        );
        return Ok(Vec::new());
    }

    let dir = tempfile::tempdir().context("aur: create temp dir for bash -n validation")?;
    let pkgbuild_path = dir.path().join("PKGBUILD");
    std::fs::write(&pkgbuild_path, pkgbuild).context("aur: write rendered PKGBUILD for bash -n")?;

    let output = std::process::Command::new("bash")
        .arg("-n")
        .arg(&pkgbuild_path)
        .output()
        .context("aur: run bash -n")?;
    if output.status.success() {
        return Ok(Vec::new());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut findings = parse_bash_n_stderr(&stderr);
    // A non-zero exit with no parseable syntax line means bash rejected the
    // file for a reason this parser didn't recognize (or bash itself errored).
    // Returning an empty Vec here would silently report a failed validator as
    // PASS, so surface a fallback finding carrying the raw stderr.
    if findings.is_empty() {
        let trimmed = stderr.trim();
        let expected = if trimmed.is_empty() {
            "bash -n reported the generated PKGBUILD invalid but emitted no parseable diagnostic"
                .to_string()
        } else {
            trimmed.to_string()
        };
        findings.push(finding("(root)", &expected));
    }
    Ok(findings)
}

/// Parse `bash -n` stderr into [`SchemaFinding`]s. A syntax error line has the
/// shape `<file>: line <N>: <message>` (e.g.
/// `/tmp/x/PKGBUILD: line 12: syntax error near unexpected token …`); the line
/// number becomes the finding field and the message its expectation. Lines
/// without a `line <digits>:` position are ignored.
///
/// The `<file>` prefix is a full tempfile path that could itself contain `: `,
/// so the parser anchors on the `: line ` marker (not the first `: `) and reads
/// the digits after it — robust to any path shape `bash` emits.
fn parse_bash_n_stderr(stderr: &str) -> Vec<SchemaFinding> {
    const MARKER: &str = ": line ";
    stderr
        .lines()
        .filter_map(|line| {
            // Anchor on the LAST `: line ` so a path containing the marker
            // text does not mis-split before the real position.
            let after = &line[line.rfind(MARKER)? + MARKER.len()..];
            let (lineno, msg) = after.split_once(':')?;
            let lineno = lineno.trim();
            if lineno.is_empty() || !lineno.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            Some(finding(&format!("line {lineno}"), msg.trim()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::{
        Amd64Variant, AurConfig, AurSourceConfig, CrateConfig, PublishConfig, ReleaseConfig,
        ScmRepoConfig, StringOrBool,
    };
    use anodizer_core::context::Context;
    use anodizer_core::test_helpers::TestContextBuilder;

    use super::*;

    /// An `AurConfig` (binary AUR) exercising the PKGBUILD-affecting options
    /// with values makepkg/AUR accept.
    fn every_option_aur_cfg() -> AurConfig {
        AurConfig {
            name: Some("widget-bin".to_string()),
            description: Some("A widget management tool".to_string()),
            homepage: Some("https://acme.example/widget".to_string()),
            license: Some("MIT".to_string()),
            maintainers: Some(vec!["Acme Corp <dev@acme.example>".to_string()]),
            contributors: Some(vec!["A Contributor".to_string()]),
            depends: Some(vec!["glibc".to_string()]),
            optdepends: Some(vec!["fzf: fuzzy finder support".to_string()]),
            provides: Some(vec!["widget".to_string()]),
            conflicts: Some(vec!["widget".to_string()]),
            backup: Some(vec!["etc/widget.conf".to_string()]),
            git_url: Some("ssh://aur@aur.archlinux.org/widget-bin.git".to_string()),
            ..Default::default()
        }
    }

    /// An `AurSourceConfig` (source AUR) exercising the PKGBUILD-affecting
    /// options. `url_template` is set explicitly so the source tarball URL is
    /// deterministic regardless of GitURL extraction.
    fn every_option_aur_source_cfg() -> AurSourceConfig {
        AurSourceConfig {
            name: Some("widget".to_string()),
            description: Some("A widget management tool".to_string()),
            homepage: Some("https://acme.example/widget".to_string()),
            license: Some("MIT".to_string()),
            maintainers: Some(vec!["Acme Corp <dev@acme.example>".to_string()]),
            depends: Some(vec!["glibc".to_string()]),
            makedepends: Some(vec!["rust".to_string(), "cargo".to_string()]),
            optdepends: Some(vec!["fzf: fuzzy finder support".to_string()]),
            provides: Some(vec!["widget".to_string()]),
            conflicts: Some(vec!["widget".to_string()]),
            url_template: Some(
                "https://github.com/acme/widget/archive/refs/tags/{{ .Tag }}.tar.gz".to_string(),
            ),
            ..Default::default()
        }
    }

    /// A crate carrying a binary-AUR `publish.aur` block.
    fn aur_crate(crate_name: &str, tag_template: &str, cfg: AurConfig) -> CrateConfig {
        CrateConfig {
            name: crate_name.to_string(),
            path: ".".to_string(),
            tag_template: tag_template.to_string(),
            release: Some(ReleaseConfig {
                github: Some(ScmRepoConfig {
                    owner: "acme".to_string(),
                    name: "widget".to_string(),
                }),
                ..Default::default()
            }),
            publish: Some(PublishConfig {
                aur: Some(cfg),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// A crate carrying a source-AUR `publish.aur_source` block.
    fn aur_source_crate(crate_name: &str, tag_template: &str, cfg: AurSourceConfig) -> CrateConfig {
        CrateConfig {
            name: crate_name.to_string(),
            path: ".".to_string(),
            tag_template: tag_template.to_string(),
            release: Some(ReleaseConfig {
                github: Some(ScmRepoConfig {
                    owner: "acme".to_string(),
                    name: "widget".to_string(),
                }),
                ..Default::default()
            }),
            publish: Some(PublishConfig {
                aur_source: Some(cfg),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// Re-scope the global template vars to the version a release would stamp,
    /// the same shape the publish stage applies before invoking a per-crate
    /// publisher.
    fn scope_version(ctx: &mut Context, version: &str) {
        ctx.template_vars_mut().set("Version", version);
        ctx.template_vars_mut().set("RawVersion", version);
        ctx.template_vars_mut().set("Tag", &format!("v{version}"));
    }

    /// Add a linux tar.gz archive carrying the url + sha256 the binary-AUR
    /// PKGBUILD `source_<arch>=`/`sha256sums_<arch>=` arrays key off.
    fn add_linux_archive(ctx: &mut Context, crate_name: &str, version: &str) {
        let target = "x86_64-unknown-linux-gnu";
        let mut meta = HashMap::new();
        meta.insert(
            "url".to_string(),
            format!(
                "https://github.com/acme/widget/releases/download/v{version}/{crate_name}-{target}.tar.gz"
            ),
        );
        meta.insert("sha256".to_string(), "a".repeat(64));
        meta.insert("format".to_string(), "tar.gz".to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: std::path::PathBuf::from(format!("/dist/{crate_name}-{target}.tar.gz")),
            name: format!("{crate_name}-{target}.tar.gz"),
            target: Some(target.to_string()),
            crate_name: crate_name.to_string(),
            metadata: meta,
            size: None,
        });
    }

    /// (a) Single-crate mode, BINARY AUR: one crate, every option set. The
    /// rendered PKGBUILD + .SRCINFO must clear both structural floors with zero
    /// findings and stamp the key fields a release ships.
    #[test]
    fn single_crate_binary_every_option_validates_and_lands_in_fields() {
        let cfg = every_option_aur_cfg();
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![aur_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_linux_archive(&mut ctx, "widget", "1.0.0");

        let findings = AurSchemaValidator.validate(&ctx).expect("validation runs");
        assert!(
            findings.is_empty(),
            "every-option single-crate binary PKGBUILD must conform, got: {findings:?}"
        );

        let rendered =
            render_aur_pkgbuild_and_srcinfo_for_crate(&ctx, "widget", &ctx.logger("publish"))
                .expect("render ok")
                .expect("not skipped");
        let pkgbuild = &rendered.pkgbuild;
        assert!(
            pkgbuild
                .lines()
                .any(|l| l.trim_start().starts_with("pkgname='widget-bin'")),
            "PKGBUILD stamps pkgname, got: {pkgbuild}"
        );
        assert!(
            pkgbuild
                .lines()
                .any(|l| l.trim_start().starts_with("pkgver=1.0.0")),
            "PKGBUILD stamps pkgver, got: {pkgbuild}"
        );
        assert!(
            pkgbuild
                .lines()
                .any(|l| l.trim_start().starts_with("arch=(")),
            "PKGBUILD carries arch array, got: {pkgbuild}"
        );
        assert!(
            pkgbuild
                .lines()
                .any(|l| l.trim_start().starts_with("source_x86_64=")),
            "PKGBUILD carries a per-arch source, got: {pkgbuild}"
        );
        assert!(
            pkgbuild
                .lines()
                .any(|l| l.trim_start().starts_with("sha256sums_x86_64=")),
            "PKGBUILD carries a per-arch checksum, got: {pkgbuild}"
        );
        assert!(
            rendered
                .srcinfo
                .lines()
                .any(|l| l.trim_start().starts_with("pkgbase = widget-bin")),
            "SRCINFO stamps pkgbase, got: {}",
            rendered.srcinfo
        );
    }

    /// (a) Single-crate mode, SOURCE AUR: one crate, every option set. The
    /// rendered source PKGBUILD + .SRCINFO must clear both floors and stamp the
    /// key fields.
    #[test]
    fn single_crate_source_every_option_validates_and_lands_in_fields() {
        let cfg = every_option_aur_source_cfg();
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![aur_source_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");

        let findings = AurSchemaValidator.validate(&ctx).expect("validation runs");
        assert!(
            findings.is_empty(),
            "every-option single-crate source PKGBUILD must conform, got: {findings:?}"
        );

        let rendered = render_aur_source_pkgbuild_and_srcinfo_for_crate(
            &ctx,
            "widget",
            &ctx.logger("publish"),
        )
        .expect("render ok")
        .expect("not skipped");
        let pkgbuild = &rendered.pkgbuild;
        assert!(
            pkgbuild
                .lines()
                .any(|l| l.trim_start().starts_with("pkgname='widget'")),
            "source PKGBUILD stamps pkgname, got: {pkgbuild}"
        );
        assert!(
            pkgbuild
                .lines()
                .any(|l| l.trim_start().starts_with("pkgver='1.0.0'")),
            "source PKGBUILD stamps pkgver, got: {pkgbuild}"
        );
        assert!(
            pkgbuild
                .lines()
                .any(|l| l.trim_start().starts_with("arch=(")),
            "source PKGBUILD carries arch array, got: {pkgbuild}"
        );
        assert!(
            pkgbuild
                .lines()
                .any(|l| l.trim_start().starts_with("source=(")),
            "source PKGBUILD carries a source array, got: {pkgbuild}"
        );
        assert!(
            pkgbuild
                .lines()
                .any(|l| l.trim_start().starts_with("sha256sums=(")),
            "source PKGBUILD carries a checksum array, got: {pkgbuild}"
        );
        assert!(
            pkgbuild
                .lines()
                .any(|l| l.trim_start().starts_with("package()")),
            "source PKGBUILD defines package(), got: {pkgbuild}"
        );
        assert!(
            rendered
                .srcinfo
                .lines()
                .any(|l| l.trim_start().starts_with("pkgbase = widget")),
            "source SRCINFO stamps pkgbase, got: {}",
            rendered.srcinfo
        );
    }

    /// (b) Workspace-lockstep mode: multiple crates share one global version.
    /// Each crate's binary + source AUR artifacts must validate independently.
    #[test]
    fn workspace_lockstep_every_option_validates() {
        let alpha = aur_crate(
            "alpha",
            "v{{ .Version }}",
            AurConfig {
                name: Some("alpha-bin".to_string()),
                git_url: Some("ssh://aur@aur.archlinux.org/alpha-bin.git".to_string()),
                ..every_option_aur_cfg()
            },
        );
        let beta = aur_source_crate(
            "beta",
            "v{{ .Version }}",
            AurSourceConfig {
                name: Some("beta".to_string()),
                url_template: Some(
                    "https://github.com/acme/beta/archive/refs/tags/{{ .Tag }}.tar.gz".to_string(),
                ),
                ..every_option_aur_source_cfg()
            },
        );
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![alpha, beta])
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_linux_archive(&mut ctx, "alpha", "1.0.0");

        let findings = AurSchemaValidator.validate(&ctx).expect("validation runs");
        assert!(
            findings.is_empty(),
            "lockstep workspace binary + source AUR must conform, got: {findings:?}"
        );
    }

    /// (c) Workspace per-crate mode: each crate carries its own tag_template /
    /// version. The publish stage scopes the global `Version` to the per-crate
    /// value before invoking the publisher, so each PKGBUILD must stamp its own
    /// pkgver under the per-crate version.
    #[test]
    fn workspace_per_crate_every_option_validates_under_own_version() {
        let alpha = aur_crate(
            "alpha",
            "alpha-v{{ .Version }}",
            AurConfig {
                name: Some("alpha-bin".to_string()),
                git_url: Some("ssh://aur@aur.archlinux.org/alpha-bin.git".to_string()),
                ..every_option_aur_cfg()
            },
        );
        let beta = aur_crate(
            "beta",
            "beta-v{{ .Version }}",
            AurConfig {
                name: Some("beta-bin".to_string()),
                git_url: Some("ssh://aur@aur.archlinux.org/beta-bin.git".to_string()),
                ..every_option_aur_cfg()
            },
        );

        let mut ctx_a = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![alpha.clone(), beta.clone()])
            .selected_crates(vec!["alpha".to_string()])
            .build();
        scope_version(&mut ctx_a, "2.0.0");
        add_linux_archive(&mut ctx_a, "alpha", "2.0.0");
        let findings_a = AurSchemaValidator
            .validate(&ctx_a)
            .expect("validation runs");
        assert!(
            findings_a.is_empty(),
            "per-crate alpha@2.0.0 must conform, got: {findings_a:?}"
        );
        let rendered_a =
            render_aur_pkgbuild_and_srcinfo_for_crate(&ctx_a, "alpha", &ctx_a.logger("publish"))
                .expect("render ok")
                .expect("not skipped");
        assert!(
            rendered_a
                .pkgbuild
                .lines()
                .any(|l| l.trim_start().starts_with("pkgver=2.0.0")),
            "alpha PKGBUILD stamps its own pkgver, got: {}",
            rendered_a.pkgbuild
        );

        let mut ctx_b = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![alpha, beta])
            .selected_crates(vec!["beta".to_string()])
            .build();
        scope_version(&mut ctx_b, "3.1.0");
        add_linux_archive(&mut ctx_b, "beta", "3.1.0");
        let findings_b = AurSchemaValidator
            .validate(&ctx_b)
            .expect("validation runs");
        assert!(
            findings_b.is_empty(),
            "per-crate beta@3.1.0 must conform, got: {findings_b:?}"
        );
        let rendered_b =
            render_aur_pkgbuild_and_srcinfo_for_crate(&ctx_b, "beta", &ctx_b.logger("publish"))
                .expect("render ok")
                .expect("not skipped");
        assert!(
            rendered_b
                .pkgbuild
                .lines()
                .any(|l| l.trim_start().starts_with("pkgver=3.1.0")),
            "beta PKGBUILD stamps its own pkgver, got: {}",
            rendered_b.pkgbuild
        );
    }

    /// A single-target / sharded snapshot that built no linux archive for a
    /// binary-AUR-configured crate must SKIP it (zero findings, no error)
    /// rather than trip the publisher's "no linux archives matched" guard.
    #[test]
    fn binary_crate_without_matching_artifact_is_skipped_not_failed() {
        let cfg = every_option_aur_cfg();
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![aur_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        // No archive artifact at all in this shard.

        let findings = AurSchemaValidator
            .validate(&ctx)
            .expect("validation runs without erroring on the absent archive");
        assert!(
            findings.is_empty(),
            "a binary-AUR crate with no archive in this shard must be skipped, got: {findings:?}"
        );
    }

    /// A binary-AUR crate WITH a matched Linux archive that is MISSING its
    /// sha256 must NOT be silently skipped as if the artifact were absent: the
    /// probe distinguishes absence (`Ok(false)`) from a broken-artifact ERROR,
    /// so `validate()` surfaces the same defect the live publish path bails on.
    #[test]
    fn binary_crate_with_matched_archive_missing_sha256_is_not_skipped() {
        let cfg = every_option_aur_cfg();
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![aur_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");

        // A matched linux archive whose sha256 metadata is absent — exactly the
        // state a missing/incomplete checksum stage leaves behind.
        let target = "x86_64-unknown-linux-gnu";
        let mut meta = HashMap::new();
        meta.insert(
            "url".to_string(),
            format!(
                "https://github.com/acme/widget/releases/download/v1.0.0/widget-{target}.tar.gz"
            ),
        );
        meta.insert("format".to_string(), "tar.gz".to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: std::path::PathBuf::from(format!("/dist/widget-{target}.tar.gz")),
            name: format!("widget-{target}.tar.gz"),
            target: Some(target.to_string()),
            crate_name: "widget".to_string(),
            metadata: meta,
            size: None,
        });

        let result = AurSchemaValidator.validate(&ctx);
        assert!(
            result.is_err(),
            "a matched-but-broken (missing sha256) archive must surface as an error, \
             not a silent skip; got: {result:?}"
        );
    }

    /// A binary-AUR crate whose `if:` renders falsy must be skipped:
    /// `render_aur_pkgbuild_and_srcinfo_for_crate` returns `None`.
    #[test]
    fn binary_crate_with_falsy_if_is_skipped() {
        let cfg = AurConfig {
            if_condition: Some("false".to_string()),
            ..every_option_aur_cfg()
        };
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![aur_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_linux_archive(&mut ctx, "widget", "1.0.0");

        let rendered =
            render_aur_pkgbuild_and_srcinfo_for_crate(&ctx, "widget", &ctx.logger("publish"))
                .expect("render ok");
        assert!(
            rendered.is_none(),
            "a falsy `if` must skip the binary-AUR crate, got a rendered PKGBUILD"
        );

        let findings = AurSchemaValidator.validate(&ctx).expect("validation runs");
        assert!(
            findings.is_empty(),
            "a skipped crate yields no findings, got: {findings:?}"
        );
    }

    /// A source-AUR entry with a truthy `skip` must render to `None`.
    #[test]
    fn source_crate_with_skip_is_skipped() {
        let cfg = AurSourceConfig {
            skip: Some(StringOrBool::Bool(true)),
            ..every_option_aur_source_cfg()
        };
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![aur_source_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");

        let rendered = render_aur_source_pkgbuild_and_srcinfo_for_crate(
            &ctx,
            "widget",
            &ctx.logger("publish"),
        )
        .expect("render ok");
        assert!(
            rendered.is_none(),
            "a truthy `skip` must suppress the source-AUR entry"
        );

        let findings = AurSchemaValidator.validate(&ctx).expect("validation runs");
        assert!(
            findings.is_empty(),
            "a skipped source entry yields no findings, got: {findings:?}"
        );
    }

    /// The configured `amd64_variant` is scoped as the `{{ .Amd64 }}` template
    /// var for the source entry's per-entry renders: a `url_template`
    /// referencing it must resolve to the configured variant (here `v3`), not a
    /// stale/empty global value. This proves the scoped-vars refactor keeps
    /// `Amd64` in scope for the same template surface the global-mutation
    /// version did.
    #[test]
    fn source_amd64_variant_is_scoped_for_url_template() {
        let cfg = AurSourceConfig {
            name: Some("widget".to_string()),
            amd64_variant: Some(Amd64Variant::V3),
            url_template: Some(
                "https://acme.example/{{ .Amd64 }}/widget-{{ .Version }}.tar.gz".to_string(),
            ),
            ..every_option_aur_source_cfg()
        };
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![aur_source_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");

        let rendered = render_aur_source_pkgbuild_and_srcinfo_for_crate(
            &ctx,
            "widget",
            &ctx.logger("publish"),
        )
        .expect("render ok")
        .expect("not skipped");
        assert!(
            rendered
                .pkgbuild
                .contains("https://acme.example/v3/widget-1.0.0.tar.gz"),
            "the url_template must resolve {{ .Amd64 }} to the configured v3, got: {}",
            rendered.pkgbuild
        );
        // Sanity: the .SRCINFO source line carries the same resolved URL.
        assert!(
            rendered.srcinfo.contains("https://acme.example/v3/"),
            ".SRCINFO source carries the Amd64-resolved url, got: {}",
            rendered.srcinfo
        );
    }

    /// Top-level `aur_sources:` array entries are rendered and validated.
    #[test]
    fn top_level_aur_source_array_validates() {
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .project_name("widget")
            .build();
        ctx.config.aur_sources = Some(vec![every_option_aur_source_cfg()]);
        scope_version(&mut ctx, "1.0.0");
        ctx.template_vars_mut().set("ProjectName", "widget");

        let rendered =
            render_top_level_aur_source(&ctx, &ctx.logger("publish")).expect("render ok");
        assert_eq!(rendered.len(), 1, "one top-level source entry renders");

        let findings = AurSchemaValidator.validate(&ctx).expect("validation runs");
        assert!(
            findings.is_empty(),
            "top-level aur_sources entry must conform, got: {findings:?}"
        );
    }

    /// The PKGBUILD structural floor must BITE: a script missing `package()` is
    /// reported, and the corrected script produces zero findings.
    #[test]
    fn pkgbuild_missing_package_fn_is_reported_and_fix_clears_it() {
        let broken = "pkgname='widget'\npkgver=1.0.0\npkgrel=1\narch=('x86_64')\nsource=(\"x.tar.gz\")\nsha256sums=('abc')\n";
        let findings = validate_pkgbuild_structural(broken);
        assert!(
            findings.iter().any(|f| f.field == "package"),
            "a PKGBUILD missing package() must be reported, got: {findings:?}"
        );

        let fixed = format!("{broken}package() {{\n  true\n}}\n");
        let fixed_findings = validate_pkgbuild_structural(&fixed);
        assert!(
            fixed_findings.is_empty(),
            "the corrected PKGBUILD must produce zero findings, got: {fixed_findings:?}"
        );
    }

    /// The PKGBUILD structural floor must BITE on a missing checksum array.
    #[test]
    fn pkgbuild_missing_checksum_is_reported() {
        let broken = "pkgname='widget'\npkgver=1.0.0\npkgrel=1\narch=('x86_64')\nsource=(\"x.tar.gz\")\npackage() {\n  true\n}\n";
        let findings = validate_pkgbuild_structural(broken);
        assert!(
            findings.iter().any(|f| f.field == "sha256sums"),
            "a PKGBUILD missing a checksum array must be reported, got: {findings:?}"
        );
    }

    /// The .SRCINFO structural floor must BITE: a metadata file missing `pkgver`
    /// is reported, and the corrected file produces zero findings.
    #[test]
    fn srcinfo_missing_pkgver_is_reported_and_fix_clears_it() {
        let broken = "pkgbase = widget\n\tpkgrel = 1\n\tarch = x86_64\npkgname = widget\n";
        let findings = validate_srcinfo_structural(broken);
        assert!(
            findings.iter().any(|f| f.field == "pkgver"),
            "a .SRCINFO missing pkgver must be reported, got: {findings:?}"
        );

        let fixed =
            "pkgbase = widget\n\tpkgver = 1.0.0\n\tpkgrel = 1\n\tarch = x86_64\npkgname = widget\n";
        let fixed_findings = validate_srcinfo_structural(fixed);
        assert!(
            fixed_findings.is_empty(),
            "the corrected .SRCINFO must produce zero findings, got: {fixed_findings:?}"
        );
    }

    /// A `pkgver` mention buried inside a comment / pkgdesc value must NOT
    /// satisfy the `pkgver=` requirement — guards against substring false-passes.
    #[test]
    fn pkgver_inside_comment_does_not_satisfy_requirement() {
        let broken = "# this pkgver= is just a comment\npkgname='widget'\npkgdesc=\"see pkgver=X\"\npkgrel=1\narch=('x86_64')\nsource=(\"x\")\nsha256sums=('a')\npackage() {\n  true\n}\n";
        let findings = validate_pkgbuild_structural(broken);
        assert!(
            findings.iter().any(|f| f.field == "pkgver"),
            "a pkgver only mentioned in a comment/string must still be reported, got: {findings:?}"
        );
    }

    /// The `bash -n` stderr parser maps a `<file>: line <N>: <message>` syntax
    /// line to a finding whose field is the line number and whose expectation is
    /// the message. Exercises the REAL full-path prefix `bash` emits (a tempdir
    /// path that itself contains `: line `-adjacent segments would still parse
    /// because the marker is anchored on the LAST `: line `). Holds even where
    /// bash itself is absent.
    #[test]
    fn bash_n_stderr_parses_into_findings() {
        let stderr = "/tmp/.tmpAb12/PKGBUILD: line 12: syntax error near unexpected token `}'\n\
             /tmp/.tmpAb12/PKGBUILD: line 12: `}'\n";
        let findings = parse_bash_n_stderr(stderr);
        assert!(
            findings.iter().any(|f| f.field == "line 12"),
            "a full-path syntax line maps to its line-number field, got: {findings:?}"
        );
        assert_eq!(findings[0].publisher, "aur");
        assert!(
            findings[0].expected.contains("syntax error"),
            "expectation carries the diagnostic, got: {}",
            findings[0].expected
        );

        // A path that itself embeds `: line ` must still resolve to the real
        // trailing position, not the path-internal one.
        let tricky = "/tmp/x: line 99/PKGBUILD: line 7: unexpected EOF\n";
        let tricky_findings = parse_bash_n_stderr(tricky);
        assert!(
            tricky_findings.iter().any(|f| f.field == "line 7"),
            "the LAST `: line ` marker wins, got: {tricky_findings:?}"
        );
    }

    /// The `bash -n` layer must accept the every-option binary PKGBUILD: render
    /// it and run it through the REAL `bash -n`, asserting zero findings.
    /// Skipped (with a visible marker) when `bash` is not on `PATH`.
    #[test]
    fn bash_n_accepts_every_option_pkgbuild() {
        let cfg = every_option_aur_cfg();
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![aur_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_linux_archive(&mut ctx, "widget", "1.0.0");
        let log = ctx.logger("publish");

        if !anodizer_core::tool_detect::tool_available("bash").unwrap_or(false) {
            log.status("SKIP bash_n_accepts_every_option_pkgbuild: bash not on PATH (syntax layer unexercised)");
            return;
        }

        let rendered = render_aur_pkgbuild_and_srcinfo_for_crate(&ctx, "widget", &log)
            .expect("render ok")
            .expect("not skipped");
        let findings = validate_pkgbuild_syntax(&rendered.pkgbuild, &log).expect("bash -n runs");
        assert!(
            findings.is_empty(),
            "the every-option PKGBUILD must pass bash -n, got: {findings:?}"
        );
    }

    /// The `bash -n` layer must BITE: a syntactically-broken PKGBUILD (an
    /// unclosed function brace) must produce a finding from the real `bash -n`.
    /// Skipped (with a visible marker) when `bash` is not on `PATH`.
    #[test]
    fn bash_n_rejects_broken_pkgbuild() {
        let ctx = TestContextBuilder::new().snapshot(true).build();
        let log = ctx.logger("publish");

        if !anodizer_core::tool_detect::tool_available("bash").unwrap_or(false) {
            log.status(
                "SKIP bash_n_rejects_broken_pkgbuild: bash not on PATH (syntax layer unexercised)",
            );
            return;
        }

        let broken = "pkgname='widget'\npkgver=1.0.0\npkgrel=1\narch=('x86_64')\npackage() {\n  install -Dm755 x y\n";
        let findings = validate_pkgbuild_syntax(broken, &log).expect("bash -n runs");
        assert!(
            !findings.is_empty(),
            "a syntactically-broken PKGBUILD must produce a bash -n finding"
        );
    }
}
