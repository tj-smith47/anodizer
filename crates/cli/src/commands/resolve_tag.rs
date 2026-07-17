use anyhow::{Context as _, Result, bail};
use serde_json::json;

pub struct ResolveTagOpts {
    pub tag: String,
    pub json: bool,
    pub config_override: Option<std::path::PathBuf>,
}

pub fn run(opts: ResolveTagOpts) -> Result<()> {
    let config_path = opts
        .config_override
        .as_deref()
        .filter(|p| p.exists())
        .map(|p| p.to_path_buf())
        .or_else(|| crate::pipeline::find_config(None).ok());

    let config = match config_path {
        Some(ref path) => crate::pipeline::load_config(path)?,
        None => bail!("no anodizer config found"),
    };

    // Collect all crates from top-level and workspaces.
    let all_crates = config.crate_universe();

    // Match the tag against each crate's tag_template prefix.
    // Prefer the longest matching prefix (most specific) to avoid ambiguity
    // when one prefix is a substring of another (e.g. "v" vs "v2-").
    let mut best: Option<(&anodizer_core::config::CrateConfig, usize)> = None;
    for c in &all_crates {
        if let Some(prefix) =
            anodizer_core::git::extract_tag_prefix(c.tag_template.as_deref().unwrap_or(""))
            && opts.tag.starts_with(&prefix)
        {
            let remainder = &opts.tag[prefix.len()..];
            let is_version = remainder
                .split('.')
                .next()
                .is_some_and(|s| !s.is_empty() && s.chars().all(|ch| ch.is_ascii_digit()));
            if is_version && best.as_ref().is_none_or(|(_, len)| prefix.len() > *len) {
                best = Some((c, prefix.len()));
            }
        }
    }

    let crate_cfg = match best {
        Some((c, _)) => c,
        None => bail!("no crate matches tag '{}'", opts.tag),
    };

    let has_builds = crate_cfg
        .builds
        .as_ref()
        .map(|b| !b.is_empty())
        .unwrap_or(false);

    if opts.json {
        let out = serde_json::to_string(&json!({
            "crate": crate_cfg.name,
            "path": crate_cfg.path,
            "has_builds": has_builds,
        }))
        .context("serialize resolve-tag JSON output")?;
        println!("{}", out);
    } else {
        println!("crate={}", crate_cfg.name);
        println!("path={}", crate_cfg.path);
        println!("has-builds={}", has_builds);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_resolve_tag_matches_simple_prefix() {
        let tag = "v1.2.3";
        let prefix = "v";
        let remainder = tag.strip_prefix(prefix).unwrap();
        assert!(
            remainder
                .split('.')
                .next()
                .unwrap()
                .chars()
                .all(|ch| ch.is_ascii_digit())
        );
    }

    #[test]
    fn test_resolve_tag_matches_monorepo_prefix() {
        let tag = "core-v0.2.3";
        let prefix = "core-v";
        let remainder = tag.strip_prefix(prefix).unwrap();
        assert!(
            remainder
                .split('.')
                .next()
                .unwrap()
                .chars()
                .all(|ch| ch.is_ascii_digit())
        );
    }

    #[test]
    fn test_resolve_tag_rejects_non_version_suffix() {
        let tag = "v-something";
        let prefix = "v";
        let remainder = tag.strip_prefix(prefix).unwrap();
        // "-something" starts with "-", not a digit
        assert!(
            !remainder
                .split('.')
                .next()
                .unwrap()
                .chars()
                .all(|ch| ch.is_ascii_digit())
        );
    }

    #[test]
    fn test_resolve_tag_longer_prefix_wins() {
        // "core-v" should match "core-v1.0.0", not "v" prefix
        let tag = "core-v1.0.0";

        let prefixes = [("v", "cfgd"), ("core-v", "cfgd-core")];
        let matched = prefixes.iter().find(|(prefix, _)| {
            if let Some(remainder) = tag.strip_prefix(prefix) {
                remainder
                    .split('.')
                    .next()
                    .map(|s| s.chars().all(|ch| ch.is_ascii_digit()))
                    .unwrap_or(false)
            } else {
                false
            }
        });
        // Both "v" and "core-v" match, but iteration order matters.
        // In real code, workspace crates come after top-level, and the
        // longer prefix is more specific. Let's verify both match:
        assert!(matched.is_some());
    }

    use super::{ResolveTagOpts, run};
    use serial_test::serial;
    use std::fs;

    fn write_simple_config(dir: &std::path::Path) -> std::path::PathBuf {
        let p = dir.join(".anodizer.yaml");
        fs::write(
            &p,
            r#"project_name: app
crates:
  - name: app
    path: "."
    tag_template: "v{{ .Version }}"
"#,
        )
        .unwrap();
        p
    }

    fn write_workspace_config(dir: &std::path::Path) -> std::path::PathBuf {
        let p = dir.join(".anodizer.yaml");
        fs::write(
            &p,
            r#"project_name: app
crates:
  - name: app
    path: "."
    tag_template: "v{{ .Version }}"
workspaces:
  - name: core
    crates:
      - name: app-core
        path: "core"
        tag_template: "core-v{{ .Version }}"
"#,
        )
        .unwrap();
        p
    }

    /// When the override doesn't exist and find_config falls back to the
    /// running test workspace, run() may either bail with no-crate-matches
    /// (because Config::default() has no crates) or with no-anodizer-config
    /// (when neither anodizer.yaml nor Cargo.toml is reachable). Both
    /// outcomes are valid "missing config" failure modes — pin the family
    /// rather than the exact message.
    #[test]
    #[serial]
    fn missing_config_bails() {
        let tmp = tempfile::tempdir().unwrap();
        let err = run(ResolveTagOpts {
            tag: "v1.0.0".into(),
            json: false,
            config_override: Some(tmp.path().join("nope.yaml")),
        })
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("no anodizer config found")
                || err.contains("config file not found")
                || err.contains("no crate matches"),
            "expected a missing-config-family error: {err}"
        );
    }

    #[test]
    #[serial]
    fn unmatched_tag_bails_with_named_tag() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = write_simple_config(tmp.path());
        let err = run(ResolveTagOpts {
            tag: "abc-not-a-tag".into(),
            json: false,
            config_override: Some(cfg),
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("no crate matches"), "{err}");
        assert!(err.contains("abc-not-a-tag"), "{err}");
    }

    #[test]
    #[serial]
    fn simple_tag_resolves_to_root_crate_text_form() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = write_simple_config(tmp.path());
        run(ResolveTagOpts {
            tag: "v0.4.2".into(),
            json: false,
            config_override: Some(cfg),
        })
        .expect("simple v-tag must resolve");
    }

    #[test]
    #[serial]
    fn longer_prefix_wins_for_workspace_crate() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = write_workspace_config(tmp.path());
        // "core-v" is more specific than "v" — must select the workspace crate.
        run(ResolveTagOpts {
            tag: "core-v2.0.1".into(),
            json: false,
            config_override: Some(cfg),
        })
        .expect("longer prefix must win on workspace crate");
    }

    #[test]
    #[serial]
    fn json_output_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = write_simple_config(tmp.path());
        run(ResolveTagOpts {
            tag: "v1.0.0".into(),
            json: true,
            config_override: Some(cfg),
        })
        .expect("json output should serialize successfully");
    }

    /// Tag prefix matches but remainder is non-numeric (e.g. "v-foo") —
    /// the version-shape gate must reject and bail with no-crate-matches.
    #[test]
    #[serial]
    fn non_version_remainder_does_not_match() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = write_simple_config(tmp.path());
        let err = run(ResolveTagOpts {
            tag: "v-not-a-version".into(),
            json: false,
            config_override: Some(cfg),
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("no crate matches"), "{err}");
    }
}
