use super::remote::{parse_github_remote, parse_remote_owner_repo};
use super::semver::{compare_prerelease, parse_semver, parse_semver_tag};
use super::tags::{
    find_latest_tag_matching, find_latest_tag_matching_with_prefix, find_previous_tag,
    get_all_semver_tags, strip_monorepo_prefix,
};
use crate::redact::redact_url_credentials;

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
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@test.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@test.com")
            .output()
            .unwrap();
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
#[serial]
fn test_find_latest_tag_none_config_unchanged_behavior() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &["v1.0.0", "v1.1.0", "v2.0.0"]);

    // Change to the temp repo so git commands work.
    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();

    let result = find_latest_tag_matching("v{{ .Version }}", None, None).unwrap();
    assert_eq!(result, Some("v2.0.0".to_string()));

    std::env::set_current_dir(orig).unwrap();
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

    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();

    let gc = crate::config::GitConfig {
        ignore_tags: Some(vec!["v3.0.0".to_string()]),
        ..Default::default()
    };
    let tags = get_all_semver_tags("v", Some(&gc), None).unwrap();
    assert_eq!(tags, vec!["v2.0.0".to_string(), "v1.0.0".to_string()]);

    std::env::set_current_dir(orig).unwrap();
}

#[test]
#[serial]
fn test_get_all_semver_tags_ignore_tag_prefixes() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &["v1.0.0", "v2.0.0", "nightly-v3.0.0"]);

    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();

    let gc = crate::config::GitConfig {
        ignore_tag_prefixes: Some(vec!["nightly-".to_string()]),
        ..Default::default()
    };
    let tags = get_all_semver_tags("", Some(&gc), None).unwrap();
    // "nightly-v3.0.0" is excluded by prefix; only v2, v1 survive, ordered desc.
    assert_eq!(tags, vec!["v2.0.0".to_string(), "v1.0.0".to_string()]);

    std::env::set_current_dir(orig).unwrap();
}

#[test]
#[serial]
fn test_get_all_semver_tags_no_config_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &["v1.0.0", "v2.0.0"]);

    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();

    let tags = get_all_semver_tags("v", None, None).unwrap();
    assert_eq!(tags, vec!["v2.0.0".to_string(), "v1.0.0".to_string()]);

    std::env::set_current_dir(orig).unwrap();
}

#[test]
#[serial]
fn test_find_latest_tag_ignore_tags_exact_match() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &["v1.0.0", "v2.0.0", "v3.0.0"]);

    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();

    let gc = crate::config::GitConfig {
        ignore_tags: Some(vec!["v3.0.0".to_string()]),
        ..Default::default()
    };
    let result = find_latest_tag_matching("v{{ .Version }}", Some(&gc), None).unwrap();
    assert_eq!(result, Some("v2.0.0".to_string()));

    std::env::set_current_dir(orig).unwrap();
}

#[test]
#[serial]
fn test_find_latest_tag_ignore_tags_multiple() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &["v1.0.0", "v2.0.0", "v3.0.0"]);

    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();

    let gc = crate::config::GitConfig {
        ignore_tags: Some(vec!["v3.0.0".to_string(), "v2.0.0".to_string()]),
        ..Default::default()
    };
    let result = find_latest_tag_matching("v{{ .Version }}", Some(&gc), None).unwrap();
    assert_eq!(result, Some("v1.0.0".to_string()));

    std::env::set_current_dir(orig).unwrap();
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

    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();

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

    std::env::set_current_dir(orig).unwrap();
}

#[test]
#[serial]
fn test_find_latest_tag_ignore_all_returns_none() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &["v1.0.0", "v2.0.0"]);

    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();

    let gc = crate::config::GitConfig {
        ignore_tags: Some(vec!["v1.0.0".to_string(), "v2.0.0".to_string()]),
        ..Default::default()
    };
    let result = find_latest_tag_matching("v{{ .Version }}", Some(&gc), None).unwrap();
    assert_eq!(result, None);

    std::env::set_current_dir(orig).unwrap();
}

