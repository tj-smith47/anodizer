use super::remote::{parse_github_remote, parse_remote_owner_repo, parse_remote_web_base};
use super::semver::{compare_prerelease, parse_semver, parse_semver_tag};
use super::tags::{
    create_tag_local_only, filter_ignored_tags, find_latest_tag_matching,
    find_latest_tag_matching_with_prefix, find_previous_tag, find_previous_tag_with_prefix,
    get_all_semver_tags, is_nightly_tag, list_remote_tag_names_in, strip_monorepo_prefix,
    tag_is_signed,
};
use crate::redact::redact_url_credentials;
use crate::test_helpers::CwdGuard;

#[test]
fn test_parse_semver() {
    let v = parse_semver("v1.2.3").unwrap();
    assert_eq!(v.major, 1);
    assert_eq!(v.minor, 2);
    assert_eq!(v.patch, 3);
    assert_eq!(v.prerelease, None);
    assert_eq!(v.build_metadata, None);
}

#[test]
fn test_parse_semver_prerelease() {
    let v = parse_semver("v1.0.0-rc.1").unwrap();
    assert_eq!(v.major, 1);
    assert_eq!(v.prerelease, Some("rc.1".to_string()));
    assert_eq!(v.build_metadata, None);
}

#[test]
fn test_parse_semver_build_metadata() {
    let v = parse_semver("v1.0.0+build.42").unwrap();
    assert_eq!(v.major, 1);
    assert_eq!(v.minor, 0);
    assert_eq!(v.patch, 0);
    assert_eq!(v.prerelease, None);
    assert_eq!(v.build_metadata, Some("build.42".to_string()));
}

#[test]
fn test_parse_semver_prerelease_and_build_metadata() {
    let v = parse_semver("v1.0.0-rc.1+build.42").unwrap();
    assert_eq!(v.major, 1);
    assert_eq!(v.prerelease, Some("rc.1".to_string()));
    assert_eq!(v.build_metadata, Some("build.42".to_string()));
}

#[test]
fn test_parse_semver_rejects_prefix() {
    // Strict parse_semver rejects prefixed tags (use parse_semver_tag instead)
    assert!(parse_semver("cfgd-core-v2.1.0").is_err());
    assert!(parse_semver("release-notes-v1.2.3").is_err());
}

#[test]
fn test_parse_semver_tag_with_prefix() {
    let v = parse_semver_tag("cfgd-core-v2.1.0").unwrap();
    assert_eq!(v.major, 2);
    assert_eq!(v.minor, 1);
    assert_eq!(v.patch, 0);
}

#[test]
fn test_parse_semver_tag_plain() {
    // parse_semver_tag also handles plain versions
    let v = parse_semver_tag("v1.2.3").unwrap();
    assert_eq!(v.major, 1);
    assert_eq!(v.minor, 2);
    assert_eq!(v.patch, 3);
}

#[test]
fn test_parse_semver_tag_with_prerelease_prefix() {
    let v = parse_semver_tag("my-project-v1.0.0-rc.1").unwrap();
    assert_eq!(v.major, 1);
    assert_eq!(v.prerelease, Some("rc.1".to_string()));
}

#[test]
fn test_version_from_tag_extracts_across_tag_families() {
    use super::semver::version_from_tag;
    assert_eq!(version_from_tag("v1.2.3"), Some("1.2.3".to_string()));
    assert_eq!(version_from_tag("1.2.3"), Some("1.2.3".to_string()));
    assert_eq!(version_from_tag("crd-v0.5.0"), Some("0.5.0".to_string()));
    assert_eq!(
        version_from_tag("sub/v1.2.3-rc.1"),
        Some("1.2.3-rc.1".to_string())
    );
    assert_eq!(version_from_tag("core-v2.0.1"), Some("2.0.1".to_string()));
    assert_eq!(
        version_from_tag("v0.4.0-beta.1"),
        Some("0.4.0-beta.1".to_string())
    );
    assert_eq!(version_from_tag(""), None);
    assert_eq!(version_from_tag("nightly"), None);
    assert_eq!(version_from_tag("not-a-version"), None);
}

#[test]
fn test_split_tag_family_prefix_and_version() {
    use super::semver::split_tag_family;
    let (prefix, sv) = split_tag_family("v1.2.3").unwrap();
    assert_eq!(prefix, "v");
    assert_eq!(sv.version_string(), "1.2.3");
    let (prefix, sv) = split_tag_family("crd-v0.5.0").unwrap();
    assert_eq!(prefix, "crd-v");
    assert_eq!(sv.version_string(), "0.5.0");
    let (prefix, sv) = split_tag_family("sub/v1.2.3-rc.1").unwrap();
    assert_eq!(prefix, "sub/v");
    assert_eq!(sv.version_string(), "1.2.3-rc.1");
    let (prefix, _) = split_tag_family("1.2.3").unwrap();
    assert_eq!(prefix, "");
    assert!(split_tag_family("").is_none());
    assert!(split_tag_family("nightly").is_none());
}

#[test]
fn test_is_prerelease() {
    assert!(parse_semver("v1.0.0-rc.1").unwrap().is_prerelease());
    assert!(!parse_semver("v1.0.0").unwrap().is_prerelease());
    // Build metadata only is NOT a prerelease
    assert!(!parse_semver("v1.0.0+build.42").unwrap().is_prerelease());
}

#[test]
fn test_parse_github_remote_https() {
    let result = parse_github_remote("https://github.com/tj-smith47/anodizer.git");
    assert_eq!(
        result,
        Some(("tj-smith47".to_string(), "anodizer".to_string()))
    );
}

#[test]
fn test_parse_github_remote_https_no_dotgit() {
    let result = parse_github_remote("https://github.com/owner/repo");
    assert_eq!(result, Some(("owner".to_string(), "repo".to_string())));
}

#[test]
fn test_parse_github_remote_ssh() {
    let result = parse_github_remote("git@github.com:owner/repo.git");
    assert_eq!(result, Some(("owner".to_string(), "repo".to_string())));
}

#[test]
fn test_parse_github_remote_ssh_no_dotgit() {
    let result = parse_github_remote("git@github.com:owner/repo");
    assert_eq!(result, Some(("owner".to_string(), "repo".to_string())));
}

#[test]
fn test_parse_github_remote_invalid() {
    let result = parse_github_remote("https://gitlab.com/foo/bar.git");
    assert_eq!(result, None);
}

#[test]
fn test_parse_github_remote_empty() {
    let result = parse_github_remote("");
    assert_eq!(result, None);
}

// -- parse_remote_owner_repo (generic) -----------------------------------

#[test]
fn test_parse_remote_github_https() {
    let result = parse_remote_owner_repo("https://github.com/owner/repo.git");
    assert_eq!(result, Some(("owner".to_string(), "repo".to_string())));
}

#[test]
fn test_parse_remote_gitlab_https() {
    let result = parse_remote_owner_repo("https://gitlab.com/owner/repo.git");
    assert_eq!(result, Some(("owner".to_string(), "repo".to_string())));
}

#[test]
fn test_parse_remote_gitea_https() {
    let result = parse_remote_owner_repo("https://gitea.example.com/myorg/myapp.git");
    assert_eq!(result, Some(("myorg".to_string(), "myapp".to_string())));
}

#[test]
fn test_parse_remote_gitlab_nested_group() {
    let result = parse_remote_owner_repo("https://gitlab.com/group/subgroup/repo.git");
    assert_eq!(
        result,
        Some(("group/subgroup".to_string(), "repo".to_string()))
    );
}

#[test]
fn test_parse_remote_ssh_gitlab() {
    let result = parse_remote_owner_repo("git@gitlab.com:owner/repo.git");
    assert_eq!(result, Some(("owner".to_string(), "repo".to_string())));
}

#[test]
fn test_parse_remote_ssh_gitea() {
    let result = parse_remote_owner_repo("git@gitea.example.com:org/app.git");
    assert_eq!(result, Some(("org".to_string(), "app".to_string())));
}

#[test]
fn test_parse_remote_ssh_nested_group() {
    let result = parse_remote_owner_repo("git@gitlab.com:group/subgroup/repo.git");
    assert_eq!(
        result,
        Some(("group/subgroup".to_string(), "repo".to_string()))
    );
}

#[test]
fn test_parse_remote_no_dotgit() {
    let result = parse_remote_owner_repo("https://gitlab.com/owner/repo");
    assert_eq!(result, Some(("owner".to_string(), "repo".to_string())));
}

#[test]
fn test_parse_remote_empty() {
    assert_eq!(parse_remote_owner_repo(""), None);
}

#[test]
fn test_parse_remote_http() {
    let result = parse_remote_owner_repo("http://gitlab.local/team/project.git");
    assert_eq!(result, Some(("team".to_string(), "project".to_string())));
}

// -- parse_remote_web_base (host-preserving) ------------------------------

#[test]
fn test_web_base_github_https() {
    assert_eq!(
        parse_remote_web_base("https://github.com/owner/repo.git").as_deref(),
        Some("https://github.com/owner/repo")
    );
}

#[test]
fn test_web_base_github_ssh() {
    assert_eq!(
        parse_remote_web_base("git@github.com:owner/repo.git").as_deref(),
        Some("https://github.com/owner/repo")
    );
}

#[test]
fn test_web_base_gitlab_ssh_self_hosted() {
    assert_eq!(
        parse_remote_web_base("git@gitlab.example.com:team/widget.git").as_deref(),
        Some("https://gitlab.example.com/team/widget")
    );
}

#[test]
fn test_web_base_gitea_https_nested_group() {
    assert_eq!(
        parse_remote_web_base("https://gitea.example.com/group/subgroup/repo.git").as_deref(),
        Some("https://gitea.example.com/group/subgroup/repo")
    );
}

#[test]
fn test_web_base_http_normalized_to_https() {
    assert_eq!(
        parse_remote_web_base("http://gitlab.local/team/project.git").as_deref(),
        Some("https://gitlab.local/team/project")
    );
}

