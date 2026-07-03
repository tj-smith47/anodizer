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

/// Canonical archive name template used when a crate sets no
/// `archive.name_template:`. The default
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

/// The full micro-architecture variant suffix — the `Arm` / `Mips` / `Amd64`
/// tail shared by the Linux-capable asset namers: the archive stage's
/// [`DEFAULT_NAME_TEMPLATE`] / [`DEFAULT_BINARY_NAME_TEMPLATE`] and the
/// `makeself` `.run` default.
///
/// Those namers can target Linux/embedded triples, so all three dimensions can
/// occur: 32-bit ARM (`armv7`/`armv6`, carried in `Arm`), MIPS variants
/// (carried in `Mips`), and the x86-64 micro-architecture level (`Amd64`).
/// Centralising the clause keeps the *suffix* byte-identical across every
/// default that appends it — only the suffix is shared, not the whole template:
/// the archive defaults prefix it with the dotted `{{ .ProjectName }}…` stem
/// while the makeself default uses the bare `{{ ProjectName }}…` form, so the
/// prefixes differ by design. Each consumer carries a drift test pinning its own
/// default to this const so the shared tail cannot drift between them.
pub const MICRO_ARCH_VARIANT_SUFFIX: &str = "{% if Arm %}v{{ Arm }}{% endif %}{% if Mips %}_{{ Mips }}{% endif %}{% if Amd64 and Amd64 != \"v1\" %}{{ Amd64 }}{% endif %}";

/// The amd64-only subset of [`MICRO_ARCH_VARIANT_SUFFIX`] — the suffix the
/// macOS/Windows OS-installer family (`app_bundles`, `dmgs`, `pkgs`, `msis`,
/// `nsis`) default name templates append.
///
/// That amd64 is the lone micro-architecture dimension this family
/// disambiguates is structural to Rust, not arbitrary special-casing: 32-bit
/// ARM and MIPS variants are encoded in the **target triple** (`armv7-…` vs
/// `armv6-…`, `mips-…` vs `mipsel-…`), so they already render a distinct
/// `Arch`; only the x86-64 micro-architecture levels share one triple
/// (`x86_64-…`, differing solely by `-Ctarget-cpu=x86-64-v{2,3}`) and therefore
/// one `Arch`. The build stage consequently only detects `amd64_variant`
/// (stored in `Artifact.metadata["amd64_variant"]`), and these OSes have no
/// 32-bit-ARM/MIPS targets at all — so amd64 is the only clause this family
/// needs. Appending it lets two amd64 builds of the same target (a baseline
/// `v1` and a `v3` tuned with `-Ctarget-cpu=x86-64-v3`) render distinct
/// installer names instead of one silently clobbering the other; `v1` (the
/// baseline) renders no suffix so the common single-variant build keeps its
/// historical name.
pub const INSTALLER_AMD64_VARIANT_SUFFIX: &str =
    "{% if Amd64 and Amd64 != \"v1\" %}{{ Amd64 }}{% endif %}";

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
/// `{% if Mips %}` guards must emit NOTHING (the go-arch `Arch` token alone is
/// the asset suffix), so `Arm64` / `Mips` / `I386` are left empty. `Amd64` is
/// the exception: an x86-64 target seeds the `"v1"` baseline — the value every
/// seeding policy gives an untagged x86-64 binary — so `{{ Amd64 }}` renders
/// identically in an archive name and in a build/installer name for the same
/// binary. The default templates guard the clause with `Amd64 != "v1"`, so the
/// rendered asset names stay byte-identical to what the archive stage has
/// always produced — the contract every consumer of a derived name
/// (binstall, nix, …) depends on.
pub fn seed_target_vars(ctx: &mut Context, target: &str) {
    let (os, arch) = map_target(target);
    let vars = ctx.template_vars_mut();
    vars.set("Os", &os);
    vars.set("Target", target);

    reset_variant_vars(vars);

    // ARM is the only architecture whose default-template suffix lives in a
    // variant var: split `armv7`/`armv6` into `Arch="arm"` + `Arm="7"/"6"` so
    // `{{ .Arch }}v{{ .Arm }}` renders `armv7` rather than `armv7v7`. Every
    // other go-arch is carried whole in `Arch` with no variant suffix.
    if let Some(version) = arch.strip_prefix("armv") {
        vars.set("Arch", "arm");
        vars.set("Arm", version);
    } else {
        if arch == "amd64" {
            vars.set("Amd64", "v1");
        }
        vars.set("Arch", &arch);
    }
}

