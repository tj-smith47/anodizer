//! AppImage packaging stage.
//!
//! Bundles a built Linux binary plus its desktop integration (a `.desktop`
//! entry + icon, an optional harvested runtime tree, and arbitrary extra
//! files) into a single self-contained, runnable `.AppImage` via
//! [`linuxdeploy`](https://github.com/linuxdeploy/linuxdeploy)'s `appimage`
//! output plugin.
//!
//! One `.AppImage` is produced per matching Linux target, so a multi-arch
//! build yields distinct, non-colliding outputs. The runtime-harvest hook
//! (helix's `hx --grammar fetch`-style step) runs ONCE on the host-native
//! binary — the harvested data (grammars / themes / queries) is
//! architecture-independent, so it is reused for every target's AppImage and
//! also staged at a stable dist path (`dist/.appimage-runtime/<id>/`) so an
//! archive `extra_files` glob can ship the same tree in tarballs.
//!
//! linuxdeploy is invoked as:
//!
//! ```text
//! linuxdeploy --appdir <AppDir> -d <desktop> -i <icon> --output appimage [extra_args...]
//! ```
//!
//! with the env it reads (`VERSION`, `ARCH`, `APP`, `OUTPUT`, and optionally
//! `UPDATE_INFORMATION`) set on the process from config + context.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, bail};

use anodizer_core::arch_path_guard::ArchPathGuard;
use anodizer_core::artifact::{Artifact, ArtifactKind, matches_id_filter};
use anodizer_core::context::Context;
use anodizer_core::stage::Stage;

// ---------------------------------------------------------------------------
// linuxdeploy command construction (pure — independently testable)
// ---------------------------------------------------------------------------

/// Build the linuxdeploy argument vector for one AppDir.
///
/// `linuxdeploy --appdir <AppDir> -d <desktop> --output appimage`, with any
/// user `extra_args` appended last.
///
/// The icon is deliberately NOT passed via `-i`: [`assemble_appdir`] pre-places
/// it into the AppDir's icon-theme tree under the exact `Icon=` name from the
/// desktop file, and linuxdeploy resolves it from there. Passing `-i` instead
/// would (a) deploy under the icon FILE's basename, which need not match the
/// desktop `Icon=` key, and (b) hard-reject any icon whose pixel resolution is
/// outside linuxdeploy's fixed accepted set (max 512x512) — a perfectly
/// ordinary 1024x1024 source icon would abort the build. An icon already
/// present in the theme tree is not resolution-checked, so pre-placing accepts
/// any size verbatim.
fn linuxdeploy_args(appdir: &Path, desktop: &Path, extra_args: &[String]) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "--appdir".into(),
        appdir.as_os_str().to_os_string(),
        "-d".into(),
        desktop.as_os_str().to_os_string(),
        "--output".into(),
        "appimage".into(),
    ];
    for a in extra_args {
        args.push(a.into());
    }
    args
}

/// Build the env map linuxdeploy reads for one AppImage.
///
/// `VERSION` / `ARCH` / `APP` are always set, plus the output filename under
/// BOTH `OUTPUT` (legacy) and `LDAI_OUTPUT` (the appimage plugin's current
/// name for it) so the plugin writes the AppImage directly under `out_filename`
/// in linuxdeploy's cwd. The plugin is selected by the `--output appimage` CLI
/// arg (see [`linuxdeploy_args`]); `OUTPUT` is the output FILENAME, not a plugin
/// selector — setting it to `appimage` makes the plugin emit a file literally
/// named `appimage` with no `.AppImage` extension. `UPDATE_INFORMATION` is set
/// only when configured (absent otherwise, so the AppImage carries no zsync
/// metadata — matching linuxdeploy's default).
fn linuxdeploy_env(
    version: &str,
    arch: &str,
    app: &str,
    out_filename: &str,
    update_information: Option<&str>,
) -> Vec<(String, String)> {
    let mut env = vec![
        ("VERSION".to_string(), version.to_string()),
        ("ARCH".to_string(), arch.to_string()),
        ("APP".to_string(), app.to_string()),
        ("OUTPUT".to_string(), out_filename.to_string()),
        ("LDAI_OUTPUT".to_string(), out_filename.to_string()),
    ];
    if let Some(ui) = update_information {
        env.push(("UPDATE_INFORMATION".to_string(), ui.to_string()));
    }
    env
}

