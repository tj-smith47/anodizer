//! Shell-completion and man-page generation config for archive entries.
//!
//! Both [`CompletionsConfig`] and [`ManpagesConfig`] are turnkey sugar over
//! the existing build-hook + `files:` glob machinery: a single block on an
//! archive entry auto-generates completion/man files and bundles them into
//! every archive (and any nfpm package whose `contents:` globs the shared
//! dist staging directory).
//!
//! Three mutually-exclusive generation modes are supported, matching the
//! patterns real projects use:
//!
//! - **Mode A — `generate:`**: run the built binary once on the host-native
//!   target (e.g. `<bin> completions <shell>`), capturing stdout per shell.
//!   Completions/man pages do not vary by architecture, so the host output is
//!   reused for every archive across all targets.
//! - **Mode B — `from_build_out:`**: harvest files a `build.rs`
//!   (clap_complete / clap_mangen) already wrote into `target/.../out/` via a
//!   per-target glob.
//! - **Mode C — `copy:`**: copy committed files from a static path/glob.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Shell-completion generation for an archive entry.
///
/// Exactly one of `generate` / `from_build_out` / `copy` must be set
/// (validated at deserialize time). Setting none is a no-op; setting more
/// than one is a config error.
///
/// YAML examples:
/// ```yaml
/// completions:
///   generate: "{{ ArtifactPath }} completions {{ Shell }}"   # mode A
///   shells: [bash, zsh, fish, powershell, nushell, elvish]
///   dst: "completions/"
/// # mode B: from_build_out: "**/out/{{ Binary }}.{bash,fish}"
/// # mode C: copy: "contrib/completion/*"
/// ```
///
/// `{{ Binary }}` resolves to the host-native binary's recorded name; when
/// no host artifact exists (modes B/C on a pure cross build) it falls back to
/// the crate name. If your binary name differs from the crate name, spell it
/// literally in the glob rather than relying on `{{ Binary }}` in that case.
#[derive(Debug, Clone, Serialize, Default, JsonSchema, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct CompletionsConfig {
    /// Mode A: command run once on the host-native binary, per shell. The
    /// `{{ Shell }}` template var is bound to each entry in `shells`, and
    /// `{{ ArtifactPath }}` / `{{ Binary }}` reference the built binary.
    /// Stdout is captured into one file per shell under `dst`.
    pub generate: Option<String>,
    /// Mode B: per-target glob harvesting files a `build.rs` wrote into the
    /// crate's `OUT_DIR` (e.g. `"**/out/{{ Binary }}.{bash,fish}"`).
    pub from_build_out: Option<String>,
    /// Mode C: glob/path of committed completion files to copy verbatim.
    pub copy: Option<String>,
    /// Shells to generate for in mode A. Arbitrary user-supplied list — not
    /// limited to bash/zsh/fish/powershell; `nushell`, `elvish`, `fig`, etc.
    /// are all valid. Ignored by modes B and C. Defaults to the four common
    /// shells when omitted in mode A.
    pub shells: Option<Vec<String>>,
    /// Destination directory inside the archive (and dist staging tree) for
    /// the generated files. Defaults to `"completions/"`.
    pub dst: Option<String>,
}

/// Man-page generation for an archive entry.
///
/// Same three mutually-exclusive modes as [`CompletionsConfig`], minus the
/// per-shell `shells` axis (man pages are shell-agnostic).
///
/// YAML examples:
/// ```yaml
/// manpages:
///   generate: "{{ ArtifactPath }} --man"     # mode A
///   dst: "man/man1/"
/// # mode B: from_build_out: "**/out/{{ Binary }}.1"
/// # mode C: copy: "man/*.1"
/// ```
#[derive(Debug, Clone, Serialize, Default, JsonSchema, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ManpagesConfig {
    /// Mode A: command run once on the host-native binary; stdout is captured
    /// into a single man file under `dst` named `<binary>.1`.
    pub generate: Option<String>,
    /// Mode B: per-target glob harvesting man files a `build.rs` wrote into
    /// the crate's `OUT_DIR` (e.g. `"**/out/{{ Binary }}.1"`).
    pub from_build_out: Option<String>,
    /// Mode C: glob/path of committed man files to copy verbatim.
    pub copy: Option<String>,
    /// Destination directory inside the archive (and dist staging tree) for
    /// the generated man files. Defaults to `"man/man1/"`.
    pub dst: Option<String>,
}

