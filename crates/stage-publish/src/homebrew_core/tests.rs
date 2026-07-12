//! Tests for the homebrew-core bump publisher: pure formula rewrite,
//! config derivation defaults, the fork+PR / direct-commit run paths driven
//! against an in-process scripted GitHub API, preflight, rollback decode,
//! and the single-crate / lockstep / per-crate config-mode axis.

use std::sync::{Arc, Mutex};

use anodizer_core::config::{
    CrateConfig, HomebrewCoreConfig, ReleaseConfig, RepositoryConfig, ScmRepoConfig, StringOrBool,
};
use anodizer_core::test_helpers::TestContextBuilder;
use anodizer_core::test_helpers::scripted_responder::{
    RequestLog, ScriptedRoute, spawn_scripted_responder,
};
use anodizer_core::{PreflightCheck, Publisher};
use base64::Engine as _;

use super::formula::{
    FormulaRewrite, flat_formula_path, formula_is_current, rewrite_formula, sharded_formula_path,
};
use super::publisher::{
    HomebrewCorePublisher, TOKEN_ENV_VARS, bump_branch, publish_to_homebrew_core,
    resolve_commit_message, resolve_download_url, resolve_formula_name, resolve_token,
    resolve_upstream,
};

const OLD_URL: &str = "https://github.com/acme/my-tool/archive/refs/tags/v1.0.0.tar.gz";
const NEW_URL: &str = "https://github.com/acme/my-tool/archive/refs/tags/v1.2.3.tar.gz";

fn old_sha() -> String {
    "11".repeat(32)
}
fn new_sha() -> String {
    "22".repeat(32)
}

// -----------------------------------------------------------------------------
// Formula fixtures
// -----------------------------------------------------------------------------

/// Archive-form formula with an explicit `version` stanza and a `bottle`
/// block whose keyed `sha256` lines must survive the rewrite untouched.
fn archive_formula() -> String {
    format!(
        r#"class MyTool < Formula
  desc "A tool"
  homepage "https://example.com"
  url "{OLD_URL}"
  sha256 "{old}"
  license "MIT"
  version "1.0.0"

  bottle do
    sha256 cellar: :any_skip_relocation, arm64_sonoma: "{bottle_a}"
    sha256 arm64_ventura: "{bottle_b}"
  end

  def install
    bin.install "my-tool"
  end
end
"#,
        old = old_sha(),
        bottle_a = "aa".repeat(32),
        bottle_b = "bb".repeat(32),
    )
}

/// Archive-form formula with NO explicit `version` stanza (Homebrew derives
/// the version from the url).
fn archive_formula_no_version() -> String {
    format!(
        r#"class MyTool < Formula
  desc "A tool"
  homepage "https://example.com"
  url "{OLD_URL}"
  sha256 "{old}"

  def install
    bin.install "my-tool"
  end
end
"#,
        old = old_sha(),
    )
}

/// Git-form formula: `url ..., tag:, revision:`. Carries a standalone
/// `sha256` too so we can prove it is left alone once the tag form is
/// detected (a git-based bump only moves `tag:`/`revision:`).
fn git_formula() -> String {
    format!(
        r#"class MyTool < Formula
  desc "A tool"
  homepage "https://example.com"
  url "https://github.com/acme/my-tool.git",
      tag: "v1.0.0",
      revision: "{old_rev}"
  sha256 "{old}"

  def install
    bin.install "my-tool"
  end
end
"#,
        old_rev = "0".repeat(40),
        old = old_sha(),
    )
}

fn rw(sha256: Option<String>, tag: Option<&str>, revision: Option<&str>) -> FormulaRewrite {
    FormulaRewrite {
        url: NEW_URL.to_string(),
        sha256,
        version: "1.2.3".to_string(),
        tag: tag.map(str::to_string),
        revision: revision.map(str::to_string),
    }
}

// =============================================================================
// formula.rs — pure rewrite (no HTTP)
// =============================================================================

#[test]
fn rewrite_archive_form_bumps_url_sha256_and_version() {
    let (out, summary) =
        rewrite_formula(&archive_formula(), &rw(Some(new_sha()), None, None)).expect("rewrite");
    assert!(out.contains(&format!("url \"{NEW_URL}\"")), "{out}");
    assert!(out.contains(&format!("sha256 \"{}\"", new_sha())), "{out}");
    assert!(out.contains("version \"1.2.3\""), "{out}");
    assert!(summary.url_rewritten);
    assert!(summary.sha256_rewritten);
    assert!(summary.version_rewritten);
    assert!(!summary.tag_rewritten);
}

