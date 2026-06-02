//! Single source of truth for archive asset naming.
//!
//! The archive stage names every release asset by rendering a `name_template`
//! against a set of per-target template variables (`Os`, `Arch`, `Target`, plus
//! the micro-architecture variants `Arm` / `Arm64` / `Amd64` / `Mips` / `I386`)
//! and appending the archive format as the file extension. Several other
//! features must compute the *same* filename without producing an archive on
//! disk — most notably cargo-binstall metadata derivation, which has to emit a
//! `pkg_url` pointing at an asset whose name exactly matches what the archive
//! stage will later upload.
//!
//! Centralising the default templates, the per-target variant seeding, and the
//! format→extension / format→`pkg_fmt` mappings here guarantees those derived
//! names cannot drift from the archive stage's own output: a `pkg_url` derived
//! through [`render_archive_asset_name`] resolves to byte-identical bytes as the
//! archive the release uploads, eliminating the "binstall 404" class by
//! construction.

use anyhow::{Context as _, Result};

use crate::context::Context;
use crate::target::map_target;

/// Canonical GoReleaser-style archive name template used when a crate sets no
/// `archive.name_template:`. Mirrors GoReleaser's default
/// (`{{ .ProjectName }}_{{ .Version }}_{{ .Os }}_{{ .Arch }}…`) with the
/// micro-architecture variant suffixes appended.
pub const DEFAULT_NAME_TEMPLATE: &str = "{{ .ProjectName }}_{{ .Version }}_{{ .Os }}_{{ .Arch }}{% if Arm %}v{{ Arm }}{% endif %}{% if Mips %}_{{ Mips }}{% endif %}{% if Amd64 and Amd64 != \"v1\" %}{{ Amd64 }}{% endif %}";

/// Multi-crate variant of [`DEFAULT_NAME_TEMPLATE`]. Identical in shape; the
/// archive stage rebinds `ProjectName` to the per-crate name so each crate's
/// stem is distinct without forcing users to hand-author `archive.name_template:`.
pub const DEFAULT_NAME_TEMPLATE_MULTI_CRATE: &str = DEFAULT_NAME_TEMPLATE;

/// Default name template for `format: binary` archives (uses `{{ .Binary }}`
/// rather than `{{ .ProjectName }}` so each binary is named individually).
pub const DEFAULT_BINARY_NAME_TEMPLATE: &str = "{{ .Binary }}_{{ .Version }}_{{ .Os }}_{{ .Arch }}{% if Arm %}v{{ Arm }}{% endif %}{% if Mips %}_{{ Mips }}{% endif %}{% if Amd64 and Amd64 != \"v1\" %}{{ Amd64 }}{% endif %}";

/// Seed the per-target template variables a `name_template` reads.
///
/// Sets `Os`, `Arch`, and `Target` from [`map_target`], plus the
/// micro-architecture variant vars (`Arm`, `Arm64`, `Amd64`, `Mips`, `I386`),
/// all reset every call so a prior target's value can never leak.
///
/// The default `name_template` concatenates `{{ .Arch }}{% if Arm %}v{{ Arm }}…`,
/// so the ARM micro-architecture must be carried in `Arm` with `Arch` reduced to
/// the bare `"arm"` — otherwise `{{ .Arch }}v{{ .Arm }}` would double to
/// `armv7v7`. This mirrors the project's tested invariant in
/// `stage-snapcraft::compute_snap_filename`
/// (`tests::test_armv7_target_splits_arch_and_arm_for_default_template`:
/// `linux_armv7`, not `linux_armv7v7`).
///
/// For every other architecture the default template's `{% if Arm %}` /
/// `{% if Mips %}` / `{% if Amd64 … %}` guards must emit NOTHING (the go-arch
/// `Arch` token alone is the asset suffix), so `Arm64` / `Amd64` / `Mips` /
/// `I386` are left empty. The result is byte-identical to the asset names the
/// archive stage has always produced, which is the contract every consumer of a
/// derived name (binstall, nix, …) depends on.
pub fn seed_target_vars(ctx: &mut Context, target: &str) {
    let (os, arch) = map_target(target);
    let vars = ctx.template_vars_mut();
    vars.set("Os", &os);
    vars.set("Target", target);

    // Reset every variant var so a prior target's value cannot leak.
    vars.set("Arm", "");
    vars.set("Arm64", "");
    vars.set("Amd64", "");
    vars.set("Mips", "");
    vars.set("I386", "");

    // ARM is the only architecture whose default-template suffix lives in a
    // variant var: split `armv7`/`armv6` into `Arch="arm"` + `Arm="7"/"6"` so
    // `{{ .Arch }}v{{ .Arm }}` renders `armv7` rather than `armv7v7`. Every
    // other go-arch is carried whole in `Arch` with no variant suffix.
    if let Some(version) = arch.strip_prefix("armv") {
        vars.set("Arch", "arm");
        vars.set("Arm", version);
    } else {
        vars.set("Arch", &arch);
    }
}