/// Map a Rust target triple's architecture to the AppImage `ARCH` token
/// linuxdeploy expects (`x86_64`, `aarch64`, `armhf`, `i686`). Falls back to
/// the anodizer arch label when no AppImage-specific mapping applies.
fn appimage_arch(target: &str) -> String {
    let first = target.split('-').next().unwrap_or("");
    match first {
        "x86_64" => "x86_64".to_string(),
        "aarch64" => "aarch64".to_string(),
        "armv7" | "armv7l" | "arm" | "armv6" | "armv6l" => "armhf".to_string(),
        "i686" | "i586" | "i386" => "i686".to_string(),
        other if !other.is_empty() => other.to_string(),
        _ => {
            let (_, arch) = anodizer_core::target::map_target(target);
            arch
        }
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Reject duplicate AppImage config IDs (per-id `default` collapses unkeyed
/// entries onto one slot — same shape as makeself/nfpm validation).
fn validate_unique_ids(configs: &[anodizer_core::config::AppImageConfig]) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for cfg in configs {
        let id = cfg.id.as_deref().unwrap_or("default");
        if !seen.insert(id.to_string()) {
            bail!("appimage: duplicate id '{}'", id);
        }
    }
    Ok(())
}

/// Validate the required fields of a single AppImage config.
fn validate_config_fields(cfg: &anodizer_core::config::AppImageConfig, id: &str) -> Result<()> {
    if cfg.desktop.as_deref().unwrap_or("").is_empty() {
        bail!("appimage: 'desktop' is required for config id '{}'", id);
    }
    if cfg.icon.as_deref().unwrap_or("").is_empty() {
        bail!("appimage: 'icon' is required for config id '{}'", id);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Binary selection (mirrors makeself's collect_matching_binaries)
// ---------------------------------------------------------------------------

/// Filter and clone the binary artifacts that match an AppImage config's
/// id-filter + os/arch selectors. AppImage is Linux-only, so the os filter
/// defaults to `["linux"]`.
fn collect_matching_binaries(
    ctx: &Context,
    cfg: &anodizer_core::config::AppImageConfig,
    os_filter: &[String],
) -> Vec<Artifact> {
    ctx.artifacts
        .all()
        .iter()
        .filter(|a| {
            matches!(
                a.kind,
                ArtifactKind::Binary
                    | ArtifactKind::UploadableBinary
                    | ArtifactKind::UniversalBinary
            )
        })
        .filter(|a| matches_id_filter(a, cfg.ids.as_deref()))
        .filter(|a| {
            if let Some(ref target) = a.target {
                let (os, _) = anodizer_core::target::map_target(target);
                os_filter.iter().any(|o| o == &os)
            } else {
                false
            }
        })
        .filter(|a| {
            if let Some(ref arch_filter) = cfg.arch {
                if let Some(ref target) = a.target {
                    let (_, arch) = anodizer_core::target::map_target(target);
                    arch_filter.iter().any(|a| a == &arch)
                } else {
                    false
                }
            } else {
                true
            }
        })
        .cloned()
        .collect()
}

/// Resolve the host-native binary for the runtime-harvest step: the built
/// artifact whose target equals the detected host target. `None` when host
/// detection fails (e.g. `rustc` unavailable) or no built artifact targets
/// the host (a pure cross build) — the caller turns that into a clear error.
///
/// Mirrors `stage-archive::run::resolve_host_binary` (the same host-once
/// pattern the archive completion harvest uses).
fn resolve_host_binary(binaries: &[Artifact]) -> Option<Artifact> {
    let host = anodizer_core::partial::detect_host_target().ok()?;
    binaries
        .iter()
        .find(|b| b.target.as_deref() == Some(host.as_str()))
        .cloned()
}

/// The clear error emitted when a runtime harvest is configured but no
/// host-native artifact exists in the build matrix (pure cross build).
fn host_missing_error(id: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "appimage: runtime_harvest for config '{id}' must run the freshly-built binary on the \
         host to populate the harvest dir, but no built artifact matches the host target (pure \
         cross build). Add the host target to your build matrix so the harvest binary exists."
    )
}

// ---------------------------------------------------------------------------
// Per-target template vars (mirrors makeself)
// ---------------------------------------------------------------------------

/// Group binary artifacts by `(platform, amd64_variant)` — e.g.
/// `("linux_amd64", Some("v3"))` — so each (os, arch) AND micro-architecture
/// variant yields exactly one AppImage.
///
/// The key carries the binary's `amd64_variant` metadata alongside the os/arch
/// platform string so two amd64 builds of one triple (a baseline `v1` and a
/// `-Ctarget-cpu=x86-64-v3` tune) land in separate groups and produce two
/// distinct `.AppImage` files instead of one silently clobbering the other.
///
/// Uses a `BTreeMap` (not `HashMap`) so iteration order is deterministic
/// across runs: callers register one AppImage Artifact per group, and
/// `HashMap` iteration is randomised per process — the matching
/// `stage-archive`/`stage-makeself` regression shipped per-run drift into
/// `dist/artifacts.json`. This stage shares the same guard.
fn group_by_platform<'a>(
    binaries: &'a [Artifact],
) -> std::collections::BTreeMap<(String, Option<String>), Vec<&'a Artifact>> {
    let mut groups: std::collections::BTreeMap<(String, Option<String>), Vec<&'a Artifact>> =
        std::collections::BTreeMap::new();
    for a in binaries {
        let platform = match &a.target {
            Some(t) => {
                let (os, arch) = anodizer_core::target::map_target(t);
                format!("{os}_{arch}")
            }
            None => "unknown".to_string(),
        };
        let variant = a.metadata.get("amd64_variant").cloned();
        groups.entry((platform, variant)).or_default().push(a);
    }
    groups
}

/// Seed Os / Arch / Target plus the per-target variant template vars so the
/// default filename template renders correctly.
///
/// `amd64_variant` is the built binary's `amd64_variant` metadata: it overrides
/// the triple-derived `Amd64` so two amd64 builds of one triple (a baseline and
/// a `-Ctarget-cpu=x86-64-v3` tune) render distinct `.AppImage` names. `None` /
/// `Some("v1")` leave the suffix empty, preserving the single-variant name; the
/// Arm/Mips triple derivation is untouched.
fn set_per_target_template_vars(
    ctx: &mut Context,
    target: Option<&str>,
    os: &str,
    arch: &str,
    amd64_variant: Option<&str>,
) {
    ctx.template_vars_mut().set("Os", os);
    ctx.template_vars_mut().set("Arch", arch);
    ctx.template_vars_mut().set("Target", target.unwrap_or(""));

    let first = target.and_then(|t| t.split('-').next()).unwrap_or("");
    ctx.template_vars_mut().set("Arm", "");
    ctx.template_vars_mut().set("Arm64", "");
    ctx.template_vars_mut().set("Amd64", "");
    ctx.template_vars_mut().set("Mips", "");
    ctx.template_vars_mut().set("I386", "");
    match first {
        "aarch64" => ctx.template_vars_mut().set("Arm64", "v8"),
        "armv7" | "armv7l" => ctx.template_vars_mut().set("Arm", "7"),
        "armv6" | "armv6l" | "arm" => ctx.template_vars_mut().set("Arm", "6"),
        "i686" | "i386" | "i586" => ctx.template_vars_mut().set("I386", "sse2"),
        c if c.starts_with("mips") => {
            ctx.template_vars_mut().set("Mips", c);
        }
        _ => {}
    }

    // Set `Amd64` from the binary's actual variant metadata (not a hardcoded
    // `v1`) so v1/v2/v3 builds of the same x86_64 triple render distinctly;
    // None / "v1" leave the suffix empty.
    anodizer_core::archive_name::seed_amd64_variant_var(ctx.template_vars_mut(), amd64_variant);
}

/// The amd64 micro-architecture variant suffix the default AppImage filename
/// appends, rendered from the binary's seeded `Amd64` template var.
///
/// AppImage keeps the whole go-arch in `arch_token` (no arm-split), so amd64 is
/// the only micro-architecture dimension that can collide on one token — hence
/// the amd64-only [`INSTALLER_AMD64_VARIANT_SUFFIX`](anodizer_core::archive_name::INSTALLER_AMD64_VARIANT_SUFFIX),
/// not the full Arm/Mips/Amd64 clause. A baseline `v1` / `None` renders empty,
/// preserving the historical single-variant name.
fn default_amd64_suffix(ctx: &Context) -> Result<String> {
    ctx.render_template(anodizer_core::archive_name::INSTALLER_AMD64_VARIANT_SUFFIX)
}

/// Render the `.AppImage` output filename for one (target, platform) combo.
///
/// Honors `cfg.filename` as a Tera template when set (appending `.AppImage`
/// if absent); otherwise composes `<project>-<version>-<arch>[<amd64>].AppImage`
/// (AppImage is Linux-only, so the os segment is omitted). The arch is the
/// AppImage-flavoured arch token, plus the amd64 micro-architecture variant
/// suffix, so multi-arch AND multi-variant builds for the same project never
/// collide on disk.
///
/// Two `appimages:` configs that differ only by `id` (no custom `filename`)
/// and target the same arch render the same default output name and would
/// clobber on disk — set an explicit `filename:` on each to disambiguate.
/// This matches the sibling makeself stage's default-naming behaviour.
/// The rendered `.AppImage` filename paired with the template that produced it:
/// `(rendered_name, resolved_template)`. The resolved template is the user's
/// `filename:` when set, else the composed default (including the amd64 variant
/// suffix) — exactly the string the [`ArchPathGuard`] cites when it rejects a
/// clobber, so the diagnostic never reports an empty template.
type ResolvedFilename = (String, String);

fn resolve_appimage_filename(
    ctx: &Context,
    name_template: Option<&str>,
    project_name: &str,
    version: &str,
    arch_token: &str,
) -> Result<ResolvedFilename> {
    if let Some(tmpl) = name_template.filter(|t| !t.is_empty()) {
        let rendered = ctx.render_template(tmpl)?;
        let output_name = if rendered.ends_with(".AppImage") {
            rendered
        } else {
            format!("{rendered}.AppImage")
        };
        return Ok((output_name, tmpl.to_string()));
    }
    let amd64_suffix = default_amd64_suffix(ctx)?;
    // The composed default is fully rendered, so it serves as both the produced
    // name and the template the guard cites (no `{{ .Arch }}` placeholder to
    // re-render — the arch token is already substituted in).
    let composed = format!("{project_name}-{version}-{arch_token}{amd64_suffix}.AppImage");
    Ok((composed.clone(), composed))
}

// ---------------------------------------------------------------------------
// AppDir assembly (pure FS — independently testable)
// ---------------------------------------------------------------------------

/// A file/dir to copy into the AppDir (`src` on disk → `dst` relative to the
/// AppDir root).
#[derive(Debug, Clone, PartialEq, Eq)]
struct AppDirEntry {
    src: PathBuf,
    dst: String,
}

/// Assemble the AppDir on disk: copy the binary, desktop file, icon, the
/// harvested runtime tree (if any), and arbitrary extra entries into
/// `appdir`. Returns the absolute paths of the desktop file and icon as they
/// land inside the AppDir (linuxdeploy is pointed at the in-tree copies so a
/// per-run worktree prefix never leaks into its argv).
fn assemble_appdir(appdir: &Path, job: &AppImageJob) -> Result<(PathBuf, PathBuf)> {
    std::fs::create_dir_all(appdir)
        .with_context(|| format!("appimage: create AppDir {}", appdir.display()))?;

    // Binary → AppDir/usr/bin/<name>.
    let bin_dst = appdir.join("usr").join("bin").join(&job.binary_name);
    if let Some(parent) = bin_dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(&job.binary_src, &bin_dst).with_context(|| {
        format!(
            "appimage: copy binary {} → {}",
            job.binary_src.display(),
            bin_dst.display()
        )
    })?;

    // Desktop file + icon land at the AppDir root (linuxdeploy moves them into
    // usr/share/applications and usr/share/icons during assembly).
    let desktop_dst = appdir.join(file_basename(&job.desktop_src, "app.desktop"));
    std::fs::copy(&job.desktop_src, &desktop_dst).with_context(|| {
        format!(
            "appimage: copy desktop file {} → {}",
            job.desktop_src.display(),
            desktop_dst.display()
        )
    })?;

    // Icon: deploy under the EXACT `Icon=` name from the desktop entry (so
    // linuxdeploy's theme resolution finds it) into both the icon-theme tree
    // and the AppDir root. See [`linuxdeploy_args`] for why the stage places
    // the icon itself instead of handing linuxdeploy a `-i` it may reject.
    let icon_ext = job
        .icon_src
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("png");
    let icon_stem =
        desktop_icon_name(&desktop_dst).unwrap_or_else(|| file_stem(&job.icon_src, &job.app_name));
    let icon_file = format!("{icon_stem}.{icon_ext}");

    let theme_apps_dir = appdir
        .join("usr")
        .join("share")
        .join("icons")
        .join("hicolor")
        .join(icon_theme_subdir(&job.icon_src))
        .join("apps");
    std::fs::create_dir_all(&theme_apps_dir).with_context(|| {
        format!(
            "appimage: create icon theme dir {}",
            theme_apps_dir.display()
        )
    })?;
    let theme_icon = theme_apps_dir.join(&icon_file);
    std::fs::copy(&job.icon_src, &theme_icon).with_context(|| {
        format!(
            "appimage: copy icon {} → {}",
            job.icon_src.display(),
            theme_icon.display()
        )
    })?;

    // Root copy: linuxdeploy symlinks `.DirIcon` from the top-level icon, so a
    // root copy named for the `Icon=` key matches its own successful-run layout.
    let icon_dst = appdir.join(&icon_file);
    std::fs::copy(&job.icon_src, &icon_dst).with_context(|| {
        format!(
            "appimage: copy icon {} → {}",
            job.icon_src.display(),
            icon_dst.display()
        )
    })?;

    for entry in &job.appdir_entries {
        copy_entry_into_appdir(appdir, entry)?;
    }

    Ok((desktop_dst, icon_dst))
}

/// Extract the `Icon=` value from a `.desktop` file so the AppDir icon is
/// deployed under the exact name linuxdeploy resolves the entry against.
///
/// Returns the trimmed value with any trailing image extension stripped (the
/// freedesktop `Icon=` key is conventionally a bare name); `None` when the file
/// is unreadable or carries no non-empty `Icon=` key, in which case the caller
/// falls back to a derived stem.
fn desktop_icon_name(desktop: &Path) -> Option<String> {
    let text = std::fs::read_to_string(desktop).ok()?;
    let raw = text
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("Icon="))?
        .trim();
    let name = Path::new(raw)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(raw)
        .trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// The freedesktop icon-theme subdirectory an icon belongs under: `scalable`
/// for SVG, else `<W>x<H>` read from the PNG header, falling back to `256x256`
/// when the dimensions can't be read.
///
/// linuxdeploy does not resolution-validate an icon already present in the
/// AppDir's theme tree (unlike one passed via `-i`), so a non-standard size
/// such as `1024x1024` is deployed verbatim — the property that lets the stage
/// accept any source icon resolution.
fn icon_theme_subdir(icon_src: &Path) -> String {
    let ext = icon_src
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext == "svg" || ext == "svgz" {
        return "scalable".to_string();
    }
    png_dimensions(icon_src)
        .map(|(w, h)| format!("{w}x{h}"))
        .unwrap_or_else(|| "256x256".to_string())
}

/// Read a PNG's pixel dimensions from its IHDR chunk (width then height, both
/// big-endian u32 immediately after the 8-byte signature + 8-byte chunk
/// header). `None` for a non-PNG or a file too short to carry an IHDR.
///
/// No image-decoding dependency: the IHDR sits at a fixed offset, so the two
/// u32s are a direct slice read.
fn png_dimensions(path: &Path) -> Option<(u32, u32)> {
    const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() < 24 || &bytes[0..8] != PNG_SIGNATURE || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    Some((w, h))
}

/// The file stem of `path` (basename without extension), or `fallback` when it
/// has none.
fn file_stem(path: &Path, fallback: &str) -> String {
    path.file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or(fallback)
        .to_string()
}

/// Copy one [`AppDirEntry`] (file or directory) into the AppDir at its `dst`.
fn copy_entry_into_appdir(appdir: &Path, entry: &AppDirEntry) -> Result<()> {
    let dst = appdir.join(&entry.dst);
    if entry.src.is_dir() {
        anodizer_core::util::copy_dir_tree(&entry.src, &dst).with_context(|| {
            format!(
                "appimage: copy dir {} → {}",
                entry.src.display(),
                dst.display()
            )
        })?;
    } else {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&entry.src, &dst).with_context(|| {
            format!(
                "appimage: copy file {} → {}",
                entry.src.display(),
                dst.display()
            )
        })?;
    }
    Ok(())
}

/// Recursively pin every regular file's mtime under `dir` to `epoch_secs`.
///
/// linuxdeploy's `appimage` output plugin wraps the AppDir in a squashfs;
/// squashfs embeds each file's on-disk mtime. `fs::copy` (in
/// [`assemble_appdir`]) stamps every staged file with the current wall-clock,
/// so two runs produce identical content with different timestamps and the
/// resulting `.AppImage` bytes drift. Pinning the AppDir tree to
/// `SOURCE_DATE_EPOCH` removes that drift; appimagetool itself honours
/// `SOURCE_DATE_EPOCH` for the squashfs superblock when the env var is set.
fn pin_appdir_mtimes(dir: &Path, epoch_secs: i64) -> Result<()> {
    let mut stack: Vec<PathBuf> = vec![dir.to_path_buf()];
    while let Some(p) = stack.pop() {
        for entry in std::fs::read_dir(&p)
            .with_context(|| format!("appimage: read_dir {} for mtime pin", p.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let ft = entry.file_type()?;
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() {
                anodizer_core::util::set_file_mtime_epoch(&path, epoch_secs)
                    .with_context(|| format!("appimage: pin mtime on {}", path.display()))?;
            }
        }
    }
    Ok(())
}

/// The basename of `path`, or `fallback` when it has none.
fn file_basename(path: &Path, fallback: &str) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(fallback)
        .to_string()
}

// ---------------------------------------------------------------------------
// Runtime harvest (host-once)
// ---------------------------------------------------------------------------

/// Render the harvest command for a config, binding `{{ .ArtifactPath }}` to
/// the host binary's path and `{{ .HarvestDir }}` to the absolute harvest
/// output dir. The bound vars are cleared immediately after rendering so they
/// never leak into later renders (mirrors completions_gen's
/// `clear_generate_vars`).
fn render_harvest_command(
    ctx: &mut Context,
    command_tmpl: &str,
    host: &Artifact,
    harvest_dir: &Path,
) -> Result<String> {
    let tvars = ctx.template_vars_mut();
    tvars.set("ArtifactPath", &host.path.to_string_lossy());
    tvars.set("HarvestDir", &harvest_dir.to_string_lossy());
    let rendered = ctx.render_template(command_tmpl);
    let tvars = ctx.template_vars_mut();
    tvars.set("ArtifactPath", "");
    tvars.set("HarvestDir", "");
    rendered.with_context(|| format!("appimage: render runtime_harvest command '{command_tmpl}'"))
}

/// Run the rendered harvest command once via `sh -c`, populating
/// `harvest_dir` (created beforehand).
fn run_harvest(cmd: &str, harvest_dir: &Path, log: &anodizer_core::log::StageLogger) -> Result<()> {
    std::fs::create_dir_all(harvest_dir)
        .with_context(|| format!("appimage: create harvest dir {}", harvest_dir.display()))?;
    log.status(&format!("harvesting AppImage runtime via `{cmd}`"));
    let output = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .output()
        .with_context(|| format!("appimage: spawn runtime_harvest command `{cmd}`"))?;
    if !output.status.success() {
        bail!(
            "appimage: runtime_harvest command `{cmd}` failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// AppImageJob — fully-owned, ready for parallel execution
// ---------------------------------------------------------------------------

/// One fully-prepared AppImage job (one per matching target). The serial
/// phase renders templates + assembles the static parts; the parallel phase
/// builds the AppDir and spawns linuxdeploy. Carries only owned data so
/// worker threads never touch `Context`.
struct AppImageJob {
    id: String,
    filename: String,
    /// Display name passed to linuxdeploy via `APP` and used as AppDir base.
    app_name: String,
    version: String,
    arch_token: String,
    update_information: Option<String>,
    extra_args: Vec<String>,
    appdir_root: PathBuf,
    output_path: PathBuf,
    binary_src: PathBuf,
    binary_name: String,
    desktop_src: PathBuf,
    icon_src: PathBuf,
    /// Extra files/dirs (including the harvested runtime tree) to drop into
    /// the AppDir before linuxdeploy runs.
    appdir_entries: Vec<AppDirEntry>,
    primary_target: Option<String>,
    primary_crate_name: String,
    /// The binary's amd64 micro-architecture variant (`None` / `Some("v1")`
    /// → baseline), recorded in the produced artifact's metadata so downstream
    /// stages can tell two amd64 builds of one triple apart.
    amd64_variant: Option<String>,
    /// Pre-resolved `SOURCE_DATE_EPOCH` seconds. Resolved in the serial phase
    /// via `ctx.env_var` so the parallel execution phase never calls
    /// `std::env`; `None` outside a reproducible (harness) build.
    sde_epoch: Option<i64>,
}

/// Execute a prepared AppImage job: assemble the AppDir, then spawn
/// linuxdeploy with the constructed argv + env. Returns the registered
/// `Artifact`.
fn execute_appimage_job(
    job: &AppImageJob,
    verbosity: anodizer_core::log::Verbosity,
) -> Result<Artifact> {
    let thread_log = anodizer_core::log::StageLogger::new("appimage", verbosity);

    // The root icon path is returned for parity with the staged layout but is
    // not handed to linuxdeploy: the icon is resolved from the pre-placed theme
    // tree (see [`linuxdeploy_args`]).
    let (desktop_dst, _icon_dst) = assemble_appdir(&job.appdir_root, job)?;

    // Reproducibility: pin every staged file's mtime to SOURCE_DATE_EPOCH so
    // the squashfs payload is byte-stable across runs (mirrors the sibling
    // makeself stage's mtime pinning). No-op outside a harness build.
    if let Some(epoch) = job.sde_epoch {
        pin_appdir_mtimes(&job.appdir_root, epoch)?;
    }

    // linuxdeploy runs with the AppDir's parent as cwd: it writes the output
    // `.AppImage` into the current dir, so a per-job dir keeps parallel jobs
    // from clobbering one another's output (each AppDir lives under
    // `dist/appimage/<id>/<platform>/`). The `--appdir` / `-d` paths it
    // receives MUST be relative to THAT cwd, not the process cwd: `appdir_root`
    // is `dist/appimage/<id>/<platform>/<app>.AppDir` relative to the worktree
    // root, and passing it verbatim while cwd is its own parent would re-resolve
    // it under work_dir and double the prefix — linuxdeploy then can't find the
    // staged AppDir / desktop file. The AppDir basename and
    // `<basename>/<desktop>` are the correct work_dir-relative forms.
    let work_dir = job.appdir_root.parent().unwrap_or(&job.appdir_root);
    let appdir_rel: PathBuf = job
        .appdir_root
        .file_name()
        .map(PathBuf::from)
        .unwrap_or_else(|| job.appdir_root.clone());
    let desktop_rel = match desktop_dst.file_name() {
        Some(name) => appdir_rel.join(name),
        None => appdir_rel.clone(),
    };
    let args = linuxdeploy_args(&appdir_rel, &desktop_rel, &job.extra_args);
    let env = linuxdeploy_env(
        &job.version,
        &job.arch_token,
        &job.app_name,
        &job.filename,
        job.update_information.as_deref(),
    );

    thread_log.status(&format!("creating AppImage {}", job.filename));

    let mut command = Command::new("linuxdeploy");
    command.args(&args).current_dir(work_dir);
    for (k, v) in &env {
        command.env(k, v);
    }
    // Forward SOURCE_DATE_EPOCH so the child appimagetool stamps the squashfs
    // superblock deterministically (it honours the var when set, else falls
    // back to wall-clock). Resolved in the serial phase to keep this code
    // `std::env`-free.
    if let Some(epoch) = job.sde_epoch {
        command.env("SOURCE_DATE_EPOCH", epoch.to_string());
    }
    let output = command.output().with_context(|| {
        format!(
            "appimage: failed to spawn 'linuxdeploy' for {} (is linuxdeploy on PATH?)",
            job.filename
        )
    })?;

    if !output.status.success() {
        bail!(
            "appimage: linuxdeploy failed for '{}' (id={}): {}{}",
            job.filename,
            job.id,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // linuxdeploy writes the AppImage into the current working dir named after
    // APP/ARCH; locate it and move it to the deterministic output path.
    let built = locate_built_appimage(&job.appdir_root, &job.filename)?;
    if built != job.output_path {
        std::fs::rename(&built, &job.output_path)
            .or_else(|_| {
                std::fs::copy(&built, &job.output_path)?;
                std::fs::remove_file(&built)
            })
            .with_context(|| {
                format!(
                    "appimage: move {} → {}",
                    built.display(),
                    job.output_path.display()
                )
            })?;
    }

    let mut metadata = HashMap::new();
    metadata.insert("id".to_string(), job.id.clone());
    metadata.insert("format".to_string(), "appimage".to_string());
    metadata.insert("ext".to_string(), ".AppImage".to_string());
    if let Some(v) = &job.amd64_variant {
        metadata.insert("amd64_variant".to_string(), v.clone());
    }

    Ok(Artifact {
        kind: ArtifactKind::AppImage,
        name: job.filename.clone(),
        path: job.output_path.clone(),
        target: job.primary_target.clone(),
        crate_name: job.primary_crate_name.clone(),
        metadata,
        size: None,
    })
}

/// Find the `.AppImage` linuxdeploy emitted in the cwd (the AppDir's parent),
/// where it writes `<APP>-<ARCH>.AppImage`.
///
/// Selection is deterministic and retry-safe: prefer an `.AppImage` whose
/// basename matches `expected_filename` (linuxdeploy's name is derived from
/// the same APP/ARCH that built `expected_filename`), then fall back to the
/// newest `.AppImage` by mtime. Picking the first `read_dir` entry would
/// otherwise be filesystem-order-dependent and could grab a stale output left
/// by a prior failed attempt in the same work dir.
fn locate_built_appimage(appdir_root: &Path, expected_filename: &str) -> Result<PathBuf> {
    let search_dir = appdir_root.parent().unwrap_or(appdir_root);
    let mut candidates: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(search_dir)
        .with_context(|| format!("appimage: scan {} for output", search_dir.display()))?
    {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) == Some("AppImage") {
            candidates.push(path);
        }
    }

    if let Some(exact) = candidates
        .iter()
        .find(|p| p.file_name().and_then(|n| n.to_str()) == Some(expected_filename))
    {
        return Ok(exact.clone());
    }

    // No exact match (linuxdeploy chose a different APP/ARCH casing): pick the
    // newest output so a stale `.AppImage` from a prior failed run is not
    // selected over the one just produced.
    candidates
        .into_iter()
        .max_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "appimage: linuxdeploy reported success but no .AppImage was found in {}",
                search_dir.display()
            )
        })
}

// ---------------------------------------------------------------------------
// AppImageStage
// ---------------------------------------------------------------------------

pub struct AppImageStage;

impl Stage for AppImageStage {
    fn name(&self) -> &str {
        "appimage"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let configs = ctx.config.appimages.clone();
        if configs.is_empty() {
            return Ok(());
        }

        let log = ctx.logger("appimage");
        validate_unique_ids(&configs)?;

        let dist = ctx.config.dist.clone();
        let dry_run = ctx.options.dry_run;
        let parallelism = ctx.options.parallelism.max(1);
        let version = ctx
            .template_vars()
            .get("Version")
            .cloned()
            .unwrap_or_else(|| "0.0.0".to_string());
        let project_name = ctx.config.project_name.clone();
        // Resolve SOURCE_DATE_EPOCH once in the serial phase so each job
        // carries the value (the parallel phase never touches `std::env`).
        let sde_epoch = ctx
            .env_var("SOURCE_DATE_EPOCH")
            .and_then(|s| s.parse::<i64>().ok());

        // One guard spans every `appimages:` config of the project: two configs
        // with the default (or identical) `filename:` render the same `.AppImage`
        // path for one arch — error loudly across configs instead of letting the
        // second silently clobber the first.
        let mut arch_guard = ArchPathGuard::new();

        let mut jobs: Vec<AppImageJob> = Vec::new();
        for cfg in &configs {
            collect_config_jobs(
                ctx,
                &log,
                cfg,
                &dist,
                &version,
                &project_name,
                sde_epoch,
                dry_run,
                &mut arch_guard,
                &mut jobs,
            )?;
        }

        if jobs.is_empty() {
            return Ok(());
        }

        let verbosity = log.verbosity();
        let built = anodizer_core::parallel::run_parallel_chunks(
            &jobs,
            parallelism,
            "appimage",
            |job: &AppImageJob| execute_appimage_job(job, verbosity),
        )?;

        for artifact in built {
            ctx.artifacts.add(artifact);
        }

        anodizer_core::template::clear_per_target_vars(ctx.template_vars_mut());
        Ok(())
    }
}

/// Collect `AppImageJob`s for one config: validate, run the host-once runtime
/// harvest, then build one job per matching Linux target.
#[allow(clippy::too_many_arguments)]
fn collect_config_jobs(
    ctx: &mut Context,
    log: &anodizer_core::log::StageLogger,
    cfg: &anodizer_core::config::AppImageConfig,
    dist: &Path,
    version: &str,
    project_name: &str,
    sde_epoch: Option<i64>,
    dry_run: bool,
    arch_guard: &mut ArchPathGuard,
    jobs: &mut Vec<AppImageJob>,
) -> Result<()> {
    let id = cfg.id.as_deref().unwrap_or("default").to_string();

    if let Some(ref d) = cfg.skip {
        let off = d
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .with_context(|| "appimage: render skip template")?;
        if off {
            log.verbose("appimage config skipped");
            return Ok(());
        }
    }

    validate_config_fields(cfg, &id)?;

    let os_filter: Vec<String> = cfg.os.clone().unwrap_or_else(|| vec!["linux".to_string()]);
    let binaries = collect_matching_binaries(ctx, cfg, &os_filter);
    if binaries.is_empty() {
        bail!(
            "appimage: no binaries found for config '{}' with os {:?}",
            id,
            os_filter
        );
    }

    // Host-once runtime harvest: render + run on the host-native binary, then
    // stage the populated tree at a stable dist path that every target's
    // AppImage (and an archive glob) can reuse.
    let harvested: Option<AppDirEntry> = if let Some(ref harvest) = cfg.runtime_harvest {
        let host = resolve_host_binary(&binaries).ok_or_else(|| host_missing_error(&id))?;
        let harvest_dir = dist.join(".appimage-runtime").join(&id);
        let cmd = render_harvest_command(ctx, &harvest.command, &host, &harvest_dir)?;
        if dry_run {
            log.status(&format!(
                "(dry-run) would harvest AppImage runtime via `{cmd}` → {}",
                harvest_dir.display()
            ));
        } else {
            run_harvest(&cmd, &harvest_dir, log)?;
        }
        Some(AppDirEntry {
            src: harvest_dir,
            dst: harvest.dir.trim_end_matches('/').to_string(),
        })
    } else {
        None
    };

    // Resolve the static (target-independent) config templates ONCE.
    let desktop_src = PathBuf::from(ctx.render_template(cfg.desktop.as_deref().unwrap_or(""))?);
    let icon_src = PathBuf::from(ctx.render_template(cfg.icon.as_deref().unwrap_or(""))?);
    let update_information = cfg
        .update_information
        .as_deref()
        .map(|u| ctx.render_template(u))
        .transpose()?;
    let extra_args: Vec<String> = cfg
        .extra_args
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|a| ctx.render_template(a))
        .collect::<Result<Vec<_>>>()?;
    let app_name = cfg
        .name
        .as_deref()
        .map(|n| ctx.render_template(n))
        .transpose()?
        .unwrap_or_else(|| project_name.to_string());

    let mut extra_entries: Vec<AppDirEntry> = Vec::new();
    if let Some(ref extras) = cfg.appdir_extra {
        for e in extras {
            extra_entries.push(AppDirEntry {
                src: PathBuf::from(ctx.render_template(&e.src)?),
                dst: ctx.render_template(&e.dst)?,
            });
        }
    }
    if let Some(h) = harvested {
        extra_entries.push(h);
    }

    // Group by (platform, amd64_variant) so each (os, arch) AND micro-arch
    // variant produces exactly one AppImage.
    let groups = group_by_platform(&binaries);

    for ((_, amd64_variant), group) in &groups {
        let Some(primary) = group.first() else {
            continue;
        };
        let (os, arch) = primary
            .target
            .as_deref()
            .map(anodizer_core::target::map_target)
            .unwrap_or_else(|| ("linux".to_string(), "unknown".to_string()));
        set_per_target_template_vars(
            ctx,
            primary.target.as_deref(),
            &os,
            &arch,
            amd64_variant.as_deref(),
        );

        let arch_token = primary
            .target
            .as_deref()
            .map(appimage_arch)
            .unwrap_or_else(|| arch.clone());

        let (filename, resolved_template) = resolve_appimage_filename(
            ctx,
            cfg.filename.as_deref(),
            project_name,
            version,
            &arch_token,
        )?;

        let output_path = dist.join(&filename);
        // Reject a `filename:` that renders the same `.AppImage` path for two
        // targets / amd64 variants (an override lacking `{{ .Arch }}` /
        // `{{ .Amd64 }}`): the second would silently overwrite the first.
        arch_guard.check(
            &output_path,
            "appimage",
            "image",
            &resolved_template,
            &filename,
            &primary.crate_name,
        )?;

        // Disambiguate the AppDir per amd64 variant so two non-baseline
        // variants of one platform don't stage into (and clobber) the same dir.
        let platform_subdir = match amd64_variant.as_deref() {
            Some(v) if v != "v1" => format!("{os}_{arch}_{v}"),
            _ => format!("{os}_{arch}"),
        };
        let appdir_root = dist
            .join("appimage")
            .join(&id)
            .join(platform_subdir)
            .join(format!("{app_name}.AppDir"));

        let binary_name = primary
            .metadata
            .get("binary")
            .cloned()
            .unwrap_or_else(|| primary.name.clone());

        if dry_run {
            log.status(&format!("(dry-run) would create AppImage {filename}"));
            continue;
        }

        jobs.push(AppImageJob {
            id: id.clone(),
            filename,
            app_name: app_name.clone(),
            version: version.to_string(),
            arch_token,
            update_information: update_information.clone(),
            extra_args: extra_args.clone(),
            appdir_root,
            output_path,
            binary_src: primary.path.clone(),
            binary_name,
            desktop_src: desktop_src.clone(),
            icon_src: icon_src.clone(),
            appdir_entries: extra_entries.clone(),
            primary_target: primary.target.clone(),
            primary_crate_name: primary.crate_name.clone(),
            amd64_variant: amd64_variant.clone(),
            sde_epoch,
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests;

/// Environment requirements for the appimage stage: the `linuxdeploy`
/// binary whenever any `appimages:` entry is active (entries whose `skip`
/// evaluates true are inert).
pub fn env_requirements(
    ctx: &anodizer_core::context::Context,
) -> Vec<anodizer_core::EnvRequirement> {
    let any = ctx.config.appimages.iter().any(|cfg| {
        !cfg.skip.as_ref().is_some_and(|s| {
            s.try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                .unwrap_or(false)
        })
    });
    if !any {
        return Vec::new();
    }
    vec![anodizer_core::EnvRequirement::Tool {
        name: "linuxdeploy".to_string(),
    }]
}