#[test]
fn rewrite_archive_form_leaves_bottle_sha256_lines_untouched() {
    let (out, _) =
        rewrite_formula(&archive_formula(), &rw(Some(new_sha()), None, None)).expect("rewrite");
    // The bottle block's keyed digests carry a key before the digest and are
    // structurally different from the source `sha256 "..."` stanza.
    assert!(
        out.contains(&format!("arm64_sonoma: \"{}\"", "aa".repeat(32))),
        "bottle arm64_sonoma digest must survive: {out}"
    );
    assert!(
        out.contains(&format!("arm64_ventura: \"{}\"", "bb".repeat(32))),
        "bottle arm64_ventura digest must survive: {out}"
    );
    // Only ONE standalone source digest is rewritten.
    assert_eq!(out.matches(&new_sha()).count(), 1, "{out}");
}

#[test]
fn rewrite_git_form_bumps_tag_and_revision_and_leaves_sha256_alone() {
    let new_rev = "f".repeat(40);
    // sha256 is supplied but must be ignored: once the tag form is detected
    // the source-digest rewrite is skipped (git formulae have no source sha).
    let (out, summary) = rewrite_formula(
        &git_formula(),
        &FormulaRewrite {
            url: "https://github.com/acme/my-tool.git".to_string(),
            sha256: Some(new_sha()),
            version: "1.2.3".to_string(),
            tag: Some("v1.2.3".to_string()),
            revision: Some(new_rev.clone()),
        },
    )
    .expect("rewrite");
    assert!(out.contains("tag: \"v1.2.3\""), "tag rewritten: {out}");
    assert!(
        out.contains(&format!("revision: \"{new_rev}\"")),
        "revision rewritten: {out}"
    );
    assert!(summary.tag_rewritten);
    assert!(summary.revision_rewritten);
    assert!(!summary.sha256_rewritten, "git form must not touch sha256");
    // The original source sha256 stanza is preserved verbatim.
    assert!(
        out.contains(&format!("sha256 \"{}\"", old_sha())),
        "source sha256 must be left alone in git form: {out}"
    );
    assert!(!out.contains(&new_sha()), "{out}");
}

#[test]
fn rewrite_archive_form_without_version_stanza_is_ok() {
    let (out, summary) = rewrite_formula(
        &archive_formula_no_version(),
        &rw(Some(new_sha()), None, None),
    )
    .expect("rewrite");
    assert!(summary.url_rewritten);
    assert!(summary.sha256_rewritten);
    assert!(!summary.version_rewritten, "no version stanza to rewrite");
    assert!(!out.contains("version \""), "{out}");
}

#[test]
fn rewrite_errors_when_no_url_stanza() {
    let text = "class MyTool < Formula\n  sha256 \"abc\"\nend\n";
    let err = rewrite_formula(text, &rw(Some(new_sha()), None, None)).unwrap_err();
    assert!(err.to_string().contains("url"), "{err:#}");
}

#[test]
fn rewrite_errors_when_archive_form_missing_sha256_digest() {
    // Archive-form formula has a source `sha256` stanza but the caller
    // supplied no new digest — a hard error, never a silent no-op.
    let err = rewrite_formula(&archive_formula(), &rw(None, None, None)).unwrap_err();
    assert!(err.to_string().contains("sha256"), "{err:#}");
}

#[test]
fn rewrite_preserves_trailing_newline_presence() {
    let with_nl = archive_formula();
    assert!(with_nl.ends_with('\n'));
    let (out, _) = rewrite_formula(&with_nl, &rw(Some(new_sha()), None, None)).expect("rewrite");
    assert!(out.ends_with('\n'), "trailing newline preserved");

    let without_nl = with_nl.trim_end_matches('\n').to_string();
    let (out, _) = rewrite_formula(&without_nl, &rw(Some(new_sha()), None, None)).expect("rewrite");
    assert!(!out.ends_with('\n'), "absent trailing newline stays absent");
}

// -----------------------------------------------------------------------------
// formula path layout
// -----------------------------------------------------------------------------

#[test]
fn sharded_and_flat_paths() {
    assert_eq!(sharded_formula_path("my-tool"), "Formula/m/my-tool.rb");
    assert_eq!(flat_formula_path("my-tool"), "Formula/my-tool.rb");
    // Digit-named formula shards under its digit.
    assert_eq!(sharded_formula_path("7zip"), "Formula/7/7zip.rb");
    // Shard char is lowercased.
    assert_eq!(sharded_formula_path("Zsh"), "Formula/z/Zsh.rb");
}

// -----------------------------------------------------------------------------
// formula_is_current — idempotency
// -----------------------------------------------------------------------------

#[test]
fn formula_is_current_matches_url_tag_or_version() {
    let archive = archive_formula();
    // url already at the new release.
    assert!(formula_is_current(&archive, OLD_URL, None, "9.9.9"));
    // version stanza carries the queried version.
    assert!(formula_is_current(
        &archive,
        "https://x/none",
        None,
        "1.0.0"
    ));
    // tag match (git form).
    assert!(formula_is_current(
        &git_formula(),
        "https://x/none",
        Some("v1.0.0"),
        "9.9.9"
    ));
    // Negative: nothing matches the new release.
    assert!(!formula_is_current(
        &archive,
        NEW_URL,
        Some("v1.2.3"),
        "1.2.3"
    ));
}

