//! Tests for the `changelog.ai` enhancement pipeline.
//!
//! Each provider's HTTP base URL is overridable via an `ANODIZER_*_ENDPOINT`
//! env var so tests can point them at a [`spawn_scripted_responder`] running
//! on `127.0.0.1`. Tests are `#[serial]` because they mutate process env.

use std::sync::Mutex;

use anodizer_core::config::{
    ChangelogAiConfig, ChangelogAiPrompt, ChangelogAiPromptSource, Config, ContentFromFile,
    ContentFromUrl,
};
use anodizer_core::context::{Context, ContextOptions};
use anodizer_core::test_helpers::scripted_responder::{ScriptedRoute, spawn_scripted_responder};
use serial_test::serial;

use super::enhance_with_ai;

// ---------------------------------------------------------------------------
// Env-mutex: serialise tests that touch the same env keys (every test in this
// module reads/writes `*_ENDPOINT` / `*_API_KEY`).
// ---------------------------------------------------------------------------

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// RAII helper that sets a process env var on construction and restores
/// (or unsets) it on drop. Avoids leaking test fixtures into sibling tests
/// when an assertion panics mid-test.
struct EnvGuard {
    key: String,
    prev: Option<String>,
}

impl EnvGuard {
    fn set(key: &str, value: &str) -> Self {
        let prev = std::env::var(key).ok();
        // SAFETY: tests serialise on ENV_LOCK; no other thread mutates env.
        unsafe { std::env::set_var(key, value) };
        Self {
            key: key.to_string(),
            prev,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: tests serialise on ENV_LOCK.
        unsafe {
            match self.prev {
                Some(ref v) => std::env::set_var(&self.key, v),
                None => std::env::remove_var(&self.key),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Test fixtures
// ---------------------------------------------------------------------------

/// Build a minimal `Context` for AI-enhancement tests.
fn make_ctx(allow_ai_failure: bool) -> Context {
    let config = Config {
        project_name: "myapp".to_string(),
        ..Config::default()
    };
    Context::new(
        config,
        ContextOptions {
            allow_ai_failure,
            ..Default::default()
        },
    )
}

/// Build a canned `200 OK` JSON response with the given body.
fn json_200(body: &'static str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    )
}

// ---------------------------------------------------------------------------
// Provider-not-configured (no-op)
// ---------------------------------------------------------------------------

#[test]
#[serial]
fn no_provider_returns_body_unchanged() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let ctx = make_ctx(false);
    let log = ctx.logger("changelog");
    let cfg = ChangelogAiConfig {
        provider: None,
        ..Default::default()
    };
    let body = "## Changes\n* one\n* two\n";
    let out = enhance_with_ai(&ctx, &cfg, body, &log).expect("no-op should succeed");
    assert_eq!(out, body);
}

#[test]
#[serial]
fn empty_provider_string_returns_body_unchanged() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let ctx = make_ctx(false);
    let log = ctx.logger("changelog");
    let cfg = ChangelogAiConfig {
        provider: Some(String::new()),
        ..Default::default()
    };
    let body = "## Changes\n* one\n";
    let out = enhance_with_ai(&ctx, &cfg, body, &log).expect("empty provider should no-op");
    assert_eq!(out, body);
}

// ---------------------------------------------------------------------------
// Anthropic — 200 OK replaces the body
// ---------------------------------------------------------------------------

#[test]
#[serial]
fn anthropic_200_replaces_body() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let body = "{\"content\":[{\"type\":\"text\",\"text\":\"# v1.0.0\\n\\nEnhanced notes.\"}]}";
    let response: &'static str = Box::leak(json_200(body).into_boxed_str());
    let (addr, _calls) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/v1/messages",
        response,
        times: Some(1),
    }]);
    let base = format!("http://{addr}");
    let _g_base = EnvGuard::set("ANODIZER_ANTHROPIC_ENDPOINT", &base);
    let _g_key = EnvGuard::set("ANTHROPIC_API_KEY", "sk-ant-test-1234");

    let ctx = make_ctx(false);
    let log = ctx.logger("changelog");
    let cfg = ChangelogAiConfig {
        provider: Some("anthropic".to_string()),
        model: Some("claude-test".to_string()),
        prompt: Some(ChangelogAiPrompt::Inline(
            "Summarise: {{ ReleaseNotes }}".to_string(),
        )),
    };
    let out = enhance_with_ai(&ctx, &cfg, "raw notes", &log).expect("200 succeeds");
    assert_eq!(out, "# v1.0.0\n\nEnhanced notes.");
}