/// Reset every micro-architecture variant template var (`Arm`, `Arm64`,
/// `Amd64`, `Mips`, `I386`) to the empty string.
///
/// The single reset behind [`seed_target_vars`] and [`seed_variant_vars`],
/// also called directly by stages whose host-build (no-triple) paths must
/// clear the variant vars a previous target seeded — resetting a subset is
/// how a stale `Arm64="v8"` leaks into the next render.
pub fn reset_variant_vars(vars: &mut crate::template::TemplateVars) {
    vars.set("Arm", "");
    vars.set("Arm64", "");
    vars.set("Amd64", "");
    vars.set("Mips", "");
    vars.set("I386", "");
}

/// Seed the micro-architecture variant template vars (`Arm`, `Arm64`, `Amd64`,
/// `Mips`, `I386`) from a target triple's first component — the build/installer
/// naming policy, distinct from [`seed_target_vars`]'s archive-asset policy.
///
/// Where [`seed_target_vars`] arm-splits `Arch` for archive-asset names, this
/// policy never touches `Arch`; it seeds each family's GoReleaser-default
/// micro-architecture level so a user template referencing `{{ .Amd64 }}` /
/// `{{ .Arm64 }}` / `{{ .I386 }}` renders the same value in a build binary
/// name and in a makeself/AppImage filename for the same binary:
/// `aarch64` → `Arm64="v8"`, `x86_64` → `Amd64=<variant>` (the binary's
/// `amd64_variant` metadata, defaulting to the `"v1"` baseline when untagged),
/// `i686` → `I386="sse2"`.
///
/// `Arm` and `Mips` are NEVER seeded: every consumer of this policy carries
/// the whole `map_target` arch token (`armv7`, `mips64el`, …) in `Arch`, so a
/// non-empty `Arm`/`Mips` doubles every default filename that appends
/// [`MICRO_ARCH_VARIANT_SUFFIX`]'s `{% if Arm %}v{{ Arm }}` /
/// `{% if Mips %}_{{ Mips }}` clauses (`…_armv7v7.run`,
/// `…_mips64el_mips64el`) — the same contract [`seed_target_vars`] pins for
/// archive names, where the composite token instead splits into
/// `Arch="arm"` + `Arm="7"`.
///
/// All five vars are reset every call so a prior target's value cannot leak.
pub fn seed_variant_vars(
    vars: &mut crate::template::TemplateVars,
    target: &str,
    amd64_variant: Option<&str>,
) {
    reset_variant_vars(vars);
    match target.split('-').next().unwrap_or("") {
        "aarch64" => vars.set("Arm64", "v8"),
        "x86_64" => vars.set("Amd64", amd64_variant.unwrap_or("v1")),
        "i686" | "i386" | "i586" => vars.set("I386", "sse2"),
        _ => {}
    }
}

/// Seed the `Amd64` micro-architecture variant template var from a built
/// binary's `amd64_variant` metadata.
///
/// Installer stages call this per `(target, variant)` so a `name` template —
/// the stage default (which appends [`INSTALLER_AMD64_VARIANT_SUFFIX`]) or a
/// user override referencing `{{ Amd64 }}` — can disambiguate two amd64 builds
/// of the same target.
///
/// `arch` is the binary's [`map_target`]-mapped arch token: an amd64 binary
/// with no `amd64_variant` metadata seeds the `"v1"` baseline — the same
/// value [`seed_variant_vars`] and [`seed_target_vars`] give an untagged
/// x86-64 binary, so `{{ Amd64 }}` renders one value everywhere for the same
/// binary — while a non-amd64 binary seeds the empty string (the level is an
/// x86-64 dimension; every policy leaves it empty for other arches). The
/// default templates' `Amd64 != "v1"` guard keeps the baseline suffix-free,
/// preserving the single-variant historical name.
///
/// Takes the [`TemplateVars`](crate::template::TemplateVars) directly rather
/// than a [`Context`] so both the ctx-render installer stages
/// (appbundle/pkg/msi/dmg, which pass `ctx.template_vars_mut()`) and the
/// clone-render stage (nsis, which renders against a cloned `name_vars`) share
/// one helper.
pub fn seed_amd64_variant_var(
    vars: &mut crate::template::TemplateVars,
    arch: &str,
    amd64_variant: Option<&str>,
) {
    let value = match amd64_variant {
        Some(v) => v,
        None if arch == "amd64" => "v1",
        None => "",
    };
    vars.set("Amd64", value);
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
    render_archive_asset_name_with_variant(ctx, name_template, target, format, None)
}