// =============================================================================
// Config derivation defaults
// =============================================================================

fn demo_crate(name: &str, path: &str) -> CrateConfig {
    CrateConfig {
        name: name.to_string(),
        path: path.to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        ..Default::default()
    }
}

fn crate_with_github(name: &str, owner: &str, repo: &str) -> CrateConfig {
    CrateConfig {
        release: Some(ReleaseConfig {
            github: Some(ScmRepoConfig {
                owner: owner.to_string(),
                name: repo.to_string(),
                token: None,
            }),
            ..Default::default()
        }),
        ..demo_crate(name, ".")
    }
}

fn ctx_with(crates: Vec<CrateConfig>) -> anodizer_core::context::Context {
    TestContextBuilder::new()
        .project_name("demo-project")
        .tag("v1.2.3")
        .crates(crates)
        .build()
}

#[test]
fn resolve_formula_name_defaults_to_primary_crate_then_project() {
    // Explicit name (templated) wins.
    let ctx = ctx_with(vec![demo_crate("core", ".")]);
    let cfg = HomebrewCoreConfig {
        name: Some("{{ .ProjectName }}-cli".into()),
        ..Default::default()
    };
    assert_eq!(
        resolve_formula_name(&ctx, &cfg).unwrap(),
        "demo-project-cli"
    );

    // Primary crate name when name/ids unset.
    assert_eq!(
        resolve_formula_name(&ctx, &HomebrewCoreConfig::default()).unwrap(),
        "core"
    );

    // Project name when there are no crates at all.
    let bare = TestContextBuilder::new()
        .project_name("bare")
        .tag("v1.2.3")
        .build();
    assert_eq!(
        resolve_formula_name(&bare, &HomebrewCoreConfig::default()).unwrap(),
        "bare"
    );
}

