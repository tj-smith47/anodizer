use anodize_core::config::TagConfig;
use anodize_core::git;
use anyhow::{Result, bail};
use colored::Colorize;
use regex::Regex;

pub struct TagOpts {
    pub dry_run: bool,
    pub custom_tag: Option<String>,
    pub default_bump: Option<String>,
    /// Reserved for workspace/monorepo support (tag a specific crate).
    #[allow(dead_code)]
    pub crate_name: Option<String>,
    pub config_override: Option<std::path::PathBuf>,
    pub verbose: bool,
}

/// Resolved tag configuration with defaults applied.
struct ResolvedConfig {
    default_bump: String,
    tag_prefix: String,
    release_branches: Vec<String>,
    custom_tag: Option<String>,
    tag_context: String,
    branch_history: String,
    initial_version: String,
    prerelease: bool,
    prerelease_suffix: String,
    force_without_changes: bool,
    force_without_changes_pre: bool,
    major_string_token: String,
    minor_string_token: String,
    patch_string_token: String,
    none_string_token: String,
    verbose: bool,
}

impl ResolvedConfig {
    fn from_tag_config(cfg: &TagConfig, opts: &TagOpts) -> Self {
        ResolvedConfig {
            default_bump: opts
                .default_bump
                .clone()
                .or_else(|| cfg.default_bump.clone())
                .unwrap_or_else(|| "minor".to_string()),
            tag_prefix: cfg
                .tag_prefix
                .clone()
                .unwrap_or_else(|| "v".to_string()),
            release_branches: cfg.release_branches.clone().unwrap_or_default(),
            custom_tag: opts
                .custom_tag
                .clone()
                .or_else(|| cfg.custom_tag.clone()),
            tag_context: cfg
                .tag_context
                .clone()
                .unwrap_or_else(|| "repo".to_string()),
            branch_history: cfg
                .branch_history
                .clone()
                .unwrap_or_else(|| "compare".to_string()),
            initial_version: cfg
                .initial_version
                .clone()
                .unwrap_or_else(|| "0.0.0".to_string()),
            prerelease: cfg.prerelease.unwrap_or(false),
            prerelease_suffix: cfg
                .prerelease_suffix
                .clone()
                .unwrap_or_else(|| "beta".to_string()),
            force_without_changes: cfg.force_without_changes.unwrap_or(false),
            force_without_changes_pre: cfg.force_without_changes_pre.unwrap_or(false),
            major_string_token: cfg
                .major_string_token
                .clone()
                .unwrap_or_else(|| "#major".to_string()),
            minor_string_token: cfg
                .minor_string_token
                .clone()
                .unwrap_or_else(|| "#minor".to_string()),
            patch_string_token: cfg
                .patch_string_token
                .clone()
                .unwrap_or_else(|| "#patch".to_string()),
            none_string_token: cfg
                .none_string_token
                .clone()
                .unwrap_or_else(|| "#none".to_string()),
            verbose: cfg.verbose.unwrap_or(true) || opts.verbose,
        }
    }
}