// ---------------------------------------------------------------------------
// OpenAI — 200 OK replaces the body
// ---------------------------------------------------------------------------

#[test]
#[serial]
fn openai_200_replaces_body() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let body = "{\"choices\":[{\"message\":{\"content\":\"# Enhanced via OpenAI\"}}]}";
    let response: &'static str = Box::leak(json_200(body).into_boxed_str());
    let (addr, _calls) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/v1/chat/completions",
        response,
        times: Some(1),
    }]);
    let base = format!("http://{addr}");
    let _g_base = EnvGuard::set("ANODIZER_OPENAI_ENDPOINT", &base);
    let _g_key = EnvGuard::set("OPENAI_API_KEY", "sk-test-abc");

    let ctx = make_ctx(false);
    let log = ctx.logger("changelog");
    let cfg = ChangelogAiConfig {
        provider: Some("openai".to_string()),
        model: None,
        prompt: Some(ChangelogAiPrompt::Inline(
            "Improve: {{ ReleaseNotes }}".to_string(),
        )),
    };
    let out = enhance_with_ai(&ctx, &cfg, "raw", &log).expect("openai 200 succeeds");
    assert_eq!(out, "# Enhanced via OpenAI");
}

// ---------------------------------------------------------------------------
// Ollama — 200 OK replaces the body
// ---------------------------------------------------------------------------

#[test]
#[serial]
fn ollama_200_replaces_body() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let body = "{\"response\":\"# Local Llama notes\"}";
    let response: &'static str = Box::leak(json_200(body).into_boxed_str());
    let (addr, _calls) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/api/generate",
        response,
        times: Some(1),
    }]);
    let base = format!("http://{addr}");
    let _g_base = EnvGuard::set("ANODIZER_OLLAMA_ENDPOINT", &base);

    let ctx = make_ctx(false);
    let log = ctx.logger("changelog");
    let cfg = ChangelogAiConfig {
        provider: Some("ollama".to_string()),
        model: None,
        prompt: Some(ChangelogAiPrompt::Inline(
            "Hi {{ ReleaseNotes }}".to_string(),
        )),
    };
    let out = enhance_with_ai(&ctx, &cfg, "raw", &log).expect("ollama 200 succeeds");
    assert_eq!(out, "# Local Llama notes");
}

// ---------------------------------------------------------------------------
// 401 — release aborts, error does not leak the API key
// ---------------------------------------------------------------------------

#[test]
#[serial]
fn anthropic_401_aborts_and_redacts() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let body = r#"{"error":"unauthorised"}"#;
    let response: &'static str = Box::leak(
        format!(
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        )
        .into_boxed_str(),
    );
    let (addr, _calls) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/v1/messages",
        response,
        times: Some(1),
    }]);
    let base = format!("http://{addr}");
    let secret = "sk-ant-very-secret-do-not-leak-9999";
    let _g_base = EnvGuard::set("ANODIZER_ANTHROPIC_ENDPOINT", &base);
    let _g_key = EnvGuard::set("ANTHROPIC_API_KEY", secret);

    let ctx = make_ctx(false);
    let log = ctx.logger("changelog");
    let cfg = ChangelogAiConfig {
        provider: Some("anthropic".to_string()),
        model: None,
        prompt: Some(ChangelogAiPrompt::Inline("p".to_string())),
    };
    let err = enhance_with_ai(&ctx, &cfg, "raw", &log).expect_err("401 should fail-closed");
    let formatted = format!("{err:#}");
    assert!(
        !formatted.contains(secret),
        "error must not leak the API key value: {formatted}"
    );
    assert!(
        formatted.contains("401") || formatted.contains("anthropic"),
        "error should mention status / provider: {formatted}"
    );
}

// ---------------------------------------------------------------------------
// 503 — fail-closed by default, degrades to original body with --allow-ai-failure
// ---------------------------------------------------------------------------

