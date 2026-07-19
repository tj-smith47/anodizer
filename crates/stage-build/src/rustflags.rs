//! Reproducibility RUSTFLAGS merge, split from `run.rs`.

/// Merge reproducibility RUSTFLAGS for a build of `target` whose working
/// directory is `cwd`, without clobbering externally-set flags.
///
/// `config` (a per-target `build.env` RUSTFLAGS) wins when present;
/// otherwise fall back to `inherited` — the process env, where the
/// determinism harness places the Windows-MSVC reproducibility flags
/// (`/Brepro`, `/DEBUG:NONE`, `codegen-units=1`, ...) alongside its own
/// `--remap-path-prefix` rules. A `--remap-path-prefix=<cwd>=/build` rule
/// is appended so source paths normalize, UNLESS the chosen base already
/// remaps `cwd` (the harness remaps the worktree, which is `cwd`) — a
/// second rule for the same prefix is shadowed by rustc's first-match-wins
/// and would only mislead.
///
/// When `target` is a `*-pc-windows-msvc` triple, the
/// [`MSVC_DETERMINISM_RUSTFLAGS`](anodizer_core::determinism::MSVC_DETERMINISM_RUSTFLAGS)
/// set is merged in (deduplicating any token already present from config or
/// the inherited env). This is keyed on the TARGET triple, not the host, so
/// a Windows binary cross-built from Linux is reproducible too. Without it,
/// a consumer's `reproducible: true` Windows build would still stamp the PE
/// COFF `TimeDateStamp` (offset 0x108) with wall-clock time and drift.
///
/// The cargo build inherits the process env (`Command::envs` adds, does
/// not clear). Overwriting RUSTFLAGS here with only the remap rule would —
/// per cargo's `RUSTFLAGS` over `CARGO_TARGET_<triple>_RUSTFLAGS`
/// precedence — suppress the harness-injected flags and reintroduce the PE
/// `TimeDateStamp` drift on Windows. Blank (whitespace-only) values are
/// treated as unset.
pub(crate) fn merge_reproducible_rustflags(
    config: Option<&str>,
    inherited: Option<&str>,
    cwd: &str,
    target: &str,
) -> String {
    let base = config
        .filter(|s| !s.trim().is_empty())
        .or(inherited.filter(|s| !s.trim().is_empty()))
        .map(str::trim)
        .unwrap_or("");
    let with_remap = if base.is_empty() {
        format!("--remap-path-prefix={cwd}=/build")
    } else if base.contains(&format!("--remap-path-prefix={cwd}=")) {
        base.to_string()
    } else {
        format!("{base} --remap-path-prefix={cwd}=/build")
    };
    if anodizer_core::target::is_windows_msvc(target) {
        anodizer_core::determinism::merge_msvc_determinism_rustflags(&with_remap)
    } else {
        with_remap
    }
}

#[cfg(test)]
mod reproducible_rustflags_tests {
    use super::merge_reproducible_rustflags;

    const CWD: &str = "/work";
    const REMAP: &str = "--remap-path-prefix=/work=/build";
    // A non-MSVC target so the MSVC determinism merge stays off — these cases
    // exercise the remap/precedence logic in isolation.
    const LINUX: &str = "x86_64-unknown-linux-gnu";
    const WIN_MSVC: &str = "x86_64-pc-windows-msvc";

    #[test]
    fn preserves_inherited_msvc_flags_from_harness() {
        // The determinism harness injects /Brepro into the child's process
        // RUSTFLAGS. With no per-target config override, the build stage must
        // carry it through — clobbering it reintroduces the PE timestamp drift.
        let inherited = "-C link-arg=/Brepro -C link-arg=/DEBUG:NONE";
        let merged = merge_reproducible_rustflags(None, Some(inherited), CWD, LINUX);
        assert!(merged.contains("/Brepro"), "got {merged}");
        assert!(merged.contains("/DEBUG:NONE"), "got {merged}");
        assert!(merged.ends_with(REMAP), "remap must be appended: {merged}");
    }