/// Render an archive's *stem* (filename without the format extension) for a
/// single target by seeding the per-target vars and rendering `name_template`.
///
/// The caller is responsible for any non-target template vars the template
/// reads (`ProjectName`, `Version`, `CrateName`, `Binary`); this only owns the
/// per-target dimension. Returns the rendered stem.
pub fn render_archive_stem(ctx: &mut Context, name_template: &str, target: &str) -> Result<String> {
    seed_target_vars(ctx, target);
    ctx.render_template(name_template)
        .with_context(|| format!("render archive name template for target '{target}'"))
}

/// Render a complete archive *asset filename* (stem + format extension) for a
/// single target.
///
/// `format` is the archive format string as configured (`tar.gz`, `zip`,
/// `tar.xz`, …); it becomes the file extension exactly as the archive stage
/// writes it (`{stem}.{format}`). Returns the full asset filename, e.g.
/// `anodizer-1.2.3-linux-amd64.tar.gz`.
pub fn render_archive_asset_name(
    ctx: &mut Context,
    name_template: &str,
    target: &str,
    format: &str,
) -> Result<String> {
    let stem = render_archive_stem(ctx, name_template, target)?;
    Ok(format!("{stem}.{format}"))
}

/// Map an archive `format` string to cargo-binstall's `pkg_fmt` value.
///
/// cargo-binstall enumerates a fixed set of package formats; the archive
/// `format` strings anodize produces map onto them as follows. `None` is
/// returned for formats cargo-binstall cannot binstall (e.g. `binary`, `none`),
/// letting the caller skip emitting an override it could never resolve.
pub fn binstall_pkg_fmt(format: &str) -> Option<&'static str> {
    match format {
        "tar.gz" | "tgz" => Some("tgz"),
        "tar.xz" | "txz" => Some("txz"),
        "tar.zst" | "tzst" => Some("tzstd"),
        "tar.bz2" | "tbz2" => Some("tbz2"),
        "tar" => Some("tar"),
        "zip" => Some("zip"),
        "bin" => Some("bin"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::context::{Context, ContextOptions};

    fn ctx() -> Context {
        let config = Config {
            project_name: "anodizer".to_string(),
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("ProjectName", "anodizer");
        ctx.template_vars_mut().set("Version", "1.2.3");
        ctx
    }

    #[test]
    fn seeds_os_arch_target() {
        let mut c = ctx();
        seed_target_vars(&mut c, "x86_64-unknown-linux-gnu");
        assert_eq!(c.template_vars().get("Os").unwrap(), "linux");
        assert_eq!(c.template_vars().get("Arch").unwrap(), "amd64");
        assert_eq!(
            c.template_vars().get("Target").unwrap(),
            "x86_64-unknown-linux-gnu"
        );
        // amd64 has no default-template suffix — every variant var is empty so
        // the `{% if Amd64 … %}` guard emits nothing.
        assert_eq!(c.template_vars().get("Amd64").unwrap(), "");
        assert_eq!(c.template_vars().get("Arm").unwrap(), "");
    }

    #[test]
    fn armv7_splits_arch_and_arm() {
        // The ARM split: Arch reduces to "arm", Arm carries the digit, so the
        // default template's `{{ .Arch }}v{{ .Arm }}` renders "armv7" (not
        // "armv7v7"). Mirrors stage-snapcraft's tested invariant.
        let mut c = ctx();
        seed_target_vars(&mut c, "armv7-unknown-linux-gnueabihf");
        assert_eq!(c.template_vars().get("Arch").unwrap(), "arm");
        assert_eq!(c.template_vars().get("Arm").unwrap(), "7");
        assert_eq!(c.template_vars().get("Amd64").unwrap(), "");
    }

    #[test]
    fn mips_carries_no_variant_suffix() {
        // mips go-arch lives entirely in Arch; Mips stays empty so the
        // `{% if Mips %}` guard adds no suffix the asset name never had.
        let mut c = ctx();
        seed_target_vars(&mut c, "mips-unknown-linux-gnu");
        assert_eq!(c.template_vars().get("Arch").unwrap(), "mips");
        assert_eq!(c.template_vars().get("Mips").unwrap(), "");
    }

    #[test]
    fn variant_vars_reset_between_targets() {
        let mut c = ctx();
        seed_target_vars(&mut c, "armv7-unknown-linux-gnueabihf");
        assert_eq!(c.template_vars().get("Arm").unwrap(), "7");
        // Re-seed a non-arm target: the stale Arm value AND the reduced Arch
        // must clear back to the new target's go-arch.
        seed_target_vars(&mut c, "x86_64-unknown-linux-gnu");
        assert_eq!(c.template_vars().get("Arm").unwrap(), "");
        assert_eq!(c.template_vars().get("Arch").unwrap(), "amd64");
    }

    #[test]
    fn default_template_renders_goreleaser_stem() {
        let mut c = ctx();
        let stem =
            render_archive_stem(&mut c, DEFAULT_NAME_TEMPLATE, "aarch64-apple-darwin").unwrap();
        assert_eq!(stem, "anodizer_1.2.3_darwin_arm64");
    }

    #[test]
    fn custom_template_renders_with_format_ext() {
        let mut c = ctx();
        let name = render_archive_asset_name(
            &mut c,
            "{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}",
            "x86_64-pc-windows-msvc",
            "zip",
        )
        .unwrap();
        assert_eq!(name, "anodizer-1.2.3-windows-amd64.zip");
    }

    /// Byte-identity regression matrix: the default `name_template` rendered
    /// through `seed_target_vars` must equal the asset names the archive stage
    /// has always produced. The reference column is the historical (HEAD)
    /// behavior — `Os`/`Arch` from `map_target` with EVERY micro-arch variant
    /// var empty at archive time (the build stage reset them before archiving,
    /// and Mips was never set). 32-bit ARM is the regression that motivated this
    /// matrix: it must stay single-`armv7`, never `armv7v7`.
    #[test]
    fn default_template_byte_identical_to_head_for_every_arch() {
        let cases: &[(&str, &str)] = &[
            // 32-bit ARM — the regression. Single armv7/armv6, no doubling.
            (
                "armv7-unknown-linux-gnueabihf",
                "anodizer_1.2.3_linux_armv7",
            ),
            (
                "armv6-unknown-linux-gnueabihf",
                "anodizer_1.2.3_linux_armv6",
            ),
            // MIPS — no added `_mips…` suffix.
            ("mips-unknown-linux-gnu", "anodizer_1.2.3_linux_mips"),
            ("mipsel-unknown-linux-gnu", "anodizer_1.2.3_linux_mipsel"),
            (
                "mips64-unknown-linux-gnuabi64",
                "anodizer_1.2.3_linux_mips64",
            ),
            (
                "mips64el-unknown-linux-gnuabi64",
                "anodizer_1.2.3_linux_mips64el",
            ),
            // i686 — go-arch 386.
            ("i686-unknown-linux-gnu", "anodizer_1.2.3_linux_386"),
            // The 6 standard triples — unchanged baseline.
            ("x86_64-unknown-linux-gnu", "anodizer_1.2.3_linux_amd64"),
            ("aarch64-unknown-linux-gnu", "anodizer_1.2.3_linux_arm64"),
            ("x86_64-apple-darwin", "anodizer_1.2.3_darwin_amd64"),
            ("aarch64-apple-darwin", "anodizer_1.2.3_darwin_arm64"),
            ("x86_64-pc-windows-msvc", "anodizer_1.2.3_windows_amd64"),
            ("aarch64-pc-windows-msvc", "anodizer_1.2.3_windows_arm64"),
        ];
        for (target, expected) in cases {
            let mut c = ctx();
            let stem = render_archive_stem(&mut c, DEFAULT_NAME_TEMPLATE, target).unwrap();
            assert_eq!(
                &stem, expected,
                "default-template asset stem for {target} must match HEAD"
            );
        }
    }

    /// Independent confirmation that the reference column above equals what the
    /// HEAD seeding produced: render the SAME default template with `Os`/`Arch`
    /// from `map_target` and all micro-arch variants forced empty (the literal
    /// HEAD archive-time state), and assert the new `seed_target_vars` path
    /// renders the identical string for every target.
    #[test]
    fn new_seeding_equals_head_seeding_per_target() {
        let targets = [
            "armv7-unknown-linux-gnueabihf",
            "armv6-unknown-linux-gnueabihf",
            "mips-unknown-linux-gnu",
            "mipsel-unknown-linux-gnu",
            "mips64-unknown-linux-gnuabi64",
            "i686-unknown-linux-gnu",
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "x86_64-apple-darwin",
            "aarch64-apple-darwin",
            "x86_64-pc-windows-msvc",
            "aarch64-pc-windows-msvc",
        ];
        for target in targets {
            // HEAD reference: map_target's Os/Arch, all variants "".
            let (os, arch) = crate::target::map_target(target);
            let mut head = ctx();
            {
                let v = head.template_vars_mut();
                v.set("Os", &os);
                v.set("Arch", &arch);
                v.set("Target", target);
                v.set("Arm", "");
                v.set("Arm64", "");
                v.set("Amd64", "");
                v.set("Mips", "");
                v.set("I386", "");
            }
            let head_stem = head.render_template(DEFAULT_NAME_TEMPLATE).unwrap();

            // New path.
            let mut new = ctx();
            let new_stem = render_archive_stem(&mut new, DEFAULT_NAME_TEMPLATE, target).unwrap();

            assert_eq!(
                head_stem, new_stem,
                "new seed_target_vars must be byte-identical to HEAD for {target}"
            );
        }
    }

    #[test]
    fn pkg_fmt_maps_known_formats() {
        assert_eq!(binstall_pkg_fmt("tar.gz"), Some("tgz"));
        assert_eq!(binstall_pkg_fmt("zip"), Some("zip"));
        assert_eq!(binstall_pkg_fmt("tar.xz"), Some("txz"));
        assert_eq!(binstall_pkg_fmt("binary"), None);
        assert_eq!(binstall_pkg_fmt("none"), None);
    }
}