pub fn run(opts: TagOpts) -> Result<()> {
    // Load config if available, but don't fail if there's no config file
    let tag_config = load_tag_config(&opts);

    let cfg = ResolvedConfig::from_tag_config(&tag_config, &opts);

    if cfg.verbose {
        eprintln!(
            "{} running auto-tag{}",
            "anodize tag:".cyan().bold(),
            if opts.dry_run { " (dry-run)" } else { "" }
        );
    }

    // If custom_tag is set, use it directly
    if let Some(ref custom) = cfg.custom_tag {
        let new_tag = if custom.starts_with(&cfg.tag_prefix) {
            custom.clone()
        } else {
            format!("{}{}", cfg.tag_prefix, custom)
        };
        if cfg.verbose {
            eprintln!("  using custom tag: {}", new_tag);
        }
        git::create_and_push_tag(&new_tag, &format!("Release {}", new_tag), opts.dry_run)?;
        println!("new_tag={}", new_tag);
        println!("old_tag=");
        println!("part=custom");
        return Ok(());
    }

    // Check release branches
    let current_branch = git::get_current_branch()?;
    if !cfg.release_branches.is_empty() && !branch_matches(&current_branch, &cfg.release_branches)
    {
        // Non-release branch: produce a hash-postfixed version, don't tag
        let short_commit = git::get_short_commit()?;
        let prev_tag = find_previous_tag(&cfg)?;
        let base_version = match &prev_tag {
            Some(tag) => {
                let sv = git::parse_semver(tag)?;
                format!("{}.{}.{}", sv.major, sv.minor, sv.patch)
            }
            None => cfg.initial_version.clone(),
        };
        let hash_tag = format!("{}{}-{}", cfg.tag_prefix, base_version, short_commit);
        if cfg.verbose {
            eprintln!(
                "  branch '{}' is not a release branch, producing hash-postfixed version: {}",
                current_branch, hash_tag
            );
        }
        println!("new_tag={}", hash_tag);
        println!("old_tag={}", prev_tag.as_deref().unwrap_or(""));
        println!("part=none");
        return Ok(());
    }

    // Find previous tag
    let prev_tag = find_previous_tag(&cfg)?;

    if cfg.verbose {
        eprintln!(
            "  previous tag: {}",
            prev_tag.as_deref().unwrap_or("(none)")
        );
    }

    // Check for changes since last tag
    if let Some(ref tag) = prev_tag {
        let has_changes = git::has_commits_since_tag(tag)?;
        if !has_changes {
            let force = if cfg.prerelease {
                cfg.force_without_changes_pre
            } else {
                cfg.force_without_changes
            };
            if !force {
                if cfg.verbose {
                    eprintln!("  no changes since {} — skipping", tag);
                }
                println!("new_tag={}", tag);
                println!("old_tag={}", tag);
                println!("part=none");
                return Ok(());
            }
            if cfg.verbose {
                eprintln!("  no changes since {}, but force_without_changes is enabled", tag);
            }
        }
    }

    // Scan commit messages to determine bump
    let messages = get_messages_for_bump(&cfg, prev_tag.as_deref())?;
    if cfg.verbose {
        eprintln!("  scanned {} commit message(s)", messages.len());
    }

    // Detect bump
    let bump = detect_bump(&messages, &cfg);
    if cfg.verbose {
        eprintln!("  detected bump: {:?}", bump);
    }

    // If #none token detected, skip tagging
    if bump == BumpKind::None {
        if cfg.verbose {
            eprintln!("  #none token found — skipping tag");
        }
        println!("new_tag={}", prev_tag.as_deref().unwrap_or(""));
        println!("old_tag={}", prev_tag.as_deref().unwrap_or(""));
        println!("part=none");
        return Ok(());
    }

    // Determine base version
    let base = match &prev_tag {
        Some(tag) => git::parse_semver(tag)?,
        None => git::parse_semver(&format!("{}{}", cfg.tag_prefix, cfg.initial_version))
            .unwrap_or(git::SemVer {
                major: 0,
                minor: 0,
                patch: 0,
                prerelease: None,
            }),
    };

    // Apply bump
    let (new_major, new_minor, new_patch) = apply_bump(base.major, base.minor, base.patch, &bump);

    // Build new version string
    let mut new_version = format!("{}.{}.{}", new_major, new_minor, new_patch);

    // Handle prerelease
    if cfg.prerelease {
        new_version = format!("{}-{}", new_version, cfg.prerelease_suffix);
    }

    let new_tag = format!("{}{}", cfg.tag_prefix, new_version);
    let old_tag = prev_tag.as_deref().unwrap_or("");

    if cfg.verbose {
        eprintln!("  {} -> {}", old_tag, new_tag);
    }

    // Create and push tag
    git::create_and_push_tag(&new_tag, &format!("Release {}", new_tag), opts.dry_run)?;

    let part_str = match bump {
        BumpKind::Major => "major",
        BumpKind::Minor => "minor",
        BumpKind::Patch => "patch",
        BumpKind::None => "none",
    };

    println!("new_tag={}", new_tag);
    println!("old_tag={}", old_tag);
    println!("part={}", part_str);

    Ok(())
}

fn load_tag_config(opts: &TagOpts) -> TagConfig {
    let config_path = opts
        .config_override
        .as_deref()
        .and_then(|p| {
            if p.exists() {
                Some(p.to_path_buf())
            } else {
                None
            }
        })
        .or_else(|| crate::pipeline::find_config(None).ok());

    if let Some(path) = config_path
        && let Ok(config) = crate::pipeline::load_config(&path)
    {
        return config.tag.unwrap_or_default();
    }
    TagConfig::default()
}