    #[test]
    fn config_override_wins_over_inherited() {
        let merged = merge_reproducible_rustflags(
            Some("-C target-cpu=native"),
            Some("-C link-arg=/Brepro"),
            CWD,
            LINUX,
        );
        assert_eq!(merged, format!("-C target-cpu=native {REMAP}"));
    }

    #[test]
    fn remap_only_when_nothing_inherited() {
        assert_eq!(merge_reproducible_rustflags(None, None, CWD, LINUX), REMAP);
        // Blank (whitespace-only) values are treated as unset, not as a real
        // base to append to — no leading-space artifact.
        assert_eq!(
            merge_reproducible_rustflags(Some(""), Some("  "), CWD, LINUX),
            REMAP
        );
    }

    #[test]
    fn does_not_double_remap_when_cwd_already_remapped() {
        // The harness already remaps the worktree (== cwd) to /anodize.
        // A second rule for the same prefix is shadowed (rustc first-match-
        // wins) and only misleads, so it must not be appended.
        let inherited = "-C link-arg=/Brepro --remap-path-prefix=/work=/anodize";
        let merged = merge_reproducible_rustflags(None, Some(inherited), CWD, LINUX);
        assert!(
            merged.starts_with(inherited),
            "must not append a shadowed cwd remap: {merged}"
        );
        assert_eq!(
            merged.matches("--remap-path-prefix=/work=").count(),
            1,
            "exactly one remap rule for the cwd prefix: {merged}"
        );
    }

    /// Regression (PE TimeDateStamp drift): a `reproducible: true` build
    /// targeting `x86_64-pc-windows-msvc` must emit the full MSVC
    /// determinism flag set — keyed on the TARGET triple, so it fires even
    /// when cross-building Windows from a Linux host (where `cfg!(windows)`
    /// is false). Without `/Brepro` the COFF TimeDateStamp at offset 0x108
    /// is wall-clock and the .exe drifts between rebuilds.
    #[test]
    fn windows_msvc_target_gets_full_determinism_flag_set() {
        let merged = merge_reproducible_rustflags(None, None, CWD, WIN_MSVC);
        for needle in [
            "-C codegen-units=1",
            "-C link-arg=/Brepro",
            "-C link-arg=/OPT:NOICF",
            "-C link-arg=/INCREMENTAL:NO",
            "-C link-arg=/DEBUG:NONE",
            "-C strip=symbols",
        ] {
            assert!(
                merged.contains(needle),
                "windows-msvc reproducible build must carry `{needle}`. got={merged}"
            );
        }
        assert!(
            merged.contains(REMAP),
            "remap rule must still be present: {merged}"
        );
    }

    /// A non-MSVC target must NOT receive the MSVC-linker-only flags —
    /// `/Brepro` and the `/...` link args make lld / ld error.
    #[test]
    fn non_msvc_target_gets_no_brepro() {
        let merged = merge_reproducible_rustflags(None, None, CWD, LINUX);
        assert!(
            !merged.contains("/Brepro"),
            "linux target must not carry the MSVC-only /Brepro: {merged}"
        );
        assert_eq!(
            merged, REMAP,
            "linux reproducible build is remap-only: {merged}"
        );
    }

    /// Aarch64 Windows-MSVC is also covered by the target-keyed gate.
    #[test]
    fn aarch64_windows_msvc_target_gets_brepro() {
        let merged = merge_reproducible_rustflags(None, None, CWD, "aarch64-pc-windows-msvc");
        assert!(merged.contains("-C link-arg=/Brepro"), "got={merged}");
    }

    /// Idempotence: an inherited MSVC set (e.g. from the harness env) is not
    /// duplicated when the target-keyed merge runs over it.
    #[test]
    fn windows_msvc_merge_does_not_duplicate_inherited_brepro() {
        let inherited = "-C codegen-units=1 -C link-arg=/Brepro";
        let merged = merge_reproducible_rustflags(None, Some(inherited), CWD, WIN_MSVC);
        assert_eq!(
            merged.matches("/Brepro").count(),
            1,
            "/Brepro must appear exactly once: {merged}"
        );
        assert_eq!(
            merged.matches("codegen-units=1").count(),
            1,
            "codegen-units=1 must appear exactly once: {merged}"
        );
    }
}