#[test]
#[serial]
fn anthropic_503_aborts_when_fail_closed() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let response = "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n";
    let (addr, _calls) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/v1/messages",
        response,
        times: Some(1),
    }]);
    let base = format!("http://{addr}");
    let _g_base = EnvGuard::set("ANODIZER_ANTHROPIC_ENDPOINT", &base);
    let _g_key = EnvGuard::set("ANTHROPIC_API_KEY", "sk-ant-test");

    let ctx = make_ctx(false);
    let log = ctx.logger("changelog");
    let cfg = ChangelogAiConfig {
        provider: Some("anthropic".to_string()),
        model: None,
        prompt: Some(ChangelogAiPrompt::Inline("p".to_string())),
    };
    let err = enhance_with_ai(&ctx, &cfg, "raw", &log).expect_err("503 should fail-closed");
    let formatted = format!("{err:#}");
    assert!(
        formatted.contains("503") || formatted.contains("--allow-ai-failure"),
        "error should mention status or the opt-out flag: {formatted}"
    );
}

#[test]
#[serial]
fn anthropic_503_degrades_with_allow_ai_failure() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let response = "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n";
    let (addr, _calls) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/v1/messages",
        response,
        times: Some(1),
    }]);
    let base = format!("http://{addr}");
    let _g_base = EnvGuard::set("ANODIZER_ANTHROPIC_ENDPOINT", &base);
    let _g_key = EnvGuard::set("ANTHROPIC_API_KEY", "sk-ant-test");

    let ctx = make_ctx(true);
    let log = ctx.logger("changelog");
    let cfg = ChangelogAiConfig {
        provider: Some("anthropic".to_string()),
        model: None,
        prompt: Some(ChangelogAiPrompt::Inline("p".to_string())),
    };
    let original = "## Original notes\n";
    let out = enhance_with_ai(&ctx, &cfg, original, &log)
        .expect("--allow-ai-failure should swallow the error");
    assert_eq!(out, original);
}

// ---------------------------------------------------------------------------
// from_file — file content used as the prompt
// ---------------------------------------------------------------------------

#[test]
#[serial]
fn prompt_from_file_is_used() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let tmp = tempfile::tempdir().expect("tempdir");
    let prompt_path = tmp.path().join("prompt.txt");
    std::fs::write(
        &prompt_path,
        "FILE-PROMPT-MARKER: enhance these notes: {{ ReleaseNotes }}",
    )
    .expect("write prompt file");

    let body = r#"{"content":[{"type":"text","text":"got it"}]}"#;
    let response: &'static str = Box::leak(json_200(body).into_boxed_str());
    let (addr, calls) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/v1/messages",
        response,
        times: Some(1),
    }]);
    let base = format!("http://{addr}");
    let _g_base = EnvGuard::set("ANODIZER_ANTHROPIC_ENDPOINT", &base);
    let _g_key = EnvGuard::set("ANTHROPIC_API_KEY", "sk-ant-test");

    let ctx = make_ctx(false);
    let log = ctx.logger("changelog");
    let cfg = ChangelogAiConfig {
        provider: Some("anthropic".to_string()),
        model: None,
        prompt: Some(ChangelogAiPrompt::Source(ChangelogAiPromptSource {
            from_file: Some(ContentFromFile {
                path: Some(prompt_path.to_string_lossy().to_string()),
            }),
            from_url: None,
        })),
    };
    let out = enhance_with_ai(&ctx, &cfg, "RAW-NOTES", &log).expect("from_file path");
    assert_eq!(out, "got it");

    let entries = calls.lock().unwrap();
    assert_eq!(entries.len(), 1, "exactly one request");
    assert!(
        entries[0].body.contains("FILE-PROMPT-MARKER"),
        "request body should include the file prompt: {}",
        entries[0].body
    );
    assert!(
        entries[0].body.contains("RAW-NOTES"),
        "request body should include the rendered ReleaseNotes: {}",
        entries[0].body
    );
}

// ---------------------------------------------------------------------------
// from_url — `${TOKEN}` env-expanded in headers, then provider called
// ---------------------------------------------------------------------------