#[test]
fn test_web_base_https_strips_userinfo() {
    assert_eq!(
        parse_remote_web_base("https://user:token@gitlab.com/owner/repo.git").as_deref(),
        Some("https://gitlab.com/owner/repo")
    );
}

#[test]
fn test_web_base_empty() {
    assert_eq!(parse_remote_web_base(""), None);
}

#[test]
fn test_strip_url_credentials_with_userinfo() {
    // `redact_url_credentials` keeps the `@` boundary and inserts the
    // `<redacted>` placeholder so the output signals there was userinfo.
    assert_eq!(
        redact_url_credentials("https://user:token@github.com/owner/repo.git"),
        "https://<redacted>@github.com/owner/repo.git"
    );
}

#[test]
fn test_strip_url_credentials_no_userinfo() {
    assert_eq!(
        redact_url_credentials("https://github.com/owner/repo.git"),
        "https://github.com/owner/repo.git"
    );
}

#[test]
fn test_strip_url_credentials_ssh_unchanged() {
    // SSH-style `git@github.com:owner/repo.git` has no `://`, so the
    // helper leaves it alone.
    assert_eq!(
        redact_url_credentials("git@github.com:owner/repo.git"),
        "git@github.com:owner/repo.git"
    );
}

#[test]
fn test_strip_url_credentials_user_only() {
    assert_eq!(
        redact_url_credentials("https://user@github.com/owner/repo.git"),
        "https://<redacted>@github.com/owner/repo.git"
    );
}

#[test]
fn test_strip_url_credentials_token_with_at_sign_does_not_leak() {
    // A token literal containing `@` (which the previous `find('@')` would
    // have split early on) must be fully consumed by the userinfo redaction
    // — `rfind('@')` locks onto the host-boundary `@`.
    let leaky = "https://user:t@k@n@github.com/owner/repo.git";
    let scrubbed = redact_url_credentials(leaky);
    assert!(!scrubbed.contains("t@k@n"));
    assert_eq!(scrubbed, "https://<redacted>@github.com/owner/repo.git");
}

#[test]
fn test_compare_prerelease_numeric() {
    // rc.9 < rc.10 (numeric comparison, not lexicographic)
    assert_eq!(
        compare_prerelease("rc.9", "rc.10"),
        std::cmp::Ordering::Less
    );
    assert_eq!(
        compare_prerelease("rc.10", "rc.9"),
        std::cmp::Ordering::Greater
    );
}

#[test]
fn test_compare_prerelease_numeric_less_than_alpha() {
    // Numeric identifiers always have lower precedence than alphanumeric
    assert_eq!(compare_prerelease("1", "alpha"), std::cmp::Ordering::Less);
    assert_eq!(
        compare_prerelease("alpha", "1"),
        std::cmp::Ordering::Greater
    );
}

#[test]
fn test_compare_prerelease_alpha_lexicographic() {
    assert_eq!(
        compare_prerelease("alpha", "beta"),
        std::cmp::Ordering::Less
    );
}

#[test]
fn test_compare_prerelease_shorter_lower_precedence() {
    // alpha < alpha.1 (shorter set = lower precedence)
    assert_eq!(
        compare_prerelease("alpha", "alpha.1"),
        std::cmp::Ordering::Less
    );
}

#[test]
fn test_compare_prerelease_equal() {
    assert_eq!(
        compare_prerelease("rc.1", "rc.1"),
        std::cmp::Ordering::Equal
    );
}

#[test]
fn test_semver_ord_prerelease_less_than_release() {
    let pre = parse_semver("v1.0.0-rc.1").unwrap();
    let rel = parse_semver("v1.0.0").unwrap();
    assert!(pre < rel);
}

#[test]
fn test_semver_ord_prerelease_numeric_sorting() {
    // v1.0.0-rc.9 < v1.0.0-rc.10 (SemVer 2.0.0 compliant)
    let rc9 = parse_semver("v1.0.0-rc.9").unwrap();
    let rc10 = parse_semver("v1.0.0-rc.10").unwrap();
    assert!(rc9 < rc10);
}

#[test]
fn test_semver_build_metadata_ignored_in_ord_and_eq() {
    // SemVer 2.0.0 section 10: build metadata MUST be ignored when
    // determining version precedence. Two versions differing only in build
    // metadata are equal under both Ord and PartialEq, even though the raw
    // string survives the round-trip via `build_metadata`.
    let a = parse_semver("v1.2.3+abc").unwrap();
    let b = parse_semver("v1.2.3+def").unwrap();

    assert_eq!(a.build_metadata.as_deref(), Some("abc"));
    assert_eq!(b.build_metadata.as_deref(), Some("def"));
    assert_eq!(a.cmp(&b), std::cmp::Ordering::Equal);
    assert_eq!(a, b);
    assert_eq!(b.cmp(&a), std::cmp::Ordering::Equal);

    // Same for build metadata vs. no metadata at all.
    let plain = parse_semver("v1.2.3").unwrap();
    assert_eq!(plain.cmp(&a), std::cmp::Ordering::Equal);
    assert_eq!(plain, a);

    // Build metadata on a prerelease — still ignored.
    let pre_a = parse_semver("v1.0.0-rc.1+build.42").unwrap();
    let pre_b = parse_semver("v1.0.0-rc.1+build.99").unwrap();
    assert_eq!(pre_a.cmp(&pre_b), std::cmp::Ordering::Equal);
    assert_eq!(pre_a, pre_b);
}

// -----------------------------------------------------------------------
// find_latest_tag_matching + GitConfig integration tests
//
// Each test creates a fresh temporary git repository with tags, then
// verifies that GitConfig fields (ignore_tags, ignore_tag_prefixes, etc.)
// are respected.
// -----------------------------------------------------------------------

use serial_test::serial;

/// Create a bare-bones git repo in `dir` with an initial commit and the
/// given list of lightweight tags.
fn init_repo_with_tags(dir: &std::path::Path, tags: &[&str]) {
    use std::process::Command;

    let run = |args: &[&str]| {
        let out = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args)
                    .current_dir(dir)
                    .env("GIT_AUTHOR_NAME", "test")
                    .env("GIT_AUTHOR_EMAIL", "test@test.com")
                    .env("GIT_COMMITTER_NAME", "test")
                    .env("GIT_COMMITTER_EMAIL", "test@test.com");
                cmd
            },
            "git",
        );
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };

    run(&["init"]);
    run(&["config", "user.email", "test@test.com"]);
    run(&["config", "user.name", "test"]);
    std::fs::write(dir.join("README"), "init").unwrap();
    run(&["add", "."]);
    run(&["commit", "-m", "initial"]);

    for tag in tags {
        run(&["tag", tag]);
    }
}

#[test]
fn is_nightly_tag_matches_minted_shapes_only() {
    // anodizer's own minted shapes (default + per-crate prefix + nushell-style).
    for t in [
        "nightly",
        "v0.5.1-9a0d7ed0-nightly",
        "operator-v0.5.1-9a0d7ed0-nightly",
        "csi-v0.5.1-9a0d7ed0-nightly",
        "v1.2.3-nightly.4+abc1234",
        "app-v2.0.0-nightly",
    ] {
        assert!(is_nightly_tag(t), "{t} must be nightly-shaped");
    }
    // Real release tags — including a crate genuinely named `nightly-*` —
    // must never be swallowed.
    for t in [
        "v1.2.3",
        "core-v0.5.0",
        "v0.2.0-beta.3",
        "nightly-tools-v1.0.0",
        "v1.0.0-rc.1",
        "vnightlyish-1.0.0",
    ] {
        assert!(!is_nightly_tag(t), "{t} must NOT be nightly-shaped");
    }
}

#[test]
fn filter_ignored_tags_applies_config_and_nightly_exclusion() {
    let tags: Vec<String> = [
        "v1.0.0",
        "operator-v0.5.1-9a0d7ed0-nightly",
        "withdrawn-v0.9.9",
        "rc-v2.0.0",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let gc = crate::config::GitConfig {
        ignore_tags: Some(vec!["withdrawn-*".to_string()]),
        ignore_tag_prefixes: Some(vec!["rc-".to_string()]),
        ..Default::default()
    };
    assert_eq!(
        filter_ignored_tags(&tags, Some(&gc), None),
        vec!["v1.0.0".to_string()]
    );
    // Nightly exclusion holds with NO git config at all — the stranded-tag
    // poisoning must not depend on the consumer's ignore_tags being right.
    assert_eq!(
        filter_ignored_tags(&tags[..2], None, None),
        vec!["v1.0.0".to_string()]
    );
}

#[test]
#[serial]
fn find_latest_tag_skips_stranded_nightly_tags_unconditionally() {
    // A failed nightly strands `<prefix>v<higher-semver>-<sha>-nightly`;
    // latest-tag resolution must keep returning the real latest stable tag
    // even with NO ignore_tags config (cfgd nightly 2026-07-21 incident:
    // the stranded csi tag outranked the real latest 0.5.0).
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &["v0.5.0", "v0.5.1-9a0d7ed0-nightly", "nightly"]);

    let _cwd = CwdGuard::new(dir).unwrap();

    let result = find_latest_tag_matching("v{{ .Version }}", None, None).unwrap();
    assert_eq!(result, Some("v0.5.0".to_string()));
}

#[test]
#[serial]
fn test_find_latest_tag_none_config_unchanged_behavior() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &["v1.0.0", "v1.1.0", "v2.0.0"]);

    // Change to the temp repo so git commands work.
    let _cwd = CwdGuard::new(dir).unwrap();

    let result = find_latest_tag_matching("v{{ .Version }}", None, None).unwrap();
    assert_eq!(result, Some("v2.0.0".to_string()));
}