#[test]
fn resolve_upstream_defaults_to_homebrew_core() {
    assert_eq!(
        resolve_upstream(&HomebrewCoreConfig::default()),
        ("Homebrew".to_string(), "homebrew-core".to_string())
    );
    let cfg = HomebrewCoreConfig {
        repository: Some(RepositoryConfig {
            owner: Some("myorg".into()),
            name: Some("homebrew-taps".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(
        resolve_upstream(&cfg),
        ("myorg".to_string(), "homebrew-taps".to_string())
    );
}

#[test]
fn resolve_download_url_defaults_to_github_source_tarball() {
    let ctx = ctx_with(vec![crate_with_github("core", "acme", "widget")]);
    assert_eq!(
        resolve_download_url(&ctx, &HomebrewCoreConfig::default()).unwrap(),
        "https://github.com/acme/widget/archive/refs/tags/v1.2.3.tar.gz"
    );
    // Explicit templated override wins.
    let cfg = HomebrewCoreConfig {
        download_url: Some("https://cdn.example.com/{{ .Version }}.tar.gz".into()),
        ..Default::default()
    };
    assert_eq!(
        resolve_download_url(&ctx, &cfg).unwrap(),
        "https://cdn.example.com/1.2.3.tar.gz"
    );
}

#[test]
fn resolve_commit_message_default_and_template() {
    let ctx = ctx_with(vec![demo_crate("core", ".")]);
    assert_eq!(
        resolve_commit_message(&ctx, &HomebrewCoreConfig::default(), "my-tool", "1.2.3").unwrap(),
        "my-tool 1.2.3"
    );
    let cfg = HomebrewCoreConfig {
        commit_msg_template: Some("bump {{ .Version }}".into()),
        ..Default::default()
    };
    assert_eq!(
        resolve_commit_message(&ctx, &cfg, "my-tool", "1.2.3").unwrap(),
        "bump 1.2.3"
    );
}

#[test]
fn bump_branch_names_formula_and_version() {
    assert_eq!(bump_branch("my-tool", "1.2.3"), "bump-my-tool-1.2.3");
}

#[test]
fn resolve_token_ladder() {
    // repository.token wins over the env ladder.
    let ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .env("HOMEBREW_CORE_GITHUB_TOKEN", "hc-tok")
        .build();
    let cfg = HomebrewCoreConfig {
        repository: Some(RepositoryConfig {
            token: Some("cfg-tok".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(resolve_token(&ctx, &cfg).as_deref(), Some("cfg-tok"));

    // HOMEBREW_CORE_GITHUB_TOKEN precedes COMMITTER_TOKEN + GITHUB ladder.
    assert_eq!(
        resolve_token(&ctx, &HomebrewCoreConfig::default()).as_deref(),
        Some("hc-tok")
    );

    // COMMITTER_TOKEN (mislav/bump-homebrew-formula-action's name) is next.
    let ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .env("COMMITTER_TOKEN", "committer-tok")
        .build();
    assert_eq!(
        resolve_token(&ctx, &HomebrewCoreConfig::default()).as_deref(),
        Some("committer-tok")
    );

    // Standard GitHub ladder is the final fallback.
    let ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .env("GITHUB_TOKEN", "gh-tok")
        .build();
    assert_eq!(
        resolve_token(&ctx, &HomebrewCoreConfig::default()).as_deref(),
        Some("gh-tok")
    );

    // Empty env values are filtered (a blank secret is not a token).
    let ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .env("HOMEBREW_CORE_GITHUB_TOKEN", "")
        .env("GITHUB_TOKEN", "")
        .build();
    assert!(resolve_token(&ctx, &HomebrewCoreConfig::default()).is_none());
}

// =============================================================================
// run() — end-to-end against a scripted GitHub API
// =============================================================================

fn leak_resp(status: &str, body: &str) -> &'static str {
    Box::leak(
        format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_boxed_str(),
    )
}

/// A `GET /contents` response carrying `content` base64-encoded (as the
/// GitHub contents API returns it) plus a stable blob sha.
fn contents_resp(content: &str) -> &'static str {
    let b64 = base64::engine::general_purpose::STANDARD.encode(content);
    let body = format!("{{\"sha\":\"blob123\",\"content\":\"{b64}\"}}");
    leak_resp("200 OK", &body)
}

fn repo_resp(default_branch: &str, can_push: bool) -> &'static str {
    let body = format!(
        "{{\"default_branch\":\"{default_branch}\",\"permissions\":{{\"push\":{can_push}}}}}"
    );
    leak_resp("200 OK", &body)
}

/// Build a context whose env source points the GitHub API base at the
/// scripted responder and carries a bump token.
fn run_ctx(
    addr: &std::net::SocketAddr,
    crates: Vec<CrateConfig>,
    cfg: HomebrewCoreConfig,
) -> anodizer_core::context::Context {
    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .crates(crates)
        .env("ANODIZER_GITHUB_API_BASE", format!("http://{addr}"))
        .env("HOMEBREW_CORE_GITHUB_TOKEN", "ghp_test")
        .build();
    ctx.config.homebrew_cores = Some(vec![cfg]);
    ctx
}

/// A homebrew-core entry with the download URL + sha256 pinned so the run
/// path never touches the network for the digest.
fn pinned_cfg() -> HomebrewCoreConfig {
    HomebrewCoreConfig {
        name: Some("my-tool".into()),
        download_url: Some(NEW_URL.into()),
        sha256: Some(new_sha()),
        ..Default::default()
    }
}

fn logged(log: &Arc<Mutex<Vec<RequestLog>>>) -> Vec<RequestLog> {
    log.lock().unwrap().clone()
}

fn find<'a>(reqs: &'a [RequestLog], method: &str, path: &str) -> Option<&'a RequestLog> {
    reqs.iter().find(|r| r.method == method && r.path == path)
}

/// The full fork+PR route set for a `Homebrew/homebrew-core` bump.
fn core_fork_pr_routes() -> Vec<ScriptedRoute> {
    vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/Homebrew/homebrew-core",
            response: repo_resp("master", false),
            times: None,
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/Homebrew/homebrew-core/contents/Formula/m/my-tool.rb?ref=master",
            response: contents_resp(&archive_formula()),
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/Homebrew/homebrew-core/forks",
            response: leak_resp("202 Accepted", "{\"owner\":{\"login\":\"forkuser\"}}"),
            times: None,
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/Homebrew/homebrew-core/pulls?state=open&head=forkuser:bump-my-tool-1.2.3&per_page=100",
            response: leak_resp("200 OK", "[]"),
            times: None,
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/Homebrew/homebrew-core/git/ref/heads/master",
            response: leak_resp("200 OK", "{\"object\":{\"sha\":\"base123\"}}"),
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/forkuser/homebrew-core/git/refs",
            response: leak_resp("201 Created", "{}"),
            times: None,
        },
        ScriptedRoute {
            method: "PUT",
            path_pattern: "/repos/forkuser/homebrew-core/contents/Formula/m/my-tool.rb",
            response: leak_resp("200 OK", "{}"),
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/Homebrew/homebrew-core/pulls",
            response: leak_resp(
                "201 Created",
                "{\"number\":42,\"html_url\":\"https://github.com/Homebrew/homebrew-core/pull/42\"}",
            ),
            times: None,
        },
    ]
}

#[test]
fn run_fork_pr_happy_path_bumps_core_formula() {
    let (addr, log) = spawn_scripted_responder(core_fork_pr_routes());
    let ctx = run_ctx(&addr, vec![demo_crate("my-tool", ".")], pinned_cfg());
    let mut targets = Vec::new();
    publish_to_homebrew_core(&ctx, &ctx.logger("publish"), &mut targets).expect("publish");

    assert_eq!(targets.len(), 1);
    let t = &targets[0];
    assert_eq!(t.formula, "my-tool");
    assert_eq!(t.version, "1.2.3");
    assert_eq!(t.head_owner, "forkuser");
    assert_eq!(t.branch, "bump-my-tool-1.2.3");
    assert!(!t.direct_commit);
    assert_eq!(
        t.pr_url.as_deref(),
        Some("https://github.com/Homebrew/homebrew-core/pull/42")
    );

    let reqs = logged(&log);
    // The created branch references the base sha.
    let refs = find(&reqs, "POST", "/repos/forkuser/homebrew-core/git/refs").expect("refs POST");
    assert!(
        refs.body.contains("refs/heads/bump-my-tool-1.2.3"),
        "{}",
        refs.body
    );
    assert!(refs.body.contains("base123"), "{}", refs.body);

    // The committed formula carries the rewritten url + sha256.
    let put = find(
        &reqs,
        "PUT",
        "/repos/forkuser/homebrew-core/contents/Formula/m/my-tool.rb",
    )
    .expect("PUT contents");
    let v: serde_json::Value = serde_json::from_str(&put.body).expect("put json");
    let committed = String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(v["content"].as_str().unwrap())
            .unwrap(),
    )
    .unwrap();
    assert!(
        committed.contains(&format!("url \"{NEW_URL}\"")),
        "{committed}"
    );
    assert!(
        committed.contains(&format!("sha256 \"{}\"", new_sha())),
        "{committed}"
    );

    // The PR is opened against the upstream base with the fork head.
    let pr = find(&reqs, "POST", "/repos/Homebrew/homebrew-core/pulls").expect("PR POST");
    assert!(pr.body.contains("\"base\":\"master\""), "{}", pr.body);
    assert!(
        pr.body.contains("\"head\":\"forkuser:bump-my-tool-1.2.3\""),
        "{}",
        pr.body
    );
    assert!(
        pr.body.contains("\"title\":\"my-tool 1.2.3\""),
        "{}",
        pr.body
    );
    assert!(
        pr.body.contains("Bump"),
        "PR body names the bump: {}",
        pr.body
    );
}