/// The resolved generation mode for a completions/manpages block.
///
/// Returned by [`CompletionsConfig::mode`] / [`ManpagesConfig::mode`] after
/// the exactly-one-set invariant has been enforced at deserialize time, so
/// the stage never has to re-validate mutual exclusivity.
#[derive(Debug, Clone, PartialEq)]
pub enum GenMode<'a> {
    /// Run the host binary (mode A): the `generate:` command template.
    Generate(&'a str),
    /// Harvest a `build.rs` OUT_DIR via a per-target glob (mode B).
    FromBuildOut(&'a str),
    /// Copy committed files from a path/glob (mode C).
    Copy(&'a str),
    /// No mode set — the block is a no-op.
    None,
}

/// Count how many of the three mutually-exclusive mode fields are set, and
/// return a `serde`-friendly error when more than one is. Shared by the two
/// blocks' `Deserialize` impls so the diagnostic wording stays in one place.
fn enforce_single_mode<E: serde::de::Error>(
    block: &str,
    generate: bool,
    from_build_out: bool,
    copy: bool,
) -> Result<(), E> {
    let set: Vec<&str> = [
        ("generate", generate),
        ("from_build_out", from_build_out),
        ("copy", copy),
    ]
    .into_iter()
    .filter_map(|(name, on)| on.then_some(name))
    .collect();
    if set.len() > 1 {
        return Err(E::custom(format!(
            "{block}: only one of `generate`, `from_build_out`, `copy` may be set \
             (got {}); these are mutually-exclusive generation modes",
            set.join(", ")
        )));
    }
    Ok(())
}

impl CompletionsConfig {
    /// Default destination directory inside the archive.
    pub const DEFAULT_DST: &'static str = "completions/";

    /// The four shells generated for when `shells:` is omitted in mode A.
    pub const DEFAULT_SHELLS: &'static [&'static str] = &["bash", "zsh", "fish", "powershell"];

    /// Resolve which generation mode (if any) this block selects. Mutual
    /// exclusivity is already guaranteed by the `Deserialize` impl, so this
    /// returns the first set field deterministically.
    pub fn mode(&self) -> GenMode<'_> {
        if let Some(g) = self.generate.as_deref() {
            GenMode::Generate(g)
        } else if let Some(b) = self.from_build_out.as_deref() {
            GenMode::FromBuildOut(b)
        } else if let Some(c) = self.copy.as_deref() {
            GenMode::Copy(c)
        } else {
            GenMode::None
        }
    }

    /// Resolve the destination directory, falling back to [`Self::DEFAULT_DST`].
    pub fn resolved_dst(&self) -> &str {
        self.dst.as_deref().unwrap_or(Self::DEFAULT_DST)
    }

    /// Resolve the shells list for mode A, falling back to the four common
    /// shells when the user did not supply one.
    pub fn resolved_shells(&self) -> Vec<String> {
        match &self.shells {
            Some(s) if !s.is_empty() => s.clone(),
            _ => Self::DEFAULT_SHELLS.iter().map(|s| s.to_string()).collect(),
        }
    }
}

impl ManpagesConfig {
    /// Default destination directory inside the archive.
    pub const DEFAULT_DST: &'static str = "man/man1/";

    /// Resolve which generation mode (if any) this block selects.
    pub fn mode(&self) -> GenMode<'_> {
        if let Some(g) = self.generate.as_deref() {
            GenMode::Generate(g)
        } else if let Some(b) = self.from_build_out.as_deref() {
            GenMode::FromBuildOut(b)
        } else if let Some(c) = self.copy.as_deref() {
            GenMode::Copy(c)
        } else {
            GenMode::None
        }
    }

    /// Resolve the destination directory, falling back to [`Self::DEFAULT_DST`].
    pub fn resolved_dst(&self) -> &str {
        self.dst.as_deref().unwrap_or(Self::DEFAULT_DST)
    }
}