#[test]
#[serial]
fn test_get_all_semver_tags_ignore_tags() {
    // The tag subcommand's find_previous_tag calls through to
    // get_all_semver_tags; its ignore_tags wiring must exclude matching
    // tags so an autotag pass doesn't regress onto a deliberately-ignored
    // tag (e.g. a withdrawn release).
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &["v1.0.0", "v2.0.0", "v3.0.0"]);

    let _cwd = CwdGuard::new(dir).unwrap();

    let gc = crate::config::GitConfig {
        ignore_tags: Some(vec!["v3.0.0".to_string()]),
        ..Default::default()
    };
    let tags = get_all_semver_tags("v", Some(&gc), None).unwrap();
    assert_eq!(tags, vec!["v2.0.0".to_string(), "v1.0.0".to_string()]);
}

#[test]
#[serial]
fn test_get_all_semver_tags_ignore_tag_prefixes() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &["v1.0.0", "v2.0.0", "nightly-v3.0.0"]);

    let _cwd = CwdGuard::new(dir).unwrap();

    let gc = crate::config::GitConfig {
        ignore_tag_prefixes: Some(vec!["nightly-".to_string()]),
        ..Default::default()
    };
    let tags = get_all_semver_tags("", Some(&gc), None).unwrap();
    // "nightly-v3.0.0" is excluded by prefix; only v2, v1 survive, ordered desc.
    assert_eq!(tags, vec!["v2.0.0".to_string(), "v1.0.0".to_string()]);
}

/// `find_latest_tag_matching` matches `ignore_tag_prefixes` WITHOUT skipping
/// empty entries (`skip_empty_ignore_prefix = false`): an empty prefix
/// matches every tag (`starts_with("")`), so all candidates are ignored and
/// the lookup yields `None`. Locks the subtler half of the dedup divergence.
#[test]
#[serial]
fn test_find_latest_tag_empty_ignore_prefix_excludes_all() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &["v1.0.0", "v2.0.0", "v3.0.0"]);

    let _cwd = CwdGuard::new(dir).unwrap();

    let gc = crate::config::GitConfig {
        ignore_tag_prefixes: Some(vec![String::new()]),
        ..Default::default()
    };
    let result = find_latest_tag_matching("v{{ .Version }}", Some(&gc), None).unwrap();
    assert_eq!(
        result, None,
        "empty ignore_tag_prefixes must exclude every tag in find_latest"
    );
}

/// The smartsemver previous-tag path matches `ignore_tag_prefixes` WITH the
/// empty-prefix skip (`skip_empty_ignore_prefix = true`): an empty entry is
/// ignored, so candidates are retained and the previous tag is returned.
/// The opposite polarity from `find_latest` above — flipping the flag in the
/// shared helper would break exactly one of this pair.
#[test]
#[serial]
fn test_smartsemver_empty_ignore_prefix_is_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &["v1.0.0", "v2.0.0", "v3.0.0"]);

    let _cwd = CwdGuard::new(dir).unwrap();

    let gc = crate::config::GitConfig {
        tag_sort: Some("smartsemver".to_string()),
        ignore_tag_prefixes: Some(vec![String::new()]),
        ..Default::default()
    };
    // Releasing v3.0.0 → previous is v2.0.0; the empty prefix is skipped, so
    // it does NOT swallow the candidate list.
    let result = find_previous_tag_with_prefix("v3.0.0", Some(&gc), None, None).unwrap();
    assert_eq!(
        result,
        Some("v2.0.0".to_string()),
        "empty ignore_tag_prefixes must be skipped on the smartsemver path"
    );
}

#[test]
#[serial]
fn test_get_all_semver_tags_no_config_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &["v1.0.0", "v2.0.0"]);

    let _cwd = CwdGuard::new(dir).unwrap();

    let tags = get_all_semver_tags("v", None, None).unwrap();
    assert_eq!(tags, vec!["v2.0.0".to_string(), "v1.0.0".to_string()]);
}

#[test]
#[serial]
fn test_find_latest_tag_ignore_tags_exact_match() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &["v1.0.0", "v2.0.0", "v3.0.0"]);

    let _cwd = CwdGuard::new(dir).unwrap();

    let gc = crate::config::GitConfig {
        ignore_tags: Some(vec!["v3.0.0".to_string()]),
        ..Default::default()
    };
    let result = find_latest_tag_matching("v{{ .Version }}", Some(&gc), None).unwrap();
    assert_eq!(result, Some("v2.0.0".to_string()));
}

#[test]
#[serial]
fn test_find_latest_tag_ignore_tags_multiple() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &["v1.0.0", "v2.0.0", "v3.0.0"]);

    let _cwd = CwdGuard::new(dir).unwrap();

    let gc = crate::config::GitConfig {
        ignore_tags: Some(vec!["v3.0.0".to_string(), "v2.0.0".to_string()]),
        ..Default::default()
    };
    let result = find_latest_tag_matching("v{{ .Version }}", Some(&gc), None).unwrap();
    assert_eq!(result, Some("v1.0.0".to_string()));
}

#[test]
#[serial]
fn test_find_latest_tag_ignore_tag_prefixes() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(
        dir,
        &["v1.0.0", "v2.0.0", "nightly-v3.0.0", "nightly-v4.0.0"],
    );

    let _cwd = CwdGuard::new(dir).unwrap();

    // Without prefix filtering, the template "v{{ .Version }}" won't match
    // nightly-v* tags anyway (regex mismatch). So test with a broader template
    // or with nightly-prefixed tags that do match a nightly template.
    // Let's test: filter out "nightly-" prefix from "nightly-v{{ .Version }}"
    let gc = crate::config::GitConfig {
        ignore_tag_prefixes: Some(vec!["nightly-".to_string()]),
        ..Default::default()
    };
    // The "v{{ .Version }}" template only matches v1.0.0, v2.0.0.
    // Without filtering, nightly tags don't match anyway, so latest = v2.0.0.
    let result = find_latest_tag_matching("v{{ .Version }}", Some(&gc), None).unwrap();
    assert_eq!(result, Some("v2.0.0".to_string()));

    // Now test with a template that would match nightly tags too:
    // Use a nightly template. Without ignore_tag_prefixes, nightly-v4.0.0 wins.
    let result_nightly = find_latest_tag_matching("nightly-v{{ .Version }}", None, None).unwrap();
    assert_eq!(result_nightly, Some("nightly-v4.0.0".to_string()));

    // With ignore_tag_prefixes filtering out "nightly-", all nightly tags are excluded.
    let result_filtered =
        find_latest_tag_matching("nightly-v{{ .Version }}", Some(&gc), None).unwrap();
    assert_eq!(result_filtered, None);
}

#[test]
#[serial]
fn test_find_latest_tag_ignore_all_returns_none() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &["v1.0.0", "v2.0.0"]);

    let _cwd = CwdGuard::new(dir).unwrap();

    let gc = crate::config::GitConfig {
        ignore_tags: Some(vec!["v1.0.0".to_string(), "v2.0.0".to_string()]),
        ..Default::default()
    };
    let result = find_latest_tag_matching("v{{ .Version }}", Some(&gc), None).unwrap();
    assert_eq!(result, None);
}

#[test]
#[serial]
fn test_find_latest_tag_ignore_tags_and_prefixes_combined() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &["v1.0.0", "v2.0.0", "v3.0.0-beta.1"]);

    let _cwd = CwdGuard::new(dir).unwrap();

    // ignore v2.0.0 by exact match, and anything starting with "v3" by prefix
    let gc = crate::config::GitConfig {
        ignore_tags: Some(vec!["v2.0.0".to_string()]),
        ignore_tag_prefixes: Some(vec!["v3".to_string()]),
        ..Default::default()
    };
    let result = find_latest_tag_matching("v{{ .Version }}", Some(&gc), None).unwrap();
    assert_eq!(result, Some("v1.0.0".to_string()));
}

#[test]
#[serial]
fn test_find_latest_tag_with_prefixed_template() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(
        dir,
        &[
            "myapp-v1.0.0",
            "myapp-v2.0.0",
            "myapp-v3.0.0",
            "other-v9.0.0",
        ],
    );

    let _cwd = CwdGuard::new(dir).unwrap();

    // Ignore myapp-v3.0.0 specifically
    let gc = crate::config::GitConfig {
        ignore_tags: Some(vec!["myapp-v3.0.0".to_string()]),
        ..Default::default()
    };
    let result = find_latest_tag_matching("myapp-v{{ .Version }}", Some(&gc), None).unwrap();
    assert_eq!(result, Some("myapp-v2.0.0".to_string()));
}

#[test]
#[serial]
fn test_find_latest_tag_default_git_config_same_as_none() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &["v1.0.0", "v1.1.0", "v2.0.0"]);

    let _cwd = CwdGuard::new(dir).unwrap();

    // Default GitConfig has all fields None — should behave identically to None
    let gc = crate::config::GitConfig::default();
    let with_default = find_latest_tag_matching("v{{ .Version }}", Some(&gc), None).unwrap();
    let with_none = find_latest_tag_matching("v{{ .Version }}", None, None).unwrap();
    assert_eq!(with_default, with_none);
    assert_eq!(with_default, Some("v2.0.0".to_string()));
}