#[test]
fn run_locates_flat_path_when_sharded_absent_and_can_push_uses_same_repo_branch() {
    // Personal formula repo the token can push to: no fork, a same-repo bump
    // branch, and the formula lives at the flat `Formula/<name>.rb` layout.
    let routes = vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/myorg/tap",
            response: repo_resp("main", true),
            times: None,
        },
        // Sharded path is unregistered → 404 → the probe falls to flat.
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/myorg/tap/contents/Formula/my-tool.rb?ref=main",
            response: contents_resp(&archive_formula()),
            times: None,
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/myorg/tap/pulls?state=open&head=myorg:bump-my-tool-1.2.3&per_page=100",
            response: leak_resp("200 OK", "[]"),
            times: None,
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/myorg/tap/git/ref/heads/main",
            response: leak_resp("200 OK", "{\"object\":{\"sha\":\"b\"}}"),
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/myorg/tap/git/refs",
            response: leak_resp("201 Created", "{}"),
            times: None,
        },
        ScriptedRoute {
            method: "PUT",
            path_pattern: "/repos/myorg/tap/contents/Formula/my-tool.rb",
            response: leak_resp("200 OK", "{}"),
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/myorg/tap/pulls",
            response: leak_resp("201 Created", "{\"number\":7,\"html_url\":\"https://x/7\"}"),
            times: None,
        },
    ];
    let (addr, log) = spawn_scripted_responder(routes);
    let mut cfg = pinned_cfg();
    cfg.repository = Some(RepositoryConfig {
        owner: Some("myorg".into()),
        name: Some("tap".into()),
        ..Default::default()
    });
    let ctx = run_ctx(&addr, vec![demo_crate("my-tool", ".")], cfg);
    let mut targets = Vec::new();
    publish_to_homebrew_core(&ctx, &ctx.logger("publish"), &mut targets).expect("publish");

    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].head_owner, "myorg", "same-repo branch, no fork");
    let reqs = logged(&log);
    // The flat path was committed to; the same-repo PR head has no owner prefix.
    assert!(find(&reqs, "PUT", "/repos/myorg/tap/contents/Formula/my-tool.rb").is_some());
    let pr = find(&reqs, "POST", "/repos/myorg/tap/pulls").expect("PR");
    assert!(
        pr.body.contains("\"head\":\"bump-my-tool-1.2.3\""),
        "{}",
        pr.body
    );
}