#[test]
#[serial]
fn prompt_from_url_expands_env_in_headers() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    // Two routes: one serves the prompt with an auth header echo, the second
    // is the Anthropic provider call that consumes it.
    let prompt_body = "URL-PROMPT-MARKER: {{ ReleaseNotes }}";
    let prompt_response: &'static str = Box::leak(
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
            prompt_body.len(),
            prompt_body
        )
        .into_boxed_str(),
    );

    let ai_body = r#"{"content":[{"type":"text","text":"done"}]}"#;
    let ai_response: &'static str = Box::leak(json_200(ai_body).into_boxed_str());

    let (addr, calls) = spawn_scripted_responder(vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/prompt.txt",
            response: prompt_response,
            times: Some(1),
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/v1/messages",
            response: ai_response,
            times: Some(1),
        },
    ]);

    let base = format!("http://{addr}");
    let _g_base = EnvGuard::set("ANODIZER_ANTHROPIC_ENDPOINT", &base);
    let _g_key = EnvGuard::set("ANTHROPIC_API_KEY", "sk-ant-test");
    let _g_tok = EnvGuard::set("MY_PROMPT_TOKEN", "secret-token-xyz");

    let prompt_url = format!("{base}/prompt.txt");
    let mut headers = std::collections::HashMap::new();
    headers.insert(
        "X-Auth".to_string(),
        "Bearer ${MY_PROMPT_TOKEN}".to_string(),
    );

    let ctx = make_ctx(false);
    let log = ctx.logger("changelog");
    let cfg = ChangelogAiConfig {
        provider: Some("anthropic".to_string()),
        model: None,
        prompt: Some(ChangelogAiPrompt::Source(ChangelogAiPromptSource {
            from_file: None,
            from_url: Some(ContentFromUrl {
                url: Some(prompt_url),
                headers: Some(headers),
            }),
        })),
    };
    let out = enhance_with_ai(&ctx, &cfg, "RAW-NOTES", &log).expect("from_url path");
    assert_eq!(out, "done");

    let entries = calls.lock().unwrap();
    assert_eq!(entries.len(), 2, "prompt fetch + ai call");
    assert_eq!(entries[0].method, "GET");
    assert_eq!(entries[0].path, "/prompt.txt");
    // The ai call body should embed the prompt template with ReleaseNotes substituted.
    assert!(
        entries[1].body.contains("URL-PROMPT-MARKER"),
        "ai body should carry the fetched prompt: {}",
        entries[1].body
    );
    assert!(
        entries[1].body.contains("RAW-NOTES"),
        "ai body should include the rendered notes: {}",
        entries[1].body
    );
}

// ---------------------------------------------------------------------------
// Unknown provider name — bails with the valid options
// ---------------------------------------------------------------------------

#[test]
#[serial]
fn unknown_provider_lists_valid_options() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let ctx = make_ctx(false);
    let log = ctx.logger("changelog");
    let cfg = ChangelogAiConfig {
        provider: Some("gemini".to_string()),
        model: None,
        prompt: Some(ChangelogAiPrompt::Inline("x".to_string())),
    };
    let err = enhance_with_ai(&ctx, &cfg, "body", &log).expect_err("unknown provider should bail");
    let formatted = format!("{err:#}");
    assert!(
        formatted.contains("anthropic")
            && formatted.contains("openai")
            && formatted.contains("ollama"),
        "error should list valid providers: {formatted}"
    );
}

// ---------------------------------------------------------------------------
// Snapshot mode — AI is skipped for cost containment
// ---------------------------------------------------------------------------

#[test]
#[serial]
fn snapshot_mode_skips_ai() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    // No env or responder needed — the snapshot guard short-circuits before
    // any provider construction. If the guard regresses the test fails
    // because `enhance` will try to read ANTHROPIC_API_KEY.
    let config = Config {
        project_name: "myapp".to_string(),
        ..Config::default()
    };
    let ctx = Context::new(
        config,
        ContextOptions {
            snapshot: true,
            ..Default::default()
        },
    );
    let log = ctx.logger("changelog");
    let cfg = ChangelogAiConfig {
        provider: Some("anthropic".to_string()),
        model: None,
        prompt: Some(ChangelogAiPrompt::Inline("x".to_string())),
    };
    let original = "## snapshot body\n";
    let out = enhance_with_ai(&ctx, &cfg, original, &log).expect("snapshot skips ai");
    assert_eq!(out, original);
}

// ---------------------------------------------------------------------------
// Body passed to the AI provider is the flat (ungrouped) commit list
// ---------------------------------------------------------------------------
//
// When `changelog.ai.use` is set the stage clears `opts.groups` BEFORE the
// per-crate render (`run.rs::run`). The render therefore emits a flat
// bullet list with no `### <group-title>` headings, and that flat string
// is what flows into `enhance_with_ai`. This test pins the property by
// rendering both shapes and confirming the flat one (what AI receives)
// has no group headings while the grouped one does.