#[test]
#[serial]
fn test_find_latest_tag_prerelease_suffix_with_default_sort() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // Create tags: two releases and a prerelease with -rc suffix.
    // v1.1.1-rc.1 is semantically version 1.1.1 with a prerelease,
    // which is > 1.1.0 in both SemVer and git version sort.
    // versionsort.suffix only affects ordering relative to the same
    // base version (e.g. v1.1.1-rc.1 vs v1.1.1), not across different
    // patch levels.
    init_repo_with_tags(dir, &["v1.0.0", "v1.1.0", "v1.1.1-rc.1"]);

    let _cwd = CwdGuard::new(dir).unwrap();

    // Without prerelease_suffix, using Rust-side SemVer sort:
    // v1.1.1-rc.1 is a prerelease of v1.1.1, which is > v1.1.0 but
    // SemVer says prereleases are < the release, so 1.1.1-rc.1 < 1.1.1.
    // But 1.1.1-rc.1 > 1.1.0 (different patch version), so it wins.
    let result_no_suffix = find_latest_tag_matching("v{{ .Version }}", None, None).unwrap();
    assert_eq!(
        result_no_suffix,
        Some("v1.1.1-rc.1".to_string()),
        "without prerelease_suffix, SemVer sort puts v1.1.1-rc.1 highest"
    );

    // With prerelease_suffix="-rc", git-delegated sort is activated
    // (use_git_sort=true). versionsort.suffix=-rc makes -rc tags sort
    // after their base version (so v1.1.1-rc.1 comes after v1.1.1),
    // but v1.1.1-rc.1 is still version 1.1.1 which is > 1.1.0.
    // Since we take the first (highest) from git's descending sort,
    // v1.1.1-rc.1 remains the latest.
    let gc = crate::config::GitConfig {
        prerelease_suffix: Some("-rc".to_string()),
        ..Default::default()
    };
    let result = find_latest_tag_matching("v{{ .Version }}", Some(&gc), None).unwrap();
    assert_eq!(
        result,
        Some("v1.1.1-rc.1".to_string()),
        "prerelease_suffix activates git-delegated sort; v1.1.1-rc.1 still highest"
    );

    // Now test the scenario where versionsort.suffix actually matters:
    // when the release version exists alongside the prerelease.
    // Add v1.1.1 — without suffix, git sorts rc before release (v1.1.1-rc.1 < v1.1.1);
    // with suffix, rc sorts *after* release but --sort=-version:refname
    // means descending, so release comes first.
    let run = |args: &[&str]| {
        let out = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = std::process::Command::new("git");
                cmd.args(args)
                    .current_dir(dir)
                    .env("GIT_AUTHOR_NAME", "test")
                    .env("GIT_AUTHOR_EMAIL", "test@test.com")
                    .env("GIT_COMMITTER_NAME", "test")
                    .env("GIT_COMMITTER_EMAIL", "test@test.com");
                cmd
            },
            "git",
        );
        assert!(out.status.success());
    };
    run(&["tag", "v1.1.1"]);

    // With versionsort.suffix=-rc and both v1.1.1 and v1.1.1-rc.1 present,
    // the suffix causes -rc.1 to sort after v1.1.1 in ascending order,
    // meaning v1.1.1-rc.1 comes last. In descending sort (-version:refname),
    // v1.1.1-rc.1 would be first. But the key point is that git-delegated
    // sort IS being used (prerelease_suffix triggers it).
    let result_both = find_latest_tag_matching("v{{ .Version }}", Some(&gc), None).unwrap();
    assert!(
        result_both.is_some(),
        "should find a tag with both release and rc present"
    );
}

#[test]
#[serial]
fn test_find_latest_tag_ignore_tags_template_rendered() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &["v1.0.0", "v2.0.0", "v3.0.0"]);

    let _cwd = CwdGuard::new(dir).unwrap();

    // Set up template vars with an env variable
    let mut vars = crate::template::TemplateVars::new();
    vars.set_env("IGNORE_TAG", "v3.0.0");

    // Use a template expression in ignore_tags
    let gc = crate::config::GitConfig {
        ignore_tags: Some(vec!["{{ .Env.IGNORE_TAG }}".to_string()]),
        ..Default::default()
    };

    // Without template_vars, the raw string "{{ .Env.IGNORE_TAG }}" won't
    // match any tag, so v3.0.0 is still included.
    let result_raw = find_latest_tag_matching("v{{ .Version }}", Some(&gc), None).unwrap();
    assert_eq!(result_raw, Some("v3.0.0".to_string()));

    // With template_vars, the template is rendered to "v3.0.0" which
    // matches and excludes that tag.
    let result_rendered =
        find_latest_tag_matching("v{{ .Version }}", Some(&gc), Some(&vars)).unwrap();
    assert_eq!(result_rendered, Some("v2.0.0".to_string()));
}

/// Create a git repo in `dir` with separate commits for each tag
/// (needed for `git describe --tags --abbrev=0` to work correctly).
fn init_repo_with_tagged_commits(dir: &std::path::Path, tags: &[&str]) {
    use std::process::Command;

    let run = |args: &[&str]| {
        let out = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args)
                    .current_dir(dir)
                    .env("GIT_AUTHOR_NAME", "test")
                    .env("GIT_AUTHOR_EMAIL", "test@test.com")
                    .env("GIT_COMMITTER_NAME", "test")
                    .env("GIT_COMMITTER_EMAIL", "test@test.com");
                cmd
            },
            "git",
        );
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };

    run(&["init"]);
    run(&["config", "user.email", "test@test.com"]);
    run(&["config", "user.name", "test"]);

    for (i, tag) in tags.iter().enumerate() {
        let filename = format!("file_{}", i);
        std::fs::write(dir.join(&filename), format!("content {}", i)).unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", &format!("commit for {}", tag)]);
        run(&["tag", tag]);
    }
}

#[test]
#[serial]
fn test_find_previous_tag_with_ignore_tags() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // Create commits with tags: v1.0.0, v2.0.0, v3.0.0
    // Each tag on a separate commit so git describe can find them.
    init_repo_with_tagged_commits(dir, &["v1.0.0", "v2.0.0", "v3.0.0"]);

    let _cwd = CwdGuard::new(dir).unwrap();

    // Without ignore_tags, previous tag of v3.0.0 should be v2.0.0
    let result = find_previous_tag("v3.0.0", None, None).unwrap();
    assert_eq!(result, Some("v2.0.0".to_string()));

    // With v2.0.0 in ignore_tags, it should be excluded via --exclude
    // and the previous tag should be v1.0.0
    let gc = crate::config::GitConfig {
        ignore_tags: Some(vec!["v2.0.0".to_string()]),
        ..Default::default()
    };
    let result_filtered = find_previous_tag("v3.0.0", Some(&gc), None).unwrap();
    assert_eq!(result_filtered, Some("v1.0.0".to_string()));
}

#[test]
#[serial]
fn test_find_previous_tag_with_ignore_tag_prefixes() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // Create tags where the previous tag has a prefix we want to ignore
    init_repo_with_tagged_commits(dir, &["v1.0.0", "nightly-v2.0.0", "v3.0.0"]);

    let _cwd = CwdGuard::new(dir).unwrap();

    // Without filtering, previous tag of v3.0.0 is nightly-v2.0.0
    let result = find_previous_tag("v3.0.0", None, None).unwrap();
    assert_eq!(result, Some("nightly-v2.0.0".to_string()));

    // With ignore_tag_prefixes=["nightly-"], nightly-v2.0.0 is excluded
    // via --exclude=nightly-* and git describe skips it, returning v1.0.0
    let gc = crate::config::GitConfig {
        ignore_tag_prefixes: Some(vec!["nightly-".to_string()]),
        ..Default::default()
    };
    let result_filtered = find_previous_tag("v3.0.0", Some(&gc), None).unwrap();
    assert_eq!(result_filtered, Some("v1.0.0".to_string()));
}

#[test]
#[serial]
fn test_find_previous_tag_no_config_unchanged_behavior() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tagged_commits(dir, &["v1.0.0", "v2.0.0"]);

    let _cwd = CwdGuard::new(dir).unwrap();

    let result = find_previous_tag("v2.0.0", None, None).unwrap();
    assert_eq!(result, Some("v1.0.0".to_string()));
}

// -----------------------------------------------------------------------
// strip_monorepo_prefix tests
// -----------------------------------------------------------------------

#[test]
fn test_strip_monorepo_prefix_with_match() {
    assert_eq!(
        strip_monorepo_prefix("subproject1/v1.2.3", "subproject1/"),
        "v1.2.3"
    );
}

#[test]
fn test_strip_monorepo_prefix_no_match() {
    assert_eq!(strip_monorepo_prefix("v1.2.3", "subproject1/"), "v1.2.3");
}

#[test]
fn test_strip_monorepo_prefix_empty_prefix() {
    assert_eq!(strip_monorepo_prefix("v1.2.3", ""), "v1.2.3");
}

#[test]
fn test_strip_monorepo_prefix_partial_match() {
    // "sub" is a prefix of "subproject1/" but not the full prefix.
    assert_eq!(
        strip_monorepo_prefix("subproject1/v1.2.3", "sub"),
        "project1/v1.2.3"
    );
}

// -----------------------------------------------------------------------
// find_latest_tag_matching_with_prefix (monorepo) tests
// -----------------------------------------------------------------------

#[test]
#[serial]
fn test_find_latest_tag_with_monorepo_prefix_filters_and_returns_full_tag() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(
        dir,
        &[
            "v1.0.0",
            "subproject1/v1.0.0",
            "subproject1/v2.0.0",
            "subproject2/v3.0.0",
        ],
    );

    let _cwd = CwdGuard::new(dir).unwrap();

    // With monorepo prefix "subproject1/", should only find subproject1 tags
    // and return the FULL tag (with prefix).
    let result =
        find_latest_tag_matching_with_prefix("v{{ .Version }}", None, None, Some("subproject1/"))
            .unwrap();
    assert_eq!(
        result,
        Some("subproject1/v2.0.0".to_string()),
        "should return the full tag with prefix"
    );
}

#[test]
#[serial]
fn test_find_latest_tag_with_monorepo_prefix_semver_comparison_uses_stripped_tag() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // Versions should be compared using the stripped tag
    init_repo_with_tags(dir, &["myapp/v1.0.0", "myapp/v2.0.0", "myapp/v1.5.0"]);

    let _cwd = CwdGuard::new(dir).unwrap();

    let result =
        find_latest_tag_matching_with_prefix("v{{ .Version }}", None, None, Some("myapp/"))
            .unwrap();
    assert_eq!(
        result,
        Some("myapp/v2.0.0".to_string()),
        "should pick the highest version based on stripped semver"
    );
}