#[test]
fn run_direct_commit_to_personal_repo_when_can_push() {
    let routes = vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/myorg/tap",
            response: repo_resp("main", true),
            times: None,
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/myorg/tap/contents/Formula/m/my-tool.rb?ref=main",
            response: contents_resp(&archive_formula()),
            times: None,
        },
        ScriptedRoute {
            method: "PUT",
            path_pattern: "/repos/myorg/tap/contents/Formula/m/my-tool.rb",
            response: leak_resp("200 OK", "{}"),
            times: None,
        },
    ];
    let (addr, log) = spawn_scripted_responder(routes);
    let mut cfg = pinned_cfg();
    cfg.repository = Some(RepositoryConfig {
        owner: Some("myorg".into()),
        name: Some("tap".into()),
        ..Default::default()
    });
    cfg.direct_commit = Some(StringOrBool::Bool(true));
    let ctx = run_ctx(&addr, vec![demo_crate("my-tool", ".")], cfg);
    let mut targets = Vec::new();
    publish_to_homebrew_core(&ctx, &ctx.logger("publish"), &mut targets).expect("publish");

    assert_eq!(targets.len(), 1);
    assert!(targets[0].direct_commit, "direct commit recorded");
    assert!(targets[0].pr_url.is_none(), "no PR opened on direct commit");
    let reqs = logged(&log);
    assert_eq!(reqs.len(), 3, "repo + locate + commit only; no PR");
    let put = find(
        &reqs,
        "PUT",
        "/repos/myorg/tap/contents/Formula/m/my-tool.rb",
    )
    .expect("PUT");
    assert!(
        put.body.contains("\"branch\":\"main\""),
        "commit to base branch: {}",
        put.body
    );
}

#[test]
fn run_direct_commit_against_core_is_forced_to_fork_pr() {
    // `direct_commit: true` is ignored for Homebrew/homebrew-core: it never
    // accepts direct pushes, so the bump still forks + PRs.
    let (addr, log) = spawn_scripted_responder(core_fork_pr_routes());
    let mut cfg = pinned_cfg();
    cfg.direct_commit = Some(StringOrBool::Bool(true));
    let ctx = run_ctx(&addr, vec![demo_crate("my-tool", ".")], cfg);
    let mut targets = Vec::new();
    publish_to_homebrew_core(&ctx, &ctx.logger("publish"), &mut targets).expect("publish");

    assert_eq!(targets.len(), 1);
    assert!(!targets[0].direct_commit, "core is never a direct commit");
    assert!(targets[0].pr_url.is_some(), "core bump opens a PR");
    let reqs = logged(&log);
    assert!(find(&reqs, "POST", "/repos/Homebrew/homebrew-core/pulls").is_some());
}

#[test]
fn run_skips_when_open_pr_already_exists() {
    let mut routes = core_fork_pr_routes();
    // The find-open-PR probe returns a live PR from an earlier run.
    routes[3].response = leak_resp("200 OK", "[{\"number\":7}]");
    let (addr, log) = spawn_scripted_responder(routes);
    let ctx = run_ctx(&addr, vec![demo_crate("my-tool", ".")], pinned_cfg());
    let mut targets = Vec::new();
    publish_to_homebrew_core(&ctx, &ctx.logger("publish"), &mut targets).expect("publish");

    assert!(targets.is_empty(), "an open PR is an idempotent skip");
    let reqs = logged(&log);
    assert!(
        find(&reqs, "POST", "/repos/forkuser/homebrew-core/git/refs").is_none(),
        "no branch is created when a PR already exists"
    );
}

#[test]
fn run_skips_when_formula_already_current() {
    // The formula's url already points at the new release.
    let current = archive_formula().replace(OLD_URL, NEW_URL);
    let routes = vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/Homebrew/homebrew-core",
            response: repo_resp("master", false),
            times: None,
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/Homebrew/homebrew-core/contents/Formula/m/my-tool.rb?ref=master",
            response: contents_resp(&current),
            times: None,
        },
    ];
    let (addr, log) = spawn_scripted_responder(routes);
    let ctx = run_ctx(&addr, vec![demo_crate("my-tool", ".")], pinned_cfg());
    let mut targets = Vec::new();
    publish_to_homebrew_core(&ctx, &ctx.logger("publish"), &mut targets).expect("publish");

    assert!(targets.is_empty(), "already-current is an idempotent skip");
    assert_eq!(
        logged(&log).len(),
        2,
        "repo + locate only; no fork/branch/PR"
    );
}

#[test]
fn run_hard_errors_when_formula_not_found() {
    // Both sharded and flat probes 404 (unregistered).
    let routes = vec![ScriptedRoute {
        method: "GET",
        path_pattern: "/repos/Homebrew/homebrew-core",
        response: repo_resp("master", false),
        times: None,
    }];
    let (addr, _log) = spawn_scripted_responder(routes);
    let ctx = run_ctx(&addr, vec![demo_crate("my-tool", ".")], pinned_cfg());
    let mut targets = Vec::new();
    let err = publish_to_homebrew_core(&ctx, &ctx.logger("publish"), &mut targets).unwrap_err();
    assert!(err.to_string().contains("not found"), "{err:#}");
    assert!(targets.is_empty());
}