/// [`render_archive_asset_name`] with the group's amd64 micro-architecture
/// level overlaid — the exact seeding sequence the archive stage performs
/// ([`seed_target_vars`] then [`seed_amd64_variant_var`] with the group's
/// `amd64_variant` metadata), so a derived name for a v2/v3-tuned target
/// carries the same `amd64v3`-style suffix the uploaded archive does.
///
/// Config-time callers obtain `amd64_variant` from
/// [`crate::build_env::config_time_amd64_variant`]; `None` renders the
/// baseline (identical to [`render_archive_asset_name`]).
pub fn render_archive_asset_name_with_variant(
    ctx: &mut Context,
    name_template: &str,
    target: &str,
    format: &str,
    amd64_variant: Option<&str>,
) -> Result<String> {
    let stem = if amd64_variant.is_some() {
        seed_target_vars(ctx, target);
        let (_, arch) = map_target(target);
        seed_amd64_variant_var(ctx.template_vars_mut(), &arch, amd64_variant);
        ctx.render_template(name_template)
            .with_context(|| format!("render archive name template for target '{target}'"))?
    } else {
        render_archive_stem(ctx, name_template, target)?
    };
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
        // amd64 seeds the unified "v1" baseline; the default template's
        // `Amd64 != "v1"` guard still emits nothing, so the asset name is
        // unchanged while `{{ Amd64 }}` matches the build/installer policies.
        assert_eq!(c.template_vars().get("Amd64").unwrap(), "v1");
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
    fn amd64_variant_var_is_arch_gated() {
        // The untagged fallback is ARCH-GATED, not unconditional: only an
        // amd64 binary falls back to the "v1" baseline; a non-amd64 binary
        // must seed the empty string (the level is an x86-64 dimension), or
        // a template's `{% if Amd64 %}` clause would fire on ARM assets. A
        // "simplified" unconditional `unwrap_or("v1")` fails the arm cases.
        let cases: [(&str, Option<&str>, &str); 4] = [
            ("arm64", None, ""),
            ("armv7", None, ""),
            ("amd64", None, "v1"),
            ("amd64", Some("v3"), "v3"),
        ];
        let mut c = ctx();
        for (arch, variant, expected) in cases {
            seed_amd64_variant_var(c.template_vars_mut(), arch, variant);
            assert_eq!(
                c.template_vars().get("Amd64").unwrap(),
                expected,
                "Amd64 for arch={arch} variant={variant:?}"
            );
        }
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
    fn archive_defaults_end_with_full_micro_arch_suffix() {
        // The Linux-capable archive defaults carry the FULL Arm/Mips/Amd64
        // suffix; both must keep it as their literal trailing clause so the
        // makeself stage (which composes from the same const) stays
        // byte-identical to them.
        assert!(DEFAULT_NAME_TEMPLATE.ends_with(MICRO_ARCH_VARIANT_SUFFIX));
        assert!(DEFAULT_BINARY_NAME_TEMPLATE.ends_with(MICRO_ARCH_VARIANT_SUFFIX));
    }

    #[test]
    fn installer_amd64_suffix_is_the_tail_of_the_full_suffix() {
        // The macOS/Windows installer family appends only the amd64 clause; it
        // must remain a true subset of the full suffix so the two families
        // disambiguate amd64 identically.
        assert!(MICRO_ARCH_VARIANT_SUFFIX.ends_with(INSTALLER_AMD64_VARIANT_SUFFIX));
    }

    #[test]
    fn variant_vars_mips_stays_empty_for_every_mips_goarch() {
        // Arch already carries the whole mips token, so a seeded Mips would
        // render the doubled `…_mips64el_mips64el` default filename.
        use crate::template::TemplateVars;
        for target in [
            "mips-unknown-linux-gnu",
            "mipsel-unknown-linux-gnu",
            "mips64-unknown-linux-gnuabi64",
            "mips64el-unknown-linux-gnuabi64",
        ] {
            let mut v = TemplateVars::new();
            seed_variant_vars(&mut v, target, None);
            assert_eq!(
                v.get("Mips").map(String::as_str),
                Some(""),
                "Mips must stay empty for {target}"
            );
        }
    }

    #[test]
    fn variant_vars_untagged_x86_64_seeds_amd64_baseline() {
        use crate::template::TemplateVars;
        let mut v = TemplateVars::new();
        seed_variant_vars(&mut v, "x86_64-unknown-linux-gnu", None);
        assert_eq!(v.get("Amd64").map(String::as_str), Some("v1"));
        // A tagged variant overrides the baseline.
        seed_variant_vars(&mut v, "x86_64-unknown-linux-gnu", Some("v3"));
        assert_eq!(v.get("Amd64").map(String::as_str), Some("v3"));
    }

    #[test]
    fn variant_vars_family_levels_and_reset() {
        use crate::template::TemplateVars;
        let mut v = TemplateVars::new();
        seed_variant_vars(&mut v, "aarch64-unknown-linux-gnu", None);
        assert_eq!(v.get("Arm64").map(String::as_str), Some("v8"));
        seed_variant_vars(&mut v, "i686-unknown-linux-gnu", None);
        assert_eq!(v.get("I386").map(String::as_str), Some("sse2"));
        assert_eq!(v.get("Arm64").map(String::as_str), Some(""));
    }

    #[test]
    fn variant_vars_arm_stays_empty_for_every_arm_token() {
        // Consumers of this policy carry the composite armv7/armv6 token in
        // `Arch`, so a seeded Arm would render the doubled `…_armv7v7.run`
        // default filename — the same doubling class the Mips guard pins.
        use crate::template::TemplateVars;
        for target in [
            "armv7-unknown-linux-gnueabihf",
            "armv7l-unknown-linux-gnueabihf",
            "armv6-unknown-linux-gnueabihf",
            "arm-unknown-linux-gnueabi",
        ] {
            let mut v = TemplateVars::new();
            seed_variant_vars(&mut v, target, None);
            assert_eq!(
                v.get("Arm").map(String::as_str),
                Some(""),
                "Arm must stay empty for {target}"
            );
        }
    }

    #[test]
    fn untagged_x86_64_renders_amd64_baseline_identically_across_policies() {
        // The unified baseline: the same untagged x86_64 binary renders
        // `{{ Amd64 }}` as "v1" through every seeding path — the archive
        // policy (seed_target_vars), the build/makeself/appimage policy
        // (seed_variant_vars), and the installer-stage policy
        // (seed_amd64_variant_var, used by msi/dmg/pkg/nsis/flatpak/nfpm/snap).
        use crate::template::TemplateVars;
        let target = "x86_64-unknown-linux-gnu";

        let mut archive = ctx();
        seed_target_vars(&mut archive, target);
        let archive_val = archive.template_vars().get("Amd64").cloned().unwrap();

        let mut build_vars = TemplateVars::new();
        seed_variant_vars(&mut build_vars, target, None);
        let build_val = build_vars.get("Amd64").cloned().unwrap();

        let mut installer_vars = TemplateVars::new();
        seed_amd64_variant_var(&mut installer_vars, "amd64", None);
        let installer_val = installer_vars.get("Amd64").cloned().unwrap();

        assert_eq!(archive_val, "v1");
        assert_eq!(build_val, archive_val);
        assert_eq!(installer_val, archive_val);
    }

    #[test]
    fn untagged_x86_64_default_archive_name_stays_suffix_free() {
        // The "v1" baseline must never surface in a default asset name — the
        // `Amd64 != "v1"` guard suppresses it, so the historical name holds.
        let mut c = ctx();
        let stem =
            render_archive_stem(&mut c, DEFAULT_NAME_TEMPLATE, "x86_64-unknown-linux-gnu").unwrap();
        assert_eq!(stem, "anodizer_1.2.3_linux_amd64");
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