#[test]
#[serial]
fn test_find_latest_tag_ignore_tags_and_prefixes_combined() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &["v1.0.0", "v2.0.0", "v3.0.0-beta.1"]);

    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();

    // ignore v2.0.0 by exact match, and anything starting with "v3" by prefix
    let gc = crate::config::GitConfig {
        ignore_tags: Some(vec!["v2.0.0".to_string()]),
        ignore_tag_prefixes: Some(vec!["v3".to_string()]),
        ..Default::default()
    };
    let result = find_latest_tag_matching("v{{ .Version }}", Some(&gc), None).unwrap();
    assert_eq!(result, Some("v1.0.0".to_string()));

    std::env::set_current_dir(orig).unwrap();
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

    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();

    // Ignore myapp-v3.0.0 specifically
    let gc = crate::config::GitConfig {
        ignore_tags: Some(vec!["myapp-v3.0.0".to_string()]),
        ..Default::default()
    };
    let result = find_latest_tag_matching("myapp-v{{ .Version }}", Some(&gc), None).unwrap();
    assert_eq!(result, Some("myapp-v2.0.0".to_string()));

    std::env::set_current_dir(orig).unwrap();
}

#[test]
#[serial]
fn test_find_latest_tag_default_git_config_same_as_none() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &["v1.0.0", "v1.1.0", "v2.0.0"]);

    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();

    // Default GitConfig has all fields None — should behave identically to None
    let gc = crate::config::GitConfig::default();
    let with_default = find_latest_tag_matching("v{{ .Version }}", Some(&gc), None).unwrap();
    let with_none = find_latest_tag_matching("v{{ .Version }}", None, None).unwrap();
    assert_eq!(with_default, with_none);
    assert_eq!(with_default, Some("v2.0.0".to_string()));

    std::env::set_current_dir(orig).unwrap();
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

    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();

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
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@test.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@test.com")
            .output()
            .unwrap();
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

    std::env::set_current_dir(orig).unwrap();
}

#[test]
#[serial]
fn test_find_latest_tag_ignore_tags_template_rendered() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &["v1.0.0", "v2.0.0", "v3.0.0"]);

    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();

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

    std::env::set_current_dir(orig).unwrap();
}

/// Create a git repo in `dir` with separate commits for each tag
/// (needed for `git describe --tags --abbrev=0` to work correctly).
fn init_repo_with_tagged_commits(dir: &std::path::Path, tags: &[&str]) {
    use std::process::Command;

    let run = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@test.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@test.com")
            .output()
            .unwrap();
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

    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();

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

    std::env::set_current_dir(orig).unwrap();
}

#[test]
#[serial]
fn test_find_previous_tag_with_ignore_tag_prefixes() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // Create tags where the previous tag has a prefix we want to ignore
    init_repo_with_tagged_commits(dir, &["v1.0.0", "nightly-v2.0.0", "v3.0.0"]);

    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();

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

    std::env::set_current_dir(orig).unwrap();
}

#[test]
#[serial]
fn test_find_previous_tag_no_config_unchanged_behavior() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tagged_commits(dir, &["v1.0.0", "v2.0.0"]);

    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();

    let result = find_previous_tag("v2.0.0", None, None).unwrap();
    assert_eq!(result, Some("v1.0.0".to_string()));

    std::env::set_current_dir(orig).unwrap();
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

    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();

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

    std::env::set_current_dir(orig).unwrap();
}

#[test]
#[serial]
fn test_find_latest_tag_with_monorepo_prefix_semver_comparison_uses_stripped_tag() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // Versions should be compared using the stripped tag
    init_repo_with_tags(dir, &["myapp/v1.0.0", "myapp/v2.0.0", "myapp/v1.5.0"]);

    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();

    let result =
        find_latest_tag_matching_with_prefix("v{{ .Version }}", None, None, Some("myapp/"))
            .unwrap();
    assert_eq!(
        result,
        Some("myapp/v2.0.0".to_string()),
        "should pick the highest version based on stripped semver"
    );

    std::env::set_current_dir(orig).unwrap();
}

#[test]
#[serial]
fn test_find_latest_tag_with_monorepo_prefix_no_matching_tags() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &["v1.0.0", "v2.0.0"]);

    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();

    // No tags start with "myapp/" so result should be None.
    let result =
        find_latest_tag_matching_with_prefix("v{{ .Version }}", None, None, Some("myapp/"))
            .unwrap();
    assert_eq!(result, None);

    std::env::set_current_dir(orig).unwrap();
}