fn find_previous_tag(cfg: &ResolvedConfig) -> Result<Option<String>> {
    let tags = match cfg.tag_context.as_str() {
        "branch" => git::get_branch_semver_tags(&cfg.tag_prefix)?,
        _ => git::get_all_semver_tags(&cfg.tag_prefix)?,
    };
    Ok(tags.into_iter().next())
}

fn branch_matches(branch: &str, patterns: &[String]) -> bool {
    for pattern in patterns {
        // Try exact match first
        if branch == pattern {
            return true;
        }
        // Try regex match (anchored to prevent partial matches)
        if let Ok(re) = Regex::new(&format!("^{}$", pattern))
            && re.is_match(branch)
        {
            return true;
        }
    }
    false
}

fn get_messages_for_bump(cfg: &ResolvedConfig, prev_tag: Option<&str>) -> Result<Vec<String>> {
    match cfg.branch_history.as_str() {
        "last" => git::get_last_commit_messages(1),
        "full" | "compare" => {
            if let Some(tag) = prev_tag {
                git::get_commit_messages_between(tag, "HEAD")
            } else {
                // No previous tag: get all commit messages
                git::get_last_commit_messages(500)
            }
        }
        other => {
            bail!("unknown branch_history mode: {}", other);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BumpKind {
    Major,
    Minor,
    Patch,
    None,
}

fn detect_bump(messages: &[String], cfg: &ResolvedConfig) -> BumpKind {
    detect_bump_from_tokens(
        messages,
        &cfg.major_string_token,
        &cfg.minor_string_token,
        &cfg.patch_string_token,
        &cfg.none_string_token,
        &cfg.default_bump,
    )
}

/// Core bump detection logic, separated for unit testing without needing the full config.
pub fn detect_bump_from_tokens(
    messages: &[String],
    major_token: &str,
    minor_token: &str,
    patch_token: &str,
    none_token: &str,
    default_bump: &str,
) -> BumpKind {
    let mut has_major = false;
    let mut has_minor = false;
    let mut has_patch = false;
    let mut has_none = false;

    for msg in messages {
        if msg.contains(none_token) {
            has_none = true;
        }
        if msg.contains(major_token) {
            has_major = true;
        }
        if msg.contains(minor_token) {
            has_minor = true;
        }
        if msg.contains(patch_token) {
            has_patch = true;
        }
    }

    // #none takes priority — skip tagging entirely
    if has_none {
        return BumpKind::None;
    }

    // Priority: major > minor > patch
    if has_major {
        return BumpKind::Major;
    }
    if has_minor {
        return BumpKind::Minor;
    }
    if has_patch {
        return BumpKind::Patch;
    }

    // Fall back to default_bump
    match default_bump {
        "major" => BumpKind::Major,
        "minor" => BumpKind::Minor,
        "patch" => BumpKind::Patch,
        "none" | "false" => BumpKind::None,
        _ => BumpKind::Minor,
    }
}

/// Apply a bump to semver components. Returns (major, minor, patch).
pub fn apply_bump(major: u64, minor: u64, patch: u64, bump: &BumpKind) -> (u64, u64, u64) {
    match bump {
        BumpKind::Major => (major + 1, 0, 0),
        BumpKind::Minor => (major, minor + 1, 0),
        BumpKind::Patch => (major, minor, patch + 1),
        BumpKind::None => (major, minor, patch),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Bump detection tests ----

    #[test]
    fn test_detect_bump_major_takes_precedence() {
        let messages = vec![
            "fix: something #patch".to_string(),
            "feat: big change #major".to_string(),
            "feat: small change #minor".to_string(),
        ];
        let result = detect_bump_from_tokens(
            &messages, "#major", "#minor", "#patch", "#none", "minor",
        );
        assert_eq!(result, BumpKind::Major);
    }

    #[test]
    fn test_detect_bump_minor_over_patch() {
        let messages = vec![
            "fix: something #patch".to_string(),
            "feat: new feature #minor".to_string(),
        ];
        let result = detect_bump_from_tokens(
            &messages, "#major", "#minor", "#patch", "#none", "patch",
        );
        assert_eq!(result, BumpKind::Minor);
    }

    #[test]
    fn test_detect_bump_patch_only() {
        let messages = vec!["fix: a bug #patch".to_string()];
        let result = detect_bump_from_tokens(
            &messages, "#major", "#minor", "#patch", "#none", "minor",
        );
        assert_eq!(result, BumpKind::Patch);
    }

    #[test]
    fn test_detect_bump_none_token() {
        let messages = vec![
            "chore: update deps #none".to_string(),
            "feat: something #major".to_string(),
        ];
        let result = detect_bump_from_tokens(
            &messages, "#major", "#minor", "#patch", "#none", "minor",
        );
        assert_eq!(result, BumpKind::None);
    }

    #[test]
    fn test_detect_bump_default_when_no_tokens() {
        let messages = vec!["fix: something".to_string(), "docs: update readme".to_string()];
        let result = detect_bump_from_tokens(
            &messages, "#major", "#minor", "#patch", "#none", "minor",
        );
        assert_eq!(result, BumpKind::Minor);
    }

    #[test]
    fn test_detect_bump_default_patch() {
        let messages = vec!["fix: something".to_string()];
        let result = detect_bump_from_tokens(
            &messages, "#major", "#minor", "#patch", "#none", "patch",
        );
        assert_eq!(result, BumpKind::Patch);
    }

    #[test]
    fn test_detect_bump_default_major() {
        let messages = vec!["fix: something".to_string()];
        let result = detect_bump_from_tokens(
            &messages, "#major", "#minor", "#patch", "#none", "major",
        );
        assert_eq!(result, BumpKind::Major);
    }

    #[test]
    fn test_detect_bump_default_none() {
        let messages = vec!["fix: something".to_string()];
        let result = detect_bump_from_tokens(
            &messages, "#major", "#minor", "#patch", "#none", "none",
        );
        assert_eq!(result, BumpKind::None);
    }

    #[test]
    fn test_detect_bump_empty_messages_uses_default() {
        let result = detect_bump_from_tokens(
            &[], "#major", "#minor", "#patch", "#none", "patch",
        );
        assert_eq!(result, BumpKind::Patch);
    }

    #[test]
    fn test_detect_bump_custom_tokens() {
        let messages = vec!["BREAKING CHANGE: rewrite".to_string()];
        let result = detect_bump_from_tokens(
            &messages,
            "BREAKING CHANGE",
            "feat:",
            "fix:",
            "skip:",
            "patch",
        );
        assert_eq!(result, BumpKind::Major);
    }

    // ---- Apply bump tests ----

    #[test]
    fn test_apply_bump_major() {
        assert_eq!(apply_bump(1, 2, 3, &BumpKind::Major), (2, 0, 0));
    }

    #[test]
    fn test_apply_bump_minor() {
        assert_eq!(apply_bump(1, 2, 3, &BumpKind::Minor), (1, 3, 0));
    }

    #[test]
    fn test_apply_bump_patch() {
        assert_eq!(apply_bump(1, 2, 3, &BumpKind::Patch), (1, 2, 4));
    }

    #[test]
    fn test_apply_bump_none() {
        assert_eq!(apply_bump(1, 2, 3, &BumpKind::None), (1, 2, 3));
    }

    #[test]
    fn test_apply_bump_from_zero() {
        assert_eq!(apply_bump(0, 0, 0, &BumpKind::Patch), (0, 0, 1));
        assert_eq!(apply_bump(0, 0, 0, &BumpKind::Minor), (0, 1, 0));
        assert_eq!(apply_bump(0, 0, 0, &BumpKind::Major), (1, 0, 0));
    }

    // ---- branch_matches tests ----

    #[test]
    fn test_branch_matches_exact() {
        assert!(branch_matches("main", &["main".to_string()]));
        assert!(branch_matches("master", &["master".to_string()]));
    }

    #[test]
    fn test_branch_matches_regex() {
        assert!(branch_matches(
            "release/1.0",
            &["release/.*".to_string()]
        ));
    }

    #[test]
    fn test_branch_no_match() {
        assert!(!branch_matches(
            "feature/foo",
            &["main".to_string(), "master".to_string()]
        ));
    }

    #[test]
    fn test_branch_matches_empty_patterns() {
        assert!(!branch_matches("main", &[]));
    }

    // ---- Prerelease suffix tests ----

    #[test]
    fn test_prerelease_suffix_application() {
        // Simulate the prerelease logic
        let version = "1.2.0";
        let suffix = "beta";
        let result = format!("{}-{}", version, suffix);
        assert_eq!(result, "1.2.0-beta");
    }

    #[test]
    fn test_prerelease_suffix_custom() {
        let version = "2.0.0";
        let suffix = "rc.1";
        let result = format!("{}-{}", version, suffix);
        assert_eq!(result, "2.0.0-rc.1");
    }

    // ---- Custom tag override tests ----

    #[test]
    fn test_custom_tag_with_prefix() {
        // If custom tag already has prefix, don't duplicate
        let custom = "v5.0.0";
        let prefix = "v";
        let tag = if custom.starts_with(prefix) {
            custom.to_string()
        } else {
            format!("{}{}", prefix, custom)
        };
        assert_eq!(tag, "v5.0.0");
    }

    #[test]
    fn test_custom_tag_without_prefix() {
        let custom = "5.0.0";
        let prefix = "v";
        let tag = if custom.starts_with(prefix) {
            custom.to_string()
        } else {
            format!("{}{}", prefix, custom)
        };
        assert_eq!(tag, "v5.0.0");
    }

    // ---- Config resolution tests ----

    #[test]
    fn test_resolved_config_defaults() {
        let cfg = TagConfig::default();
        let opts = TagOpts {
            dry_run: false,
            custom_tag: None,
            default_bump: None,
            crate_name: None,
            config_override: None,
            verbose: false,
        };
        let resolved = ResolvedConfig::from_tag_config(&cfg, &opts);
        assert_eq!(resolved.default_bump, "minor");
        assert_eq!(resolved.tag_prefix, "v");
        assert_eq!(resolved.tag_context, "repo");
        assert_eq!(resolved.branch_history, "compare");
        assert_eq!(resolved.initial_version, "0.0.0");
        assert!(!resolved.prerelease);
        assert_eq!(resolved.prerelease_suffix, "beta");
        assert!(!resolved.force_without_changes);
        assert!(!resolved.force_without_changes_pre);
        assert_eq!(resolved.major_string_token, "#major");
        assert_eq!(resolved.minor_string_token, "#minor");
        assert_eq!(resolved.patch_string_token, "#patch");
        assert_eq!(resolved.none_string_token, "#none");
        assert!(resolved.verbose); // default is true
    }

    #[test]
    fn test_resolved_config_cli_overrides() {
        let cfg = TagConfig {
            default_bump: Some("minor".to_string()),
            ..Default::default()
        };
        let opts = TagOpts {
            dry_run: false,
            custom_tag: Some("v9.9.9".to_string()),
            default_bump: Some("major".to_string()),
            crate_name: None,
            config_override: None,
            verbose: false,
        };
        let resolved = ResolvedConfig::from_tag_config(&cfg, &opts);
        assert_eq!(resolved.default_bump, "major");
        assert_eq!(resolved.custom_tag, Some("v9.9.9".to_string()));
    }

    #[test]
    fn test_resolved_config_full_config() {
        let cfg = TagConfig {
            default_bump: Some("patch".to_string()),
            tag_prefix: Some("release-v".to_string()),
            release_branches: Some(vec!["main".to_string(), "release/.*".to_string()]),
            custom_tag: None,
            tag_context: Some("branch".to_string()),
            branch_history: Some("last".to_string()),
            initial_version: Some("1.0.0".to_string()),
            prerelease: Some(true),
            prerelease_suffix: Some("alpha".to_string()),
            force_without_changes: Some(true),
            force_without_changes_pre: Some(true),
            major_string_token: Some("BREAKING".to_string()),
            minor_string_token: Some("feat:".to_string()),
            patch_string_token: Some("fix:".to_string()),
            none_string_token: Some("skip".to_string()),
            git_api_tagging: Some(false),
            verbose: Some(false),
        };
        let opts = TagOpts {
            dry_run: false,
            custom_tag: None,
            default_bump: None,
            crate_name: None,
            config_override: None,
            verbose: false,
        };
        let resolved = ResolvedConfig::from_tag_config(&cfg, &opts);
        assert_eq!(resolved.default_bump, "patch");
        assert_eq!(resolved.tag_prefix, "release-v");
        assert_eq!(resolved.release_branches.len(), 2);
        assert_eq!(resolved.tag_context, "branch");
        assert_eq!(resolved.branch_history, "last");
        assert_eq!(resolved.initial_version, "1.0.0");
        assert!(resolved.prerelease);
        assert_eq!(resolved.prerelease_suffix, "alpha");
        assert!(resolved.force_without_changes);
        assert!(resolved.force_without_changes_pre);
        assert_eq!(resolved.major_string_token, "BREAKING");
        assert_eq!(resolved.minor_string_token, "feat:");
        assert_eq!(resolved.patch_string_token, "fix:");
        assert_eq!(resolved.none_string_token, "skip");
        assert!(!resolved.verbose);
    }

    // ---- Config parsing from YAML tests ----

    #[test]
    fn test_tag_config_from_yaml_full() {
        let yaml = r##"
default_bump: patch
tag_prefix: "v"
release_branches:
  - main
  - "release/.*"
tag_context: branch
branch_history: last
initial_version: "1.0.0"
prerelease: true
prerelease_suffix: rc
force_without_changes: true
force_without_changes_pre: false
major_string_token: "#major"
minor_string_token: "#minor"
patch_string_token: "#patch"
none_string_token: "#none"
git_api_tagging: true
verbose: false
"##;
        let cfg: TagConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.default_bump, Some("patch".to_string()));
        assert_eq!(cfg.tag_prefix, Some("v".to_string()));
        assert_eq!(
            cfg.release_branches,
            Some(vec!["main".to_string(), "release/.*".to_string()])
        );
        assert_eq!(cfg.tag_context, Some("branch".to_string()));
        assert_eq!(cfg.branch_history, Some("last".to_string()));
        assert_eq!(cfg.initial_version, Some("1.0.0".to_string()));
        assert_eq!(cfg.prerelease, Some(true));
        assert_eq!(cfg.prerelease_suffix, Some("rc".to_string()));
        assert_eq!(cfg.force_without_changes, Some(true));
        assert_eq!(cfg.force_without_changes_pre, Some(false));
        assert_eq!(cfg.git_api_tagging, Some(true));
        assert_eq!(cfg.verbose, Some(false));
    }

    #[test]
    fn test_tag_config_from_yaml_minimal() {
        let yaml = "{}";
        let cfg: TagConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.default_bump, None);
        assert_eq!(cfg.tag_prefix, None);
        assert_eq!(cfg.release_branches, None);
    }

    #[test]
    fn test_tag_config_from_yaml_defaults() {
        let yaml = "default_bump: major";
        let cfg: TagConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.default_bump, Some("major".to_string()));
        assert_eq!(cfg.tag_prefix, None); // not set, will use default when resolved
    }

    #[test]
    fn test_top_level_config_with_tag_section() {
        let yaml = r#"
project_name: myproject
crates:
  - name: myproject
    path: "."
    tag_template: "v{{ .Version }}"
tag:
  default_bump: patch
  tag_prefix: "v"
  branch_history: last
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let tag = config.tag.unwrap();
        assert_eq!(tag.default_bump, Some("patch".to_string()));
        assert_eq!(tag.branch_history, Some("last".to_string()));
    }

    // ---- Integration-style bump logic tests ----

    #[test]
    fn test_full_bump_flow_major() {
        let messages = vec!["feat: breaking change #major".to_string()];
        let bump = detect_bump_from_tokens(
            &messages, "#major", "#minor", "#patch", "#none", "patch",
        );
        assert_eq!(bump, BumpKind::Major);
        let (maj, min, pat) = apply_bump(1, 5, 3, &bump);
        assert_eq!((maj, min, pat), (2, 0, 0));
        let new_tag = format!("v{}.{}.{}", maj, min, pat);
        assert_eq!(new_tag, "v2.0.0");
    }

    #[test]
    fn test_full_bump_flow_minor_default() {
        let messages = vec!["docs: update readme".to_string()];
        let bump = detect_bump_from_tokens(
            &messages, "#major", "#minor", "#patch", "#none", "minor",
        );
        assert_eq!(bump, BumpKind::Minor);
        let (maj, min, pat) = apply_bump(1, 2, 3, &bump);
        assert_eq!((maj, min, pat), (1, 3, 0));
    }

    #[test]
    fn test_full_bump_flow_prerelease() {
        let messages = vec!["feat: new thing #minor".to_string()];
        let bump = detect_bump_from_tokens(
            &messages, "#major", "#minor", "#patch", "#none", "patch",
        );
        assert_eq!(bump, BumpKind::Minor);
        let (maj, min, pat) = apply_bump(1, 2, 3, &bump);
        let version = format!("{}.{}.{}-beta", maj, min, pat);
        assert_eq!(version, "1.3.0-beta");
    }
}