#[test]
fn run_dry_run_opens_no_pr() {
    let (addr, log) = spawn_scripted_responder(core_fork_pr_routes());
    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .crates(vec![demo_crate("my-tool", ".")])
        .dry_run(true)
        .env("ANODIZER_GITHUB_API_BASE", format!("http://{addr}"))
        .env("HOMEBREW_CORE_GITHUB_TOKEN", "ghp_test")
        .build();
    ctx.config.homebrew_cores = Some(vec![pinned_cfg()]);
    let mut targets = Vec::new();
    publish_to_homebrew_core(&ctx, &ctx.logger("publish"), &mut targets).expect("dry-run");
    assert!(targets.is_empty());
    assert_eq!(logged(&log).len(), 0, "dry-run makes no requests");
}

#[test]
fn run_hard_errors_when_token_missing() {
    // Env carries the API base but no token var → the run bails before any call.
    let ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .crates(vec![demo_crate("my-tool", ".")])
        .env("ANODIZER_GITHUB_API_BASE", "http://127.0.0.1:1")
        .build();
    let mut ctx = ctx;
    ctx.config.homebrew_cores = Some(vec![pinned_cfg()]);
    let mut targets = Vec::new();
    let err = publish_to_homebrew_core(&ctx, &ctx.logger("publish"), &mut targets).unwrap_err();
    assert!(err.to_string().contains("token is required"), "{err:#}");
}

// =============================================================================
// run() evidence + rollback decode
// =============================================================================

#[test]
fn run_builds_evidence_with_pr_ref_and_snapshot() {
    let (addr, _log) = spawn_scripted_responder(core_fork_pr_routes());
    let mut ctx = run_ctx(&addr, vec![demo_crate("my-tool", ".")], pinned_cfg());
    let evidence = HomebrewCorePublisher::new().run(&mut ctx).expect("run");
    assert_eq!(evidence.publisher, "homebrew-core");
    assert_eq!(
        evidence.primary_ref.as_deref(),
        Some("https://github.com/Homebrew/homebrew-core/pull/42")
    );
    match &evidence.extra {
        anodizer_core::PublishEvidenceExtra::HomebrewCore(e) => {
            assert_eq!(e.homebrew_core_targets.len(), 1);
            let t = &e.homebrew_core_targets[0];
            assert_eq!(t.head_owner, "forkuser");
            assert_eq!(t.branch, "bump-my-tool-1.2.3");
            assert_eq!(t.token_env_var.as_deref(), Some(TOKEN_ENV_VARS[0]));
        }
        other => panic!("expected HomebrewCore evidence, got {other:?}"),
    }
}

#[test]
fn rollback_decodes_evidence_and_closes_the_pr() {
    let routes = vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/Homebrew/homebrew-core/pulls?state=open&head=forkuser:bump-my-tool-1.2.3&per_page=100",
            response: leak_resp("200 OK", "[{\"number\":42}]"),
            times: None,
        },
        ScriptedRoute {
            method: "PATCH",
            path_pattern: "/repos/Homebrew/homebrew-core/pulls/42",
            response: leak_resp("200 OK", "{}"),
            times: None,
        },
    ];
    let (addr, log) = spawn_scripted_responder(routes);
    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .crates(vec![demo_crate("my-tool", ".")])
        .env("ANODIZER_GITHUB_API_BASE", format!("http://{addr}"))
        .env("HOMEBREW_CORE_GITHUB_TOKEN", "ghp_test")
        .build();

    let mut evidence = anodizer_core::PublishEvidence::new("homebrew-core");
    evidence.extra = anodizer_core::PublishEvidenceExtra::HomebrewCore(
        anodizer_core::publish_evidence::HomebrewCoreExtra {
            homebrew_core_targets: vec![
                anodizer_core::publish_evidence::HomebrewCoreTargetSnapshot {
                    formula: "my-tool".into(),
                    version: "1.2.3".into(),
                    upstream_owner: "Homebrew".into(),
                    upstream_repo: "homebrew-core".into(),
                    head_owner: "forkuser".into(),
                    branch: "bump-my-tool-1.2.3".into(),
                    direct_commit: false,
                    pr_url: Some("https://github.com/Homebrew/homebrew-core/pull/42".into()),
                    token_env_var: Some(TOKEN_ENV_VARS[0].into()),
                },
            ],
        },
    );

    HomebrewCorePublisher::new()
        .rollback(&mut ctx, &evidence)
        .expect("rollback");
    let reqs = logged(&log);
    assert!(
        find(&reqs, "PATCH", "/repos/Homebrew/homebrew-core/pulls/42").is_some(),
        "rollback must close the opened PR: {reqs:?}"
    );
}