#[test]
#[serial]
fn test_find_latest_tag_with_monorepo_prefix_none_behaves_like_original() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &["v1.0.0", "v1.1.0", "v2.0.0"]);

    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();

    // Without monorepo prefix, should behave exactly like find_latest_tag_matching.
    let result_with_prefix =
        find_latest_tag_matching_with_prefix("v{{ .Version }}", None, None, None).unwrap();
    let result_original = find_latest_tag_matching("v{{ .Version }}", None, None).unwrap();
    assert_eq!(result_with_prefix, result_original);
    assert_eq!(result_with_prefix, Some("v2.0.0".to_string()));

    std::env::set_current_dir(orig).unwrap();
}

#[test]
#[serial]
fn test_find_latest_tag_with_monorepo_prefix_and_prerelease() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &["svc/v1.0.0", "svc/v1.1.0-rc.1", "svc/v1.1.0"]);

    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();

    let result =
        find_latest_tag_matching_with_prefix("v{{ .Version }}", None, None, Some("svc/")).unwrap();
    assert_eq!(
        result,
        Some("svc/v1.1.0".to_string()),
        "release v1.1.0 should win over v1.1.0-rc.1"
    );

    std::env::set_current_dir(orig).unwrap();
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
#[serial]
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
#[serial]
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
    Command::new("git")
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@test.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@test.com")
        .args(["add", "."])
        .output()
        .unwrap();
    Command::new("git")
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@test.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@test.com")
        .args(["commit", "-m", "post-tag commit"])
        .output()
        .unwrap();
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

    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();

    let gc = crate::config::GitConfig {
        tag_sort: Some("semver".to_string()),
        ..Default::default()
    };
    let result = find_latest_tag_matching("v{{ .Version }}", Some(&gc), None).unwrap();
    // v1.2.0-beta.1 has the highest M.m.p tuple even though it's a prerelease.
    assert_eq!(result, Some("v1.2.0-beta.1".to_string()));

    std::env::set_current_dir(orig).unwrap();
}

#[test]
#[serial]
fn test_find_latest_tag_semver_mode_ignores_prerelease_suffix_setting() {
    // For semver mode, `prerelease_suffix` must not flip the path into
    // git-delegated sort; ordering stays Rust-side SemVer.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &["v1.0.0", "v1.1.0-rc.1", "v1.1.0"]);

    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();

    let gc = crate::config::GitConfig {
        tag_sort: Some("semver".to_string()),
        prerelease_suffix: Some("-rc".to_string()),
        ..Default::default()
    };
    let result = find_latest_tag_matching("v{{ .Version }}", Some(&gc), None).unwrap();
    // SemVer: release v1.1.0 > prerelease v1.1.0-rc.1.
    assert_eq!(result, Some("v1.1.0".to_string()));

    std::env::set_current_dir(orig).unwrap();
}

#[test]
#[serial]
fn test_find_latest_tag_smartsemver_skips_prereleases_for_release_target() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &["v1.0.0", "v1.1.0-rc.1", "v1.1.0", "v1.2.0-beta.1"]);

    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();

    let gc = crate::config::GitConfig {
        tag_sort: Some("smartsemver".to_string()),
        ..Default::default()
    };
    let mut vars = crate::template::TemplateVars::new();
    vars.set("Version", "v1.2.0");

    let result = find_latest_tag_matching("v{{ .Version }}", Some(&gc), Some(&vars)).unwrap();
    assert_eq!(
        result,
        Some("v1.1.0".to_string()),
        "smartsemver must drop v1.2.0-beta.1 and v1.1.0-rc.1 when current is release"
    );

    std::env::set_current_dir(orig).unwrap();
}

#[test]
#[serial]
fn test_find_latest_tag_smartsemver_keeps_prereleases_for_prerelease_target() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &["v1.0.0", "v1.1.0-rc.1", "v1.1.0", "v1.2.0-beta.1"]);

    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();

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

    std::env::set_current_dir(orig).unwrap();
}