#[test]
#[serial]
fn test_find_latest_tag_with_monorepo_prefix_no_matching_tags() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &["v1.0.0", "v2.0.0"]);

    let _cwd = CwdGuard::new(dir).unwrap();

    // No tags start with "myapp/" so result should be None.
    let result =
        find_latest_tag_matching_with_prefix("v{{ .Version }}", None, None, Some("myapp/"))
            .unwrap();
    assert_eq!(result, None);
}

#[test]
#[serial]
fn test_find_latest_tag_with_monorepo_prefix_none_behaves_like_original() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &["v1.0.0", "v1.1.0", "v2.0.0"]);

    let _cwd = CwdGuard::new(dir).unwrap();

    // Without monorepo prefix, should behave exactly like find_latest_tag_matching.
    let result_with_prefix =
        find_latest_tag_matching_with_prefix("v{{ .Version }}", None, None, None).unwrap();
    let result_original = find_latest_tag_matching("v{{ .Version }}", None, None).unwrap();
    assert_eq!(result_with_prefix, result_original);
    assert_eq!(result_with_prefix, Some("v2.0.0".to_string()));
}

#[test]
#[serial]
fn test_find_latest_tag_with_monorepo_prefix_and_prerelease() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &["svc/v1.0.0", "svc/v1.1.0-rc.1", "svc/v1.1.0"]);

    let _cwd = CwdGuard::new(dir).unwrap();

    let result =
        find_latest_tag_matching_with_prefix("v{{ .Version }}", None, None, Some("svc/")).unwrap();
    assert_eq!(
        result,
        Some("svc/v1.1.0".to_string()),
        "release v1.1.0 should win over v1.1.0-rc.1"
    );
}

// -----------------------------------------------------------------------
// bail!()-site redaction in git/ submodules.
//
// `git_output_in`, `add_path_in`, and `commit_in` interpolate raw `git`
// stderr into anyhow errors. The redact wrapper inserted at each call
// site must scrub any secret value reachable through the process env
// (e.g. GITHUB_TOKEN) before the message reaches user-visible logs.
// -----------------------------------------------------------------------

use super::commits::{add_path_in, commit_in};

#[test]
#[serial(token_env)]
fn test_add_path_in_bail_redacts_token_in_stderr() {
    // SAFETY: `serial_test` serializes env-var-mutating tests so the
    // process env is single-writer; this test sets GITHUB_TOKEN to a
    // sentinel value, triggers a non-existent path bail, then asserts
    // the sentinel does not appear in the error.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &[]);

    let secret = "ghp_addpathintestSentinel_123456789";
    let prev = std::env::var("GITHUB_TOKEN").ok();
    // SAFETY: serialized via `#[serial]`.
    unsafe {
        std::env::set_var("GITHUB_TOKEN", secret);
    }

    // Engineer stderr that mentions the token: we pre-write a file
    // named with the token, then `git add <nonexistent>` to trigger a
    // bail. The token does not enter stderr naturally, so we test the
    // redaction wiring by ensuring that any stderr text matching the
    // token would be scrubbed. We do this by adding a path that
    // git CANNOT add (the secret as a non-existent file name) so that
    // the git error itself names the secret.
    let nonexistent = dir.join(format!("missing-{secret}.txt"));
    let rel = nonexistent.strip_prefix(dir).unwrap();
    let err = add_path_in(dir, rel).expect_err("git add must fail on a non-existent path");
    let msg = format!("{err:#}");

    // Restore prior env before assertions.
    unsafe {
        if let Some(prev) = prev {
            std::env::set_var("GITHUB_TOKEN", prev);
        } else {
            std::env::remove_var("GITHUB_TOKEN");
        }
    }

    assert!(
        !msg.contains(secret),
        "add_path_in bail leaked GITHUB_TOKEN: {msg}"
    );
    assert!(
        msg.contains("$GITHUB_TOKEN"),
        "redaction must substitute $GITHUB_TOKEN: {msg}"
    );
}

#[test]
#[serial(token_env)]
fn test_commit_in_bail_redacts_token_in_stderr() {
    // Same shape as the add_path_in test, but for the `commit_in`
    // bail site. Set GITHUB_TOKEN, trigger a commit failure by
    // running in a directory with no staged changes AND a commit
    // message that embeds the secret (so git's stderr could echo it
    // back if a future git version ever did), then assert it was
    // redacted.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &[]);

    let secret = "ghp_commitintestSentinel_987654321";
    let prev = std::env::var("GITHUB_TOKEN").ok();
    unsafe {
        std::env::set_var("GITHUB_TOKEN", secret);
    }

    // With nothing staged, `git commit -m <msg>` exits 1 and prints
    // "nothing to commit" to stderr. The message itself contains the
    // secret; if a future git version surfaces commit-message text in
    // stderr the redact wrapper still scrubs the token.
    let msg_with_secret = format!("release {secret}");
    let err = commit_in(dir, &msg_with_secret, false)
        .expect_err("commit must fail when nothing is staged");
    let msg = format!("{err:#}");

    unsafe {
        if let Some(prev) = prev {
            std::env::set_var("GITHUB_TOKEN", prev);
        } else {
            std::env::remove_var("GITHUB_TOKEN");
        }
    }

    assert!(
        !msg.contains(secret),
        "commit_in bail leaked GITHUB_TOKEN: {msg}"
    );
}

#[test]
fn test_detect_github_repo_error_strips_url_credentials() {
    // `parse_github_remote` does not match a `gitlab.example.com` URL,
    // so feeding such a URL to the wrapping error path forces the
    // redaction helper to run on its argument. Exercise the redaction
    // wrapper directly because spinning up a non-github origin in a
    // temp repo just to trigger this branch is not worth the test
    // runtime.
    let leaky = "https://ghp_leakytoken@gitlab.example.com/grp/proj.git";
    // The helper used inside detect_github_repo:
    let scrubbed = redact_url_credentials(leaky);
    assert!(!scrubbed.contains("ghp_leakytoken"));
    assert_eq!(
        scrubbed,
        "https://<redacted>@gitlab.example.com/grp/proj.git"
    );
}

// ── short_commit_str — canonical short-hash truncation ─────────────────────

#[test]
fn short_commit_str_truncates_to_seven_chars_to_match_git_short() {
    use super::commits::{SHORT_COMMIT_LEN, short_commit_str};
    // git's `--short` default is 7 chars; the helper must match.
    assert_eq!(SHORT_COMMIT_LEN, 7);
    let full = "deadbeef1234567890abcdef";
    let short = short_commit_str(full);
    assert_eq!(short.len(), 7);
    assert_eq!(short, "deadbee");
}

#[test]
fn short_commit_str_passes_short_inputs_through_unchanged() {
    use super::commits::short_commit_str;
    // Inputs already at or under SHORT_COMMIT_LEN are returned
    // unchanged — saves an allocation in the common case where the
    // caller is already passing a short hash from a template var.
    assert_eq!(short_commit_str("abc"), "abc");
    assert_eq!(short_commit_str("abc1234"), "abc1234");
    assert_eq!(short_commit_str(""), "");
}

// ── head_is_at_tag — auto-detect tag commits ───────────────────────────────

#[test]
fn head_is_at_tag_returns_true_when_head_has_tag() {
    use super::tags::head_is_at_tag;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tagged_commits(dir, &["v1.0.0"]);
    // HEAD is at v1.0.0's commit; describe --exact-match should succeed.
    assert!(
        head_is_at_tag(dir).unwrap(),
        "HEAD has tag v1.0.0 attached; head_is_at_tag should return true"
    );
}

#[test]
fn head_is_at_tag_returns_false_when_head_has_no_tag() {
    use super::tags::head_is_at_tag;
    use std::process::Command;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tagged_commits(dir, &["v1.0.0"]);
    // Advance HEAD past the tagged commit so describe --exact-match fails.
    std::fs::write(dir.join("untagged.txt"), "no tag here").unwrap();
    anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.current_dir(dir)
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@test.com")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@test.com")
                .args(["add", "."]);
            cmd
        },
        "git",
    );
    anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.current_dir(dir)
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@test.com")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@test.com")
                .args(["commit", "-m", "post-tag commit"]);
            cmd
        },
        "git",
    );
    assert!(
        !head_is_at_tag(dir).unwrap(),
        "HEAD is one commit past v1.0.0; head_is_at_tag should return false"
    );
}

// -----------------------------------------------------------------------
// semver / smartsemver tag_sort modes
// -----------------------------------------------------------------------

#[test]
#[serial]
fn test_find_latest_tag_semver_mode_orders_by_semver() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &["v1.0.0", "v1.1.0-rc.1", "v1.1.0", "v1.2.0-beta.1"]);

    let _cwd = CwdGuard::new(dir).unwrap();

    let gc = crate::config::GitConfig {
        tag_sort: Some("semver".to_string()),
        ..Default::default()
    };
    let result = find_latest_tag_matching("v{{ .Version }}", Some(&gc), None).unwrap();
    // v1.2.0-beta.1 has the highest M.m.p tuple even though it's a prerelease.
    assert_eq!(result, Some("v1.2.0-beta.1".to_string()));
}

#[test]
#[serial]
fn test_find_latest_tag_semver_mode_ignores_prerelease_suffix_setting() {
    // For semver mode, `prerelease_suffix` must not flip the path into
    // git-delegated sort; ordering stays Rust-side SemVer.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &["v1.0.0", "v1.1.0-rc.1", "v1.1.0"]);

    let _cwd = CwdGuard::new(dir).unwrap();

    let gc = crate::config::GitConfig {
        tag_sort: Some("semver".to_string()),
        prerelease_suffix: Some("-rc".to_string()),
        ..Default::default()
    };
    let result = find_latest_tag_matching("v{{ .Version }}", Some(&gc), None).unwrap();
    // SemVer: release v1.1.0 > prerelease v1.1.0-rc.1.
    assert_eq!(result, Some("v1.1.0".to_string()));
}