// Custom Deserialize impls enforce the exactly-one-mode invariant at config
// load time so the archive stage can `match self.mode()` without re-checking.
// They mirror the field set of the derived struct (kept in sync by hand —
// the field count is small and stable).

impl<'de> Deserialize<'de> for CompletionsConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Raw {
            generate: Option<String>,
            from_build_out: Option<String>,
            copy: Option<String>,
            shells: Option<Vec<String>>,
            dst: Option<String>,
        }
        let raw = Raw::deserialize(deserializer)?;
        enforce_single_mode(
            "completions",
            raw.generate.is_some(),
            raw.from_build_out.is_some(),
            raw.copy.is_some(),
        )?;
        Ok(CompletionsConfig {
            generate: raw.generate,
            from_build_out: raw.from_build_out,
            copy: raw.copy,
            shells: raw.shells,
            dst: raw.dst,
        })
    }
}

impl<'de> Deserialize<'de> for ManpagesConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Raw {
            generate: Option<String>,
            from_build_out: Option<String>,
            copy: Option<String>,
            dst: Option<String>,
        }
        let raw = Raw::deserialize(deserializer)?;
        enforce_single_mode(
            "manpages",
            raw.generate.is_some(),
            raw.from_build_out.is_some(),
            raw.copy.is_some(),
        )?;
        Ok(ManpagesConfig {
            generate: raw.generate,
            from_build_out: raw.from_build_out,
            copy: raw.copy,
            dst: raw.dst,
        })
    }
}