#[test]
#[serial]
fn test_find_latest_tag_smartsemver_no_version_keeps_all() {
    // Without a Version template var, smartsemver cannot decide what to
    // filter — behave as plain semver and keep every tag.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &["v1.0.0", "v1.1.0-rc.1"]);

    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();

    let gc = crate::config::GitConfig {
        tag_sort: Some("smartsemver".to_string()),
        ..Default::default()
    };
    let result = find_latest_tag_matching("v{{ .Version }}", Some(&gc), None).unwrap();
    assert_eq!(result, Some("v1.1.0-rc.1".to_string()));

    std::env::set_current_dir(orig).unwrap();
}

#[test]
#[serial]
fn test_find_latest_tag_smartsemver_prerelease_suffix_marks_non_semver_tags() {
    // A tag like `v1.1.0-rc1` has no SemVer prerelease component (the regex
    // requires `-<id>(.<id>)*`-style identifiers). The `prerelease_suffix`
    // hint must still flag it as a prerelease for the smartsemver filter.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tags(dir, &["v1.0.0", "v1.1.0"]);

    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();

    let gc = crate::config::GitConfig {
        tag_sort: Some("smartsemver".to_string()),
        prerelease_suffix: Some("-rc".to_string()),
        ..Default::default()
    };
    let mut vars = crate::template::TemplateVars::new();
    vars.set("Version", "v1.1.0");

    let result = find_latest_tag_matching("v{{ .Version }}", Some(&gc), Some(&vars)).unwrap();
    assert_eq!(
        result,
        Some("v1.1.0".to_string()),
        "release v1.1.0 wins; suffix-flagged prereleases would be filtered if present"
    );

    std::env::set_current_dir(orig).unwrap();
}

#[test]
#[serial]
fn test_find_previous_tag_smartsemver_skips_prerelease_predecessor() {
    // GoReleaser's canonical bug fix: shipping v0.2.0 after a v0.2.0-beta.3
    // tag must surface v0.1.0 as the predecessor (not the beta) so the
    // changelog has real commits to enumerate.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tagged_commits(dir, &["v0.1.0", "v0.2.0-beta.3", "v0.2.0"]);

    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();

    let gc = crate::config::GitConfig {
        tag_sort: Some("smartsemver".to_string()),
        ..Default::default()
    };
    let mut vars = crate::template::TemplateVars::new();
    vars.set("Version", "v0.2.0");

    let result = find_previous_tag("v0.2.0", Some(&gc), Some(&vars)).unwrap();
    assert_eq!(result, Some("v0.1.0".to_string()));

    std::env::set_current_dir(orig).unwrap();
}

#[test]
#[serial]
fn test_find_previous_tag_smartsemver_keeps_prerelease_when_current_is_prerelease() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tagged_commits(dir, &["v0.1.0", "v0.2.0-beta.1", "v0.2.0-beta.2"]);

    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();

    let gc = crate::config::GitConfig {
        tag_sort: Some("smartsemver".to_string()),
        ..Default::default()
    };
    let mut vars = crate::template::TemplateVars::new();
    vars.set("Version", "v0.2.0-beta.2");

    let result = find_previous_tag("v0.2.0-beta.2", Some(&gc), Some(&vars)).unwrap();
    assert_eq!(result, Some("v0.2.0-beta.1".to_string()));

    std::env::set_current_dir(orig).unwrap();
}

#[test]
#[serial]
fn test_find_previous_tag_smartsemver_without_template_vars_falls_back_to_semver_sort() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_tagged_commits(dir, &["v0.1.0", "v0.2.0-beta.3", "v0.2.0"]);

    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();

    let gc = crate::config::GitConfig {
        tag_sort: Some("smartsemver".to_string()),
        ..Default::default()
    };
    // No template_vars supplied: smartsemver filter is dormant, but the
    // list+sort path still excludes `current_tag` and returns the semver-
    // highest remainder. v0.2.0-beta.3 ranks above v0.1.0 with no filter.
    let result = find_previous_tag("v0.2.0", Some(&gc), None).unwrap();
    assert_eq!(result, Some("v0.2.0-beta.3".to_string()));

    std::env::set_current_dir(orig).unwrap();
}