#[test]
#[serial]
fn test_find_latest_tag_smartsemver_returns_semver_highest() {
    // find_latest_tag_matching with smartsemver performs pure SemVer ordering
    // without prerelease filtering. The filter applies only to the
    // previous-tag lookup (find_previous_tag*), where current_tag determines
    // whether prereleases should be excluded.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &["v1.0.0", "v1.1.0-rc.1", "v1.1.0", "v1.2.0-beta.1"]);

    let _cwd = CwdGuard::new(dir).unwrap();

    let gc = crate::config::GitConfig {
        tag_sort: Some("smartsemver".to_string()),
        ..Default::default()
    };

    let result = find_latest_tag_matching("v{{ .Version }}", Some(&gc), None).unwrap();
    // v1.2.0-beta.1 has the highest M.m.p tuple; no prerelease filtering here.
    assert_eq!(result, Some("v1.2.0-beta.1".to_string()));
}

#[test]
#[serial]
fn test_find_latest_tag_smartsemver_keeps_prereleases_for_prerelease_target() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &["v1.0.0", "v1.1.0-rc.1", "v1.1.0", "v1.2.0-beta.1"]);

    let _cwd = CwdGuard::new(dir).unwrap();

    let gc = crate::config::GitConfig {
        tag_sort: Some("smartsemver".to_string()),
        ..Default::default()
    };
    let mut vars = crate::template::TemplateVars::new();
    vars.set("Version", "v1.2.0-beta.2");

    let result = find_latest_tag_matching("v{{ .Version }}", Some(&gc), Some(&vars)).unwrap();
    assert_eq!(
        result,
        Some("v1.2.0-beta.1".to_string()),
        "smartsemver with prerelease target keeps all candidates"
    );
}

#[test]
#[serial]
fn test_find_previous_tag_smartsemver_rc1_classified_as_prerelease() {
    // The SemVer regex captures everything after the first `-` as the prerelease
    // identifier, so `v1.1.0-rc1` (no dot separator) is still flagged as a
    // prerelease and dropped by the smartsemver filter when current is a
    // release. No prerelease_suffix config is required.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tagged_commits(dir, &["v1.0.0", "v1.1.0-rc1", "v1.1.0"]);

    let _cwd = CwdGuard::new(dir).unwrap();

    let gc = crate::config::GitConfig {
        tag_sort: Some("smartsemver".to_string()),
        ..Default::default()
    };
    let result = find_previous_tag("v1.1.0", Some(&gc), None).unwrap();
    assert_eq!(
        result,
        Some("v1.0.0".to_string()),
        "v1.1.0-rc1 must be classified as prerelease via SemVer parsing alone"
    );

    // Confirm the parser agrees: v1.1.0-rc1 has prerelease = Some("rc1").
    let sv = parse_semver_tag("v1.1.0-rc1").unwrap();
    assert!(sv.is_prerelease());
}

#[test]
#[serial]
fn test_find_previous_tag_smartsemver_skips_prerelease_predecessor() {
    // Regression: shipping v0.2.0 after a v0.2.0-beta.3
    // tag must surface v0.1.0 as the predecessor (not the beta) so the
    // changelog has real commits to enumerate.
    //
    // No template_vars are needed — current_tag carries the signal.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tagged_commits(dir, &["v0.1.0", "v0.2.0-beta.3", "v0.2.0"]);

    let _cwd = CwdGuard::new(dir).unwrap();

    let gc = crate::config::GitConfig {
        tag_sort: Some("smartsemver".to_string()),
        ..Default::default()
    };

    // No template vars — signal is derived from current_tag "v0.2.0" (release).
    let result = find_previous_tag("v0.2.0", Some(&gc), None).unwrap();
    assert_eq!(result, Some("v0.1.0".to_string()));
}

#[test]
#[serial]
fn test_find_previous_tag_smartsemver_keeps_prerelease_when_current_is_prerelease() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tagged_commits(dir, &["v0.1.0", "v0.2.0-beta.1", "v0.2.0-beta.2"]);

    let _cwd = CwdGuard::new(dir).unwrap();

    let gc = crate::config::GitConfig {
        tag_sort: Some("smartsemver".to_string()),
        ..Default::default()
    };

    // current_tag is a prerelease → filter is off → v0.2.0-beta.1 is found.
    let result = find_previous_tag("v0.2.0-beta.2", Some(&gc), None).unwrap();
    assert_eq!(result, Some("v0.2.0-beta.1".to_string()));
}

#[test]
#[serial]
fn test_find_previous_tag_smartsemver_release_tag_filters_prereleases() {
    // No template_vars supplied; filter must engage from current_tag alone.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tagged_commits(dir, &["v0.1.0", "v0.2.0-beta.3", "v0.2.0"]);

    let _cwd = CwdGuard::new(dir).unwrap();

    let gc = crate::config::GitConfig {
        tag_sort: Some("smartsemver".to_string()),
        ..Default::default()
    };
    let result = find_previous_tag("v0.2.0", Some(&gc), None).unwrap();
    assert_eq!(
        result,
        Some("v0.1.0".to_string()),
        "smartsemver must skip v0.2.0-beta.3 with current_tag v0.2.0"
    );
}

#[test]
#[serial]
fn test_find_previous_tag_smartsemver_monorepo_prefix() {
    // Monorepo: tags "svc/v0.1.0", "svc/v0.2.0-beta.3", "svc/v0.2.0".
    // With current_tag "svc/v0.2.0" (release), smartsemver must return
    // "svc/v0.1.0", not "svc/v0.2.0-beta.3".
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tagged_commits(dir, &["svc/v0.1.0", "svc/v0.2.0-beta.3", "svc/v0.2.0"]);

    let _cwd = CwdGuard::new(dir).unwrap();

    let gc = crate::config::GitConfig {
        tag_sort: Some("smartsemver".to_string()),
        ..Default::default()
    };
    let result =
        find_previous_tag_with_prefix("svc/v0.2.0", Some(&gc), None, Some("svc/")).unwrap();
    assert_eq!(
        result,
        Some("svc/v0.1.0".to_string()),
        "smartsemver must skip svc/v0.2.0-beta.3 in monorepo mode"
    );
}

#[test]
#[serial]
fn test_find_previous_tag_smartsemver_early_dev_no_panic() {
    // Only tag is the current one; after excluding current_tag, no candidates remain.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tagged_commits(dir, &["v0.0.0"]);

    let _cwd = CwdGuard::new(dir).unwrap();

    let gc = crate::config::GitConfig {
        tag_sort: Some("smartsemver".to_string()),
        ..Default::default()
    };
    let result = find_previous_tag("v0.0.0", Some(&gc), None).unwrap();
    assert_eq!(result, None);
}

#[test]
#[serial]
fn list_remote_tag_names_dedupes_peeled_annotated_entries() {
    use std::process::Command;

    let work = tempfile::tempdir().unwrap();
    init_repo_with_tags(work.path(), &["v1.0.0"]);

    let run = |args: &[&str]| {
        let out = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args)
                    .current_dir(work.path())
                    .env("GIT_AUTHOR_NAME", "test")
                    .env("GIT_AUTHOR_EMAIL", "test@test.com")
                    .env("GIT_COMMITTER_NAME", "test")
                    .env("GIT_COMMITTER_EMAIL", "test@test.com");
                cmd
            },
            "git",
        );
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };
    // An annotated tag produces BOTH `refs/tags/v1.1.0` and the peeled
    // `refs/tags/v1.1.0^{}` in ls-remote output; each name must come back once.
    run(&["tag", "-a", "v1.1.0", "-m", "release v1.1.0"]);

    let bare = tempfile::tempdir().unwrap();
    let out = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.args(["init", "--bare", "-q"]).arg(bare.path());
            cmd
        },
        "git",
    );
    assert!(out.status.success(), "git init --bare failed");
    run(&["remote", "add", "origin", bare.path().to_str().unwrap()]);
    run(&["push", "origin", "v1.0.0", "v1.1.0"]);

    let mut names = list_remote_tag_names_in(work.path(), "origin").unwrap();
    names.sort();
    assert_eq!(names, vec!["v1.0.0".to_string(), "v1.1.0".to_string()]);

    // An unreachable remote propagates the error (callers fall back to local).
    run(&[
        "remote",
        "set-url",
        "origin",
        "/nonexistent/never-a-repo.git",
    ]);
    assert!(list_remote_tag_names_in(work.path(), "origin").is_err());
}

/// Set up an empty repo with a single commit and ephemeral SSH tag-signing
/// configured (no gpg-agent needed). Returns the keydir so the key outlives
/// the repo for the whole test.
fn init_repo_with_ssh_signing(dir: &std::path::Path) -> tempfile::TempDir {
    use std::process::Command;

    let run = |args: &[&str]| {
        let out = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args)
                    .current_dir(dir)
                    .env("GIT_AUTHOR_NAME", "test")
                    .env("GIT_AUTHOR_EMAIL", "test@test.com")
                    .env("GIT_COMMITTER_NAME", "test")
                    .env("GIT_COMMITTER_EMAIL", "test@test.com");
                cmd
            },
            "git",
        );
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };

    run(&["init"]);
    run(&["config", "user.email", "test@test.com"]);
    run(&["config", "user.name", "test"]);
    std::fs::write(dir.join("README"), "init").unwrap();
    run(&["add", "."]);
    run(&["commit", "-m", "initial"]);

    let keydir = tempfile::tempdir().unwrap();
    let key_path = keydir.path().join("sign_key");
    let keygen = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("ssh-keygen");
            cmd.args(["-t", "ed25519", "-N", "", "-C", "anodizer-test", "-f"])
                .arg(&key_path);
            cmd
        },
        "ssh-keygen",
    );
    assert!(
        keygen.status.success(),
        "ssh-keygen failed: {}",
        String::from_utf8_lossy(&keygen.stderr)
    );
    let pub_path = format!("{}.pub", key_path.display());
    run(&["config", "gpg.format", "ssh"]);
    run(&["config", "user.signingkey", &pub_path]);
    keydir
}