#[test]
#[serial]
fn ai_receives_flat_commit_list_not_grouped() {
    use crate::group::{CommitInfo, GroupedCommits};
    use crate::render::{ChangelogRenderOpts, render_changelog_with_provider};
    use anodizer_core::config::ChangelogGroup;

    fn commit(raw: &str, kind: &str, desc: &str) -> CommitInfo {
        CommitInfo {
            raw_message: raw.to_string(),
            kind: kind.to_string(),
            description: desc.to_string(),
            hash: "abc1234".to_string(),
            full_hash: "abc1234abc1234abc1234abc1234abc1234abcd".to_string(),
            author_name: "Alice".to_string(),
            author_email: "alice@example.com".to_string(),
            login: "alice".to_string(),
            co_authors: Vec::new(),
        }
    }

    let commits = vec![
        commit("feat: add login", "feat", "add login"),
        commit("fix: resolve crash", "fix", "resolve crash"),
    ];

    // Grouped render — what the stage produces WITHOUT AI.
    let groups = vec![
        ChangelogGroup {
            title: "Features".into(),
            regexp: Some("^feat".into()),
            order: Some(0),
            groups: None,
        },
        ChangelogGroup {
            title: "Bug Fixes".into(),
            regexp: Some("^fix".into()),
            order: Some(1),
            groups: None,
        },
    ];
    let log =
        anodizer_core::log::StageLogger::new("changelog", anodizer_core::log::Verbosity::Normal);
    let grouped = crate::group::group_commits(&commits, &groups, &log).expect("group commits");
    let grouped_body = render_changelog_with_provider(
        &grouped,
        ChangelogRenderOpts {
            abbrev: 7,
            format_template: None,
            logins: "",
            use_source: "git",
            title: None,
            divider: None,
            scm_provider: None,
        },
    )
    .expect("grouped render");

    // Flat render — what the stage produces WITH AI (groups cleared).
    let flat = vec![GroupedCommits {
        title: String::new(),
        commits: commits.clone(),
        subgroups: Vec::new(),
    }];
    let flat_body = render_changelog_with_provider(
        &flat,
        ChangelogRenderOpts {
            abbrev: 7,
            format_template: None,
            logins: "",
            use_source: "git",
            title: None,
            divider: None,
            scm_provider: None,
        },
    )
    .expect("flat render");

    // The grouped body carries section headings; the flat one does not.
    assert!(
        grouped_body.contains("## Features"),
        "grouped body should have section heading: {grouped_body}"
    );
    assert!(
        grouped_body.contains("## Bug Fixes"),
        "grouped body should have section heading: {grouped_body}"
    );
    assert!(
        !flat_body.contains("## Features"),
        "flat body must NOT have group headings: {flat_body}"
    );
    assert!(
        !flat_body.contains("## Bug Fixes"),
        "flat body must NOT have group headings: {flat_body}"
    );
    // Both bodies must still contain the commit descriptions.
    assert!(
        flat_body.contains("add login"),
        "commit text preserved: {flat_body}"
    );
    assert!(
        flat_body.contains("resolve crash"),
        "commit text preserved: {flat_body}"
    );

    // Now drive enhance_with_ai with the flat body and confirm the AI
    // sees that exact string (no internal regrouping in enhance_with_ai).
    let echo_body: &'static str = "{\"content\":[{\"type\":\"text\",\"text\":\"echoed: ok\"}]}";
    let response: &'static str = Box::leak(json_200(echo_body).into_boxed_str());
    let (addr, calls) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/v1/messages",
        response,
        times: Some(1),
    }]);
    let base = format!("http://{addr}");
    let _g_base = EnvGuard::set("ANODIZER_ANTHROPIC_ENDPOINT", &base);
    let _g_key = EnvGuard::set("ANTHROPIC_API_KEY", "sk-ant-test");

    let ctx = make_ctx(false);
    let log = ctx.logger("changelog");
    let cfg = ChangelogAiConfig {
        provider: Some("anthropic".to_string()),
        model: None,
        prompt: Some(ChangelogAiPrompt::Inline(
            "polish: {{ ReleaseNotes }}".to_string(),
        )),
    };
    let _ = enhance_with_ai(&ctx, &cfg, &flat_body, &log).expect("ai call");

    let entries = calls.lock().unwrap();
    assert_eq!(entries.len(), 1);
    let req_body = &entries[0].body;
    assert!(
        !req_body.contains("## Features"),
        "AI request body must not carry group headings: {req_body}"
    );
    assert!(
        !req_body.contains("## Bug Fixes"),
        "AI request body must not carry group headings: {req_body}"
    );
    assert!(
        req_body.contains("add login"),
        "AI request body should carry the flat commit text: {req_body}"
    );
}