/// The conventional on-disk filename a shell expects its completion file to
/// have, given the binary name. Mirrors clap_complete's emitted names so
/// generated files drop straight into the shell's lookup path once unpacked:
///
/// | shell      | filename        |
/// |------------|-----------------|
/// | bash       | `<name>`        |
/// | zsh        | `_<name>`       |
/// | fish       | `<name>.fish`   |
/// | powershell | `_<name>.ps1`   |
/// | elvish     | `<name>.elv`    |
/// | nushell    | `<name>.nu`     |
/// | fig        | `<name>.ts`     |
///
/// Unknown shells fall back to `<name>.<shell>` so an arbitrary user-supplied
/// shell still produces a deterministic, collision-free filename.
pub fn completion_filename(binary: &str, shell: &str) -> String {
    match shell.to_ascii_lowercase().as_str() {
        "bash" => binary.to_string(),
        "zsh" => format!("_{binary}"),
        "fish" => format!("{binary}.fish"),
        "powershell" | "pwsh" => format!("_{binary}.ps1"),
        "elvish" => format!("{binary}.elv"),
        "nushell" | "nu" => format!("{binary}.nu"),
        "fig" => format!("{binary}.ts"),
        other => format!("{binary}.{other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_completions(yaml: &str) -> Result<CompletionsConfig, serde_yaml_ng::Error> {
        serde_yaml_ng::from_str(yaml)
    }

    #[test]
    fn mode_a_generate_parses() {
        let c = parse_completions(
            "generate: \"{{ .ArtifactPath }} completions {{ .Shell }}\"\nshells: [bash, zsh, nushell, elvish]\ndst: \"completions/\"",
        )
        .unwrap();
        assert_eq!(
            c.mode(),
            GenMode::Generate("{{ .ArtifactPath }} completions {{ .Shell }}")
        );
        assert_eq!(
            c.resolved_shells(),
            vec!["bash", "zsh", "nushell", "elvish"]
        );
        assert_eq!(c.resolved_dst(), "completions/");
    }

    #[test]
    fn mode_b_from_build_out_parses() {
        let c = parse_completions("from_build_out: \"**/out/{{ .Binary }}.{bash,fish}\"").unwrap();
        assert_eq!(
            c.mode(),
            GenMode::FromBuildOut("**/out/{{ .Binary }}.{bash,fish}")
        );
    }

    #[test]
    fn mode_c_copy_parses() {
        let c = parse_completions("copy: \"contrib/completion/*\"").unwrap();
        assert_eq!(c.mode(), GenMode::Copy("contrib/completion/*"));
    }

    #[test]
    fn two_modes_at_once_is_error() {
        let err = parse_completions("generate: \"x\"\ncopy: \"y\"").unwrap_err();
        assert!(
            err.to_string().contains("only one of"),
            "expected mutual-exclusivity error, got: {err}"
        );
    }

    #[test]
    fn no_mode_is_noop() {
        let c = parse_completions("shells: [bash]").unwrap();
        assert_eq!(c.mode(), GenMode::None);
    }

    #[test]
    fn default_shells_when_omitted() {
        let c = parse_completions("generate: \"x\"").unwrap();
        assert_eq!(
            c.resolved_shells(),
            vec!["bash", "zsh", "fish", "powershell"]
        );
    }

    #[test]
    fn manpages_two_modes_error() {
        let err: Result<ManpagesConfig, _> =
            serde_yaml_ng::from_str("generate: \"x\"\nfrom_build_out: \"y\"");
        assert!(err.unwrap_err().to_string().contains("only one of"));
    }

    #[test]
    fn manpages_default_dst() {
        let m: ManpagesConfig =
            serde_yaml_ng::from_str("generate: \"{{ .ArtifactPath }} --man\"").unwrap();
        assert_eq!(m.resolved_dst(), "man/man1/");
    }

    #[test]
    fn completion_filenames_follow_clap_convention() {
        assert_eq!(completion_filename("rg", "bash"), "rg");
        assert_eq!(completion_filename("rg", "zsh"), "_rg");
        assert_eq!(completion_filename("rg", "fish"), "rg.fish");
        assert_eq!(completion_filename("rg", "powershell"), "_rg.ps1");
        assert_eq!(completion_filename("rg", "elvish"), "rg.elv");
        assert_eq!(completion_filename("rg", "nushell"), "rg.nu");
        // arbitrary shell → deterministic fallback
        assert_eq!(completion_filename("rg", "fig"), "rg.ts");
        assert_eq!(completion_filename("rg", "weirdshell"), "rg.weirdshell");
    }

    #[test]
    fn completion_filename_accepts_shell_aliases_case_insensitively() {
        // `pwsh` maps to the same file as `powershell`; `nu` as `nushell`.
        assert_eq!(completion_filename("rg", "pwsh"), "_rg.ps1");
        assert_eq!(completion_filename("rg", "nu"), "rg.nu");
        // Shell name is lower-cased before matching.
        assert_eq!(completion_filename("rg", "BASH"), "rg");
        assert_eq!(completion_filename("rg", "Fish"), "rg.fish");
    }

    #[test]
    fn empty_shells_list_falls_back_to_defaults() {
        // An explicit empty list is treated as "unset" — the four common shells.
        let c = parse_completions("generate: \"x\"\nshells: []").unwrap();
        assert_eq!(
            c.resolved_shells(),
            vec!["bash", "zsh", "fish", "powershell"]
        );
    }

    #[test]
    fn manpages_mode_resolves_all_three_variants() {
        let generate: ManpagesConfig =
            serde_yaml_ng::from_str("generate: \"{{ .ArtifactPath }} --man\"").unwrap();
        assert_eq!(
            generate.mode(),
            GenMode::Generate("{{ .ArtifactPath }} --man")
        );
        let harvest: ManpagesConfig =
            serde_yaml_ng::from_str("from_build_out: \"**/out/{{ .Binary }}.1\"").unwrap();
        assert_eq!(
            harvest.mode(),
            GenMode::FromBuildOut("**/out/{{ .Binary }}.1")
        );
        let copy: ManpagesConfig = serde_yaml_ng::from_str("copy: \"man/*.1\"").unwrap();
        assert_eq!(copy.mode(), GenMode::Copy("man/*.1"));
        // No mode set is a no-op.
        let none: ManpagesConfig = serde_yaml_ng::from_str("dst: \"man/\"").unwrap();
        assert_eq!(none.mode(), GenMode::None);
    }

    #[test]
    fn manpages_rejects_unknown_field() {
        assert!(serde_yaml_ng::from_str::<ManpagesConfig>("shells: [bash]").is_err());
    }
}