/// P1 regression: a prior run leaves an UNSIGNED tag at HEAD (debris); a
/// re-run with `sign=true` must upgrade it to a SIGNED tag via the
/// idempotent-reuse branch, not silently reuse the unsigned one.
#[test]
#[serial]
fn create_tag_local_only_upgrades_unsigned_reuse_to_signed() {
    let tmp = tempfile::tempdir().unwrap();
    let _keydir = init_repo_with_ssh_signing(tmp.path());
    let log = crate::log::StageLogger::new("test", crate::log::Verbosity::Quiet);

    // Seed the exact debris the bug is about: an UNSIGNED annotated tag at HEAD.
    let seeded = create_tag_local_only(tmp.path(), "v1.0.0", "seed", false, false, &log);
    assert!(seeded.is_ok(), "seeding unsigned tag failed: {seeded:?}");
    assert!(
        !tag_is_signed(tmp.path(), "v1.0.0").unwrap(),
        "precondition: seeded tag must be unsigned"
    );

    // Re-run with sign=true: the reuse branch must delete + re-create signed.
    let re = create_tag_local_only(tmp.path(), "v1.0.0", "seed", false, true, &log);
    assert!(re.is_ok(), "signed re-run failed: {re:?}");
    assert!(
        tag_is_signed(tmp.path(), "v1.0.0").unwrap(),
        "reuse path must UPGRADE the unsigned tag to a signed one"
    );
}

/// The reuse branch must NOT re-create when the existing tag is ALREADY signed
/// (sign=true): a signed tag at HEAD is reused as-is.
#[test]
#[serial]
fn create_tag_local_only_reuses_already_signed_tag() {
    let tmp = tempfile::tempdir().unwrap();
    let _keydir = init_repo_with_ssh_signing(tmp.path());
    let log = crate::log::StageLogger::new("test", crate::log::Verbosity::Quiet);

    let first = create_tag_local_only(tmp.path(), "v1.0.0", "seed", false, true, &log);
    assert!(first.is_ok(), "first signed create failed: {first:?}");
    assert!(tag_is_signed(tmp.path(), "v1.0.0").unwrap());

    // Re-run over the already-signed tag: idempotent reuse, still signed.
    let again = create_tag_local_only(tmp.path(), "v1.0.0", "seed", false, true, &log);
    assert!(again.is_ok(), "signed reuse failed: {again:?}");
    assert!(
        tag_is_signed(tmp.path(), "v1.0.0").unwrap(),
        "already-signed tag must remain signed on reuse"
    );
}

// ---------------------------------------------------------------------------
// Path-taking (`*_in`) tag mutate/query helpers — driven against throwaway
// git repos (no cwd mutation, so no `#[serial]` needed).
// ---------------------------------------------------------------------------

/// Run a git command in `dir`, asserting success.
fn tags_run_git(dir: &std::path::Path, args: &[&str]) {
    use std::process::Command;
    let out = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@test.com")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@test.com");
            cmd
        },
        "git",
    );
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Init a repo in `dir` with one commit on `main`.
fn tags_init_commit_repo(dir: &std::path::Path) {
    tags_run_git(dir, &["init", "-q", "-b", "main"]);
    tags_run_git(dir, &["config", "user.email", "test@test.com"]);
    tags_run_git(dir, &["config", "user.name", "test"]);
    std::fs::write(dir.join("README"), "init").unwrap();
    tags_run_git(dir, &["add", "."]);
    tags_run_git(dir, &["commit", "-q", "-m", "initial"]);
}

/// Add a bare `origin` remote to the repo in `dir`; returns the bare dir
/// (keep it alive for the remote's lifetime).
fn tags_add_bare_origin(dir: &std::path::Path) -> tempfile::TempDir {
    let bare = tempfile::tempdir().unwrap();
    tags_run_git(bare.path(), &["init", "--bare", "-q"]);
    tags_run_git(
        dir,
        &["remote", "add", "origin", bare.path().to_str().unwrap()],
    );
    bare
}

fn tags_quiet_log() -> crate::log::StageLogger {
    crate::log::StageLogger::new("test", crate::log::Verbosity::Quiet)
}

#[test]
fn get_branch_semver_tags_in_excludes_tags_unmerged_into_head() {
    use super::tags::get_branch_semver_tags_in;
    let tmp = tempfile::tempdir().unwrap();
    tags_init_commit_repo(tmp.path());
    // v1.0.0 sits on main (merged into HEAD).
    tags_run_git(tmp.path(), &["tag", "v1.0.0"]);
    // v9.9.9 sits on a divergent branch that is NEVER merged into main.
    tags_run_git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
    std::fs::write(tmp.path().join("f"), "x").unwrap();
    tags_run_git(tmp.path(), &["add", "."]);
    tags_run_git(tmp.path(), &["commit", "-q", "-m", "feature"]);
    tags_run_git(tmp.path(), &["tag", "v9.9.9"]);
    tags_run_git(tmp.path(), &["checkout", "-q", "main"]);

    let tags = get_branch_semver_tags_in(tmp.path(), "v", None, None).unwrap();
    // `--merged HEAD` must keep v1.0.0 and drop the unmerged v9.9.9.
    assert_eq!(
        tags,
        vec!["v1.0.0".to_string()],
        "only tags merged into HEAD are returned; v9.9.9 is on an unmerged branch"
    );
}

#[test]
fn create_and_push_tag_in_creates_tag_and_warns_without_origin() {
    use super::tags::create_and_push_tag_in;
    let tmp = tempfile::tempdir().unwrap();
    tags_init_commit_repo(tmp.path());
    let log = tags_quiet_log();
    // No origin, non-strict: the tag is created locally and the missing push
    // is a warning (not an error).
    create_and_push_tag_in(tmp.path(), "v1.0.0", "rel", false, false, &log, false)
        .expect("tag creation must succeed without an origin in non-strict mode");
    let out = super::git_output_in(tmp.path(), &["tag", "--list"]).unwrap();
    assert!(
        out.lines().any(|l| l == "v1.0.0"),
        "the annotated tag must exist locally; got {out:?}"
    );
}

#[test]
fn create_and_push_tag_in_pushes_to_origin() {
    use super::tags::{create_and_push_tag_in, list_remote_tag_names_in};
    let tmp = tempfile::tempdir().unwrap();
    tags_init_commit_repo(tmp.path());
    let _bare = tags_add_bare_origin(tmp.path());
    let log = tags_quiet_log();
    create_and_push_tag_in(tmp.path(), "v2.0.0", "rel", false, false, &log, true)
        .expect("push to a present origin must succeed");
    let names = list_remote_tag_names_in(tmp.path(), "origin").unwrap();
    assert!(
        names.contains(&"v2.0.0".to_string()),
        "the tag must land on origin; got {names:?}"
    );
}

#[test]
fn create_and_push_tag_in_strict_bails_without_origin() {
    use super::tags::create_and_push_tag_in;
    let tmp = tempfile::tempdir().unwrap();
    tags_init_commit_repo(tmp.path());
    let log = tags_quiet_log();
    let err = create_and_push_tag_in(tmp.path(), "v3.0.0", "rel", false, false, &log, true)
        .expect_err("strict mode with no origin must error");
    assert!(
        format!("{err:#}").contains("no 'origin' remote"),
        "strict-mode error must name the missing origin: {err:#}"
    );
}

#[test]
fn delete_local_tag_in_is_idempotent_and_removes_present_tags() {
    use super::tags::delete_local_tag_in;
    let tmp = tempfile::tempdir().unwrap();
    tags_init_commit_repo(tmp.path());
    tags_run_git(tmp.path(), &["tag", "v1.0.0"]);
    // Present tag: removed.
    delete_local_tag_in(tmp.path(), "v1.0.0").expect("deleting a present tag succeeds");
    let out = super::git_output_in(tmp.path(), &["tag", "--list"]).unwrap();
    assert!(!out.lines().any(|l| l == "v1.0.0"), "tag must be gone");
    // Missing tag: idempotent success (the `not found` branch).
    delete_local_tag_in(tmp.path(), "v1.0.0")
        .expect("deleting an already-absent tag must be idempotent, not an error");
}

#[test]
fn delete_remote_tag_in_removes_a_present_remote_tag() {
    use super::tags::{delete_remote_tag_in, list_remote_tag_names_in};
    let tmp = tempfile::tempdir().unwrap();
    tags_init_commit_repo(tmp.path());
    let _bare = tags_add_bare_origin(tmp.path());
    // Seed a tag on origin, then delete it: the success path returns Ok and the
    // ref is gone from the remote.
    tags_run_git(tmp.path(), &["tag", "v1.0.0"]);
    tags_run_git(tmp.path(), &["push", "-q", "origin", "v1.0.0"]);
    assert!(
        list_remote_tag_names_in(tmp.path(), "origin")
            .unwrap()
            .contains(&"v1.0.0".to_string()),
        "precondition: the tag is on origin"
    );
    delete_remote_tag_in(tmp.path(), "v1.0.0").expect("deleting a present remote tag succeeds");
    assert!(
        !list_remote_tag_names_in(tmp.path(), "origin")
            .unwrap()
            .contains(&"v1.0.0".to_string()),
        "the tag must be gone from origin after delete"
    );
}

#[test]
fn delete_remote_tag_in_bails_without_origin() {
    use super::tags::delete_remote_tag_in;
    let tmp = tempfile::tempdir().unwrap();
    tags_init_commit_repo(tmp.path());
    // No origin at all: the push fails for a reason other than "already absent",
    // so it must bubble up as an error.
    let err = delete_remote_tag_in(tmp.path(), "v1.0.0")
        .expect_err("a push with no origin remote must error");
    assert!(
        format!("{err:#}").contains("git push origin"),
        "error must surface the failed push: {err:#}"
    );
}