#[test]
fn snapshot_serde_round_trips_without_token_value() {
    let snap = anodizer_core::publish_evidence::HomebrewCoreTargetSnapshot {
        formula: "my-tool".into(),
        version: "1.2.3".into(),
        upstream_owner: "Homebrew".into(),
        upstream_repo: "homebrew-core".into(),
        head_owner: "forkuser".into(),
        branch: "bump-my-tool-1.2.3".into(),
        direct_commit: false,
        pr_url: Some("https://x/42".into()),
        token_env_var: Some(TOKEN_ENV_VARS[0].into()),
    };
    let json = serde_json::to_string(&snap).unwrap();
    // The env-var NAME is carried; no token VALUE field exists on the type.
    assert!(json.contains("HOMEBREW_CORE_GITHUB_TOKEN"));
    assert!(!json.contains("\"token\":"), "no token value field: {json}");
    let back: anodizer_core::publish_evidence::HomebrewCoreTargetSnapshot =
        serde_json::from_str(&json).unwrap();
    assert_eq!(back, snap);
}

// =============================================================================
// preflight
// =============================================================================

fn preflight_ctx(
    addr: &std::net::SocketAddr,
    cfg: HomebrewCoreConfig,
) -> anodizer_core::context::Context {
    run_ctx(addr, vec![demo_crate("my-tool", ".")], cfg)
}

#[test]
fn preflight_passes_when_token_present_and_formula_exists() {
    let routes = vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/Homebrew/homebrew-core",
            response: repo_resp("master", false),
            times: None,
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/Homebrew/homebrew-core/contents/Formula/m/my-tool.rb?ref=master",
            response: contents_resp(&archive_formula()),
            times: None,
        },
    ];
    let (addr, _log) = spawn_scripted_responder(routes);
    let ctx = preflight_ctx(&addr, pinned_cfg());
    assert!(matches!(
        HomebrewCorePublisher::new()
            .preflight(&ctx)
            .expect("preflight"),
        PreflightCheck::Pass
    ));
}

#[test]
fn preflight_warns_when_formula_not_found() {
    let routes = vec![ScriptedRoute {
        method: "GET",
        path_pattern: "/repos/Homebrew/homebrew-core",
        response: repo_resp("master", false),
        times: None,
    }];
    let (addr, _log) = spawn_scripted_responder(routes);
    let ctx = preflight_ctx(&addr, pinned_cfg());
    match HomebrewCorePublisher::new()
        .preflight(&ctx)
        .expect("preflight")
    {
        PreflightCheck::Warning(m) => assert!(m.contains("not found"), "{m}"),
        other => panic!("expected Warning, got {other:?}"),
    }
}

#[test]
fn preflight_warns_when_version_already_current() {
    let current = archive_formula().replace(OLD_URL, NEW_URL);
    let routes = vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/Homebrew/homebrew-core",
            response: repo_resp("master", false),
            times: None,
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/Homebrew/homebrew-core/contents/Formula/m/my-tool.rb?ref=master",
            response: contents_resp(&current),
            times: None,
        },
    ];
    let (addr, _log) = spawn_scripted_responder(routes);
    let ctx = preflight_ctx(&addr, pinned_cfg());
    match HomebrewCorePublisher::new()
        .preflight(&ctx)
        .expect("preflight")
    {
        PreflightCheck::Warning(m) => assert!(m.contains("already at"), "{m}"),
        other => panic!("expected Warning, got {other:?}"),
    }
}

// =============================================================================
// Config-mode axis: single-crate, lockstep, per-crate `ids:` scoping
// =============================================================================

#[test]
fn config_mode_single_crate_derives_name_from_the_one_crate() {
    let ctx = ctx_with(vec![demo_crate("my-tool", ".")]);
    assert_eq!(
        resolve_formula_name(&ctx, &HomebrewCoreConfig::default()).unwrap(),
        "my-tool"
    );
}

#[test]
fn config_mode_lockstep_derives_name_from_primary_crate() {
    // Two lockstep crates, no `ids:`, no explicit name → the primary
    // (first) crate names the formula.
    let ctx = ctx_with(vec![demo_crate("core", "."), demo_crate("cli", "cli")]);
    assert_eq!(
        resolve_formula_name(&ctx, &HomebrewCoreConfig::default()).unwrap(),
        "core"
    );
}

#[test]
fn config_mode_per_crate_ids_scopes_name_and_download_url() {
    // Per-crate pattern: `ids: [cli]` selects the cli crate for both the
    // formula name and the default download URL's source repo.
    let ctx = ctx_with(vec![
        crate_with_github("core", "acme", "core-repo"),
        crate_with_github("cli", "acme", "cli-repo"),
    ]);
    let cfg = HomebrewCoreConfig {
        ids: Some(vec!["cli".into()]),
        ..Default::default()
    };
    assert_eq!(resolve_formula_name(&ctx, &cfg).unwrap(), "cli");
    assert_eq!(
        resolve_download_url(&ctx, &cfg).unwrap(),
        "https://github.com/acme/cli-repo/archive/refs/tags/v1.2.3.tar.gz"
    );
}