#[test]
fn get_tags_at_sha_in_returns_empty_on_unknown_sha() {
    use super::tags::get_tags_at_sha_in;
    let tmp = tempfile::tempdir().unwrap();
    tags_init_commit_repo(tmp.path());
    // A malformed object name makes `git tag --points-at` exit non-zero (a
    // valid-form but non-existent 40-hex sha exits 0 with no output, which would
    // pass through the success path instead); the helper must warn and return an
    // empty list rather than erroring.
    let tags = get_tags_at_sha_in(tmp.path(), "bad!!ref")
        .expect("a malformed revision yields no tags, not an error");
    assert!(
        tags.is_empty(),
        "malformed revision must yield no tags; got {tags:?}"
    );
}

#[test]
fn push_branch_and_tags_atomic_in_dry_run_pushes_nothing() {
    use super::tags::{AtomicPushSpec, push_branch_and_tags_atomic_in};
    let tmp = tempfile::tempdir().unwrap();
    tags_init_commit_repo(tmp.path());
    tags_run_git(tmp.path(), &["tag", "v1.0.0"]);
    let _bare = tags_add_bare_origin(tmp.path());
    let log = tags_quiet_log();
    let tags = vec!["v1.0.0".to_string()];

    // Dry-run with a REAL origin present: the tag must NOT reach the remote.
    push_branch_and_tags_atomic_in(
        tmp.path(),
        &AtomicPushSpec {
            remote: "origin",
            branch: Some("main"),
            tags: &tags,
            dry_run: true,
            strict: false,
        },
        &log,
    )
    .expect("dry-run push is a no-op");
    let names = super::tags::list_remote_tag_names_in(tmp.path(), "origin").unwrap();
    assert!(
        names.is_empty(),
        "dry-run must push nothing to origin; got {names:?}"
    );
}

#[test]
fn push_branch_and_tags_atomic_in_lands_branch_and_tags_on_origin() {
    use super::tags::{AtomicPushSpec, list_remote_tag_names_in, push_branch_and_tags_atomic_in};
    let tmp = tempfile::tempdir().unwrap();
    tags_init_commit_repo(tmp.path());
    tags_run_git(tmp.path(), &["tag", "v1.0.0"]);
    let bare = tags_add_bare_origin(tmp.path());
    let log = tags_quiet_log();
    let tags = vec!["v1.0.0".to_string()];

    // Real atomic push of branch HEAD + the tag: both refs must land on origin.
    push_branch_and_tags_atomic_in(
        tmp.path(),
        &AtomicPushSpec {
            remote: "origin",
            branch: Some("main"),
            tags: &tags,
            dry_run: false,
            strict: true,
        },
        &log,
    )
    .expect("atomic branch+tag push to a present origin must succeed");

    let names = list_remote_tag_names_in(tmp.path(), "origin").unwrap();
    assert!(
        names.contains(&"v1.0.0".to_string()),
        "the tag must land on origin; got {names:?}"
    );
    // The exact branch ref must exist on the bare remote too (show-ref --verify
    // resolves the full refname, so a differently-named branch would not pass).
    super::git_output_in(bare.path(), &["show-ref", "--verify", "refs/heads/main"])
        .expect("refs/heads/main must exist on origin after the atomic push");
}

#[test]
fn push_branch_and_tags_atomic_in_nothing_to_push_is_noop() {
    use super::tags::{AtomicPushSpec, push_branch_and_tags_atomic_in};
    let tmp = tempfile::tempdir().unwrap();
    tags_init_commit_repo(tmp.path());
    let log = tags_quiet_log();
    // No branch and no tags: the "nothing to push" guard returns Ok before any
    // remote check (there is no origin, so reaching the remote check would err
    // under strict — proving the guard short-circuits first).
    push_branch_and_tags_atomic_in(
        tmp.path(),
        &AtomicPushSpec {
            remote: "origin",
            branch: None,
            tags: &[],
            dry_run: false,
            strict: true,
        },
        &log,
    )
    .expect("nothing-to-push short-circuits before the remote check");
}

#[test]
fn push_branch_and_tags_atomic_in_branch_only_empty_tags_pushes_branch() {
    use super::tags::{AtomicPushSpec, push_branch_and_tags_atomic_in};
    let tmp = tempfile::tempdir().unwrap();
    tags_init_commit_repo(tmp.path());
    let bare = tags_add_bare_origin(tmp.path());
    let log = tags_quiet_log();
    // branch = Some + empty tags: the tags-empty fork does a plain (non-atomic)
    // branch push. The branch ref must land on origin.
    push_branch_and_tags_atomic_in(
        tmp.path(),
        &AtomicPushSpec {
            remote: "origin",
            branch: Some("main"),
            tags: &[],
            dry_run: false,
            strict: true,
        },
        &log,
    )
    .expect("branch-only push must succeed");
    super::git_output_in(bare.path(), &["show-ref", "--verify", "refs/heads/main"])
        .expect("refs/heads/main must exist on origin after the branch-only push");
}

#[test]
fn push_branch_and_tags_atomic_in_strict_bails_without_remote() {
    use super::tags::{AtomicPushSpec, push_branch_and_tags_atomic_in};
    let tmp = tempfile::tempdir().unwrap();
    tags_init_commit_repo(tmp.path());
    let log = tags_quiet_log();
    let tags = vec!["v1.0.0".to_string()];
    tags_run_git(tmp.path(), &["tag", "v1.0.0"]);
    // No origin + strict: the missing-remote guard must error.
    let err = push_branch_and_tags_atomic_in(
        tmp.path(),
        &AtomicPushSpec {
            remote: "origin",
            branch: Some("main"),
            tags: &tags,
            dry_run: false,
            strict: true,
        },
        &log,
    )
    .expect_err("strict push with no remote must error");
    assert!(
        format!("{err:#}").contains("no 'origin' remote"),
        "strict-mode error must name the missing remote: {err:#}"
    );
}

#[test]
fn find_previous_tag_in_returns_none_when_repo_has_no_tags() {
    use super::tags::find_previous_tag_in;
    let tmp = tempfile::tempdir().unwrap();
    tags_init_commit_repo(tmp.path());
    // A repo with a commit but zero tags: the empty-tag-list early return yields
    // None (no previous tag to find).
    let prev = find_previous_tag_in(tmp.path(), "v1.0.0", None, None).unwrap();
    assert_eq!(prev, None, "a tagless repo has no previous tag");
}

#[test]
fn get_first_commit_in_returns_the_root_commit() {
    use super::tags::get_first_commit_in;
    let tmp = tempfile::tempdir().unwrap();
    tags_init_commit_repo(tmp.path());
    // A second commit so HEAD != root.
    std::fs::write(tmp.path().join("b"), "x").unwrap();
    tags_run_git(tmp.path(), &["add", "."]);
    tags_run_git(tmp.path(), &["commit", "-q", "-m", "second"]);
    let root = get_first_commit_in(tmp.path()).unwrap();
    // The reported root must match `git rev-list --max-parents=0 HEAD`.
    let expected = super::git_output_in(tmp.path(), &["rev-list", "--max-parents=0", "HEAD"])
        .unwrap()
        .lines()
        .last()
        .unwrap()
        .to_string();
    assert_eq!(
        root, expected,
        "must return the repository's root commit sha"
    );
}

#[test]
fn tag_points_at_head_in_true_for_head_tag_false_otherwise() {
    use super::tags::tag_points_at_head_in;
    let tmp = tempfile::tempdir().unwrap();
    tags_init_commit_repo(tmp.path());
    tags_run_git(tmp.path(), &["tag", "at-head"]);
    // A tag on an EARLIER commit: advance HEAD past it.
    std::fs::write(tmp.path().join("b"), "x").unwrap();
    tags_run_git(tmp.path(), &["add", "."]);
    tags_run_git(tmp.path(), &["commit", "-q", "-m", "second"]);
    tags_run_git(tmp.path(), &["tag", "at-head-2"]);

    assert!(
        tag_points_at_head_in(tmp.path(), "at-head-2").unwrap(),
        "a tag on HEAD must report true"
    );
    assert!(
        !tag_points_at_head_in(tmp.path(), "at-head").unwrap(),
        "a tag on an earlier commit must report false"
    );
}

#[test]
fn get_tags_at_head_in_lists_only_head_tags() {
    use super::tags::get_tags_at_head_in;
    let tmp = tempfile::tempdir().unwrap();
    tags_init_commit_repo(tmp.path());
    tags_run_git(tmp.path(), &["tag", "old"]);
    std::fs::write(tmp.path().join("b"), "x").unwrap();
    tags_run_git(tmp.path(), &["add", "."]);
    tags_run_git(tmp.path(), &["commit", "-q", "-m", "second"]);
    tags_run_git(tmp.path(), &["tag", "new"]);
    let at_head = get_tags_at_head_in(tmp.path()).unwrap();
    assert_eq!(
        at_head,
        vec!["new".to_string()],
        "only the tag on HEAD is returned, not the one on the earlier commit"
    );
}

#[test]
fn list_tags_with_prefix_filters_and_sorts_by_reverse_semver() {
    use super::tags::list_tags_with_prefix;
    let tmp = tempfile::tempdir().unwrap();
    tags_init_commit_repo(tmp.path());
    for t in ["v1.0.0", "v1.2.0", "v2.0.0", "other-1"] {
        tags_run_git(tmp.path(), &["tag", t]);
    }
    let tags = list_tags_with_prefix(tmp.path(), "v").unwrap();
    // Only the `v`-prefixed tags, newest semver first; `other-1` excluded.
    assert_eq!(
        tags,
        vec![
            "v2.0.0".to_string(),
            "v1.2.0".to_string(),
            "v1.0.0".to_string()
        ],
    );
}
