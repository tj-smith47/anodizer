//! A crate's cask under `publish.homebrew_cask:` and under the
//! `publish.homebrew.cask:` sub-block: both register the publisher, render
//! and push the same cask.

use super::tests::{
    COMMAND_PASSES, RenderPass, art_with_url_sha, make_bare_cask_tap, run_render_pass,
};
use anodizer_core::artifact::ArtifactKind;
use anodizer_core::config::{
    Config, CrateConfig, GitRepoConfig, HomebrewCaskConfig, HomebrewConfig, PublishConfig,
    RepositoryConfig, StringOrBool, WorkspaceConfig,
};
use anodizer_core::context::{Context, ContextOptions};
use anodizer_core::log::{LogCapture, LogLevel, StageLogger};
use anodizer_core::{EnvRequirement, Publisher};
use std::path::Path;

fn tap_repository(bare_url: &str) -> RepositoryConfig {
    RepositoryConfig {
        owner: Some("myorg".to_string()),
        name: Some("homebrew-tap".to_string()),
        branch: Some("main".to_string()),
        git: Some(GitRepoConfig {
            url: Some(bare_url.to_string()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn cask(name: &str) -> HomebrewCaskConfig {
    HomebrewCaskConfig {
        name: Some(name.to_string()),
        description: Some("A tool".to_string()),
        homepage: Some("https://acme.example".to_string()),
        ..Default::default()
    }
}

/// The two spellings of one crate's cask, beside a formula on `bare_url`.
fn same_tap_publish(bare_url: &str, name: &str, legacy: bool) -> PublishConfig {
    let mut formula = HomebrewConfig {
        repository: Some(tap_repository(bare_url)),
        ..Default::default()
    };
    if legacy {
        formula.cask = Some(cask(name));
        PublishConfig {
            homebrew: Some(formula),
            ..Default::default()
        }
    } else {
        PublishConfig {
            homebrew: Some(formula),
            homebrew_cask: Some(cask(name)),
            ..Default::default()
        }
    }
}

/// A cask with no formula, pushed to its own `repository:`.
fn cask_only_publish(bare_url: &str, name: &str) -> PublishConfig {
    PublishConfig {
        homebrew_cask: Some(HomebrewCaskConfig {
            repository: Some(tap_repository(bare_url)),
            ..cask(name)
        }),
        ..Default::default()
    }
}

fn crate_with(name: &str, publish: PublishConfig) -> CrateConfig {
    CrateConfig {
        name: name.to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        publish: Some(publish),
        ..Default::default()
    }
}

/// A context over `config` at version 1.2.3, with a macOS archive for every
/// named crate.
fn release_ctx(config: Config, crate_names: &[&str]) -> Context {
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.2.3");
    ctx.template_vars_mut().set("RawVersion", "1.2.3");
    ctx.template_vars_mut().set("Tag", "v1.2.3");
    for name in crate_names {
        let mut a = art_with_url_sha(
            ArtifactKind::Archive,
            &format!("{name}-darwin.tar.gz"),
            "aarch64-apple-darwin",
            &format!("https://e.com/{name}-darwin.tar.gz"),
            "d".repeat(64).as_str(),
        );
        a.crate_name = name.to_string();
        a.metadata
            .insert("format".to_string(), "tar.gz".to_string());
        ctx.artifacts.add(a);
    }
    ctx
}

fn single_crate_ctx(publish: PublishConfig) -> Context {
    release_ctx(
        Config {
            crates: vec![crate_with("alpha", publish)],
            ..Default::default()
        },
        &["alpha"],
    )
}

fn homebrew_publisher(ctx: &Context) -> Box<dyn Publisher> {
    crate::registry::configured_publishers(ctx)
        .into_iter()
        .find(|p| p.name() == "homebrew")
        .expect("homebrew is registered")
}

fn tap_file(bare: &str, path: &str) -> String {
    anodizer_core::test_helpers::git_test_stdout(
        Path::new(bare),
        &["show", &format!("main:{path}")],
    )
}

fn tap_files(bare: &str) -> String {
    anodizer_core::test_helpers::git_test_stdout(
        Path::new(bare),
        &["ls-tree", "-r", "--name-only", "main"],
    )
}

/// Beside a formula, `homebrew_cask:` and `homebrew.cask:` push the same
/// files with the same content into the formula's tap.
#[test]
fn both_spellings_beside_a_formula_push_the_same_tap_commit() {
    let mut pushed = Vec::new();
    for legacy in [true, false] {
        let (bare_url, bare) = make_bare_cask_tap("main");
        let mut ctx = single_crate_ctx(same_tap_publish(&bare_url, "alpha", legacy));
        homebrew_publisher(&ctx)
            .run(&mut ctx)
            .expect("publish runs");
        pushed.push((
            tap_files(&bare_url),
            tap_file(&bare_url, "alpha.rb"),
            tap_file(&bare_url, "Casks/alpha.rb"),
        ));
        drop(bare);
    }
    assert_eq!(pushed[0], pushed[1], "both spellings push the same tap");
    let (files, _, cask_rb) = &pushed[0];
    assert_eq!(files, "Casks/alpha.rb\nREADME\nalpha.rb", "{files}");
    assert!(cask_rb.contains("cask \"alpha\" do"), "{cask_rb}");
}

/// A crate whose only homebrew config is `publish.homebrew_cask:` registers
/// the publisher, needs its tap's token, and pushes the same cask the
/// sub-block spelling pushes beside its formula.
#[test]
fn a_cask_only_crate_publishes_its_cask() {
    let (bare_url, bare) = make_bare_cask_tap("main");
    let mut ctx = single_crate_ctx(cask_only_publish(&bare_url, "alpha"));
    let publisher = homebrew_publisher(&ctx);
    assert!(!publisher.config_fully_inactive(&ctx));
    assert!(
        !publisher.requirements(&ctx).is_empty(),
        "the cask's tap needs a token"
    );
    publisher.run(&mut ctx).expect("publish runs");
    assert_eq!(tap_files(&bare_url), "Casks/alpha.rb\nREADME");

    let (legacy_url, legacy_bare) = make_bare_cask_tap("main");
    let mut legacy_ctx = single_crate_ctx(same_tap_publish(&legacy_url, "alpha", true));
    homebrew_publisher(&legacy_ctx)
        .run(&mut legacy_ctx)
        .expect("publish runs");
    assert_eq!(
        tap_file(&bare_url, "Casks/alpha.rb"),
        tap_file(&legacy_url, "Casks/alpha.rb")
    );
    drop((bare, legacy_bare));
}

/// A cask whose own `repository:` names another tap is pushed there, and
/// the formula's tap gets the formula alone.
#[test]
fn a_cask_with_its_own_tap_is_pushed_there() {
    let (formula_url, formula_bare) = make_bare_cask_tap("main");
    let (cask_url, cask_bare) = make_bare_cask_tap("main");
    let mut publish = same_tap_publish(&formula_url, "alpha", true);
    let mut own = cask("alpha");
    own.repository = Some(RepositoryConfig {
        name: Some("homebrew-casks".to_string()),
        ..tap_repository(&cask_url)
    });
    publish.homebrew.as_mut().unwrap().cask = Some(own);
    let mut ctx = single_crate_ctx(publish);
    homebrew_publisher(&ctx)
        .run(&mut ctx)
        .expect("publish runs");
    assert_eq!(tap_files(&formula_url), "README\nalpha.rb");
    assert_eq!(tap_files(&cask_url), "Casks/alpha.rb\nREADME");
    drop((formula_bare, cask_bare));
}

/// Lockstep (top-level `crates:`) and per-crate (`workspaces:`) configs
/// each publish every cask-only crate to its own tap.
#[test]
fn cask_only_crates_publish_in_lockstep_and_per_crate_configs() {
    for per_crate in [false, true] {
        let (alpha_url, alpha_bare) = make_bare_cask_tap("main");
        let (beta_url, beta_bare) = make_bare_cask_tap("main");
        let crates = vec![
            crate_with("alpha", cask_only_publish(&alpha_url, "alpha")),
            crate_with("beta", cask_only_publish(&beta_url, "beta")),
        ];
        let config = if per_crate {
            Config {
                workspaces: Some(vec![WorkspaceConfig {
                    name: "ws".to_string(),
                    crates,
                    ..Default::default()
                }]),
                ..Default::default()
            }
        } else {
            Config {
                crates,
                ..Default::default()
            }
        };
        let mut ctx = release_ctx(config, &["alpha", "beta"]);
        homebrew_publisher(&ctx)
            .run(&mut ctx)
            .expect("publish runs");
        for (url, name) in [(&alpha_url, "alpha"), (&beta_url, "beta")] {
            let rb = tap_file(url, &format!("Casks/{name}.rb"));
            assert!(rb.contains(&format!("cask \"{name}\" do")), "{rb}");
            assert!(
                rb.contains(&format!("https://e.com/{name}-darwin.tar.gz")),
                "{rb}"
            );
        }
        drop((alpha_bare, beta_bare));
    }
}

/// The render the schema validator and both push paths share gives the
/// same cask for either spelling, with or without a formula beside it.
#[test]
fn every_spelling_renders_the_same_cask() {
    let rendered: Vec<String> = [
        same_tap_publish("/unused", "alpha", true),
        same_tap_publish("/unused", "alpha", false),
        cask_only_publish("/unused", "alpha"),
    ]
    .into_iter()
    .map(|publish| {
        let ctx = single_crate_ctx(publish);
        super::render_homebrew_cask_for_crate(&ctx, "alpha", &ctx.logger("publish"))
            .expect("render ok")
            .expect("cask configured")
            .content
    })
    .collect();
    assert_eq!(rendered[0], rendered[1]);
    assert_eq!(rendered[0], rendered[2]);
    assert!(rendered[0].contains("version \"1.2.3\""), "{}", rendered[0]);
}

fn commit_count(bare: &str) -> String {
    anodizer_core::test_helpers::git_test_stdout(Path::new(bare), &["rev-list", "--count", "main"])
}

/// A tap repository on github.com with no `git.url`, so the preflight probe
/// runs, carrying a templated token whose env var names this repository in
/// the requirements.
fn github_tap(name: &str, token_var: &str) -> RepositoryConfig {
    RepositoryConfig {
        owner: Some("myorg".to_string()),
        name: Some(name.to_string()),
        branch: Some("main".to_string()),
        token: Some(format!("{{{{ .Env.{token_var} }}}}")),
        ..Default::default()
    }
}

/// A formula left out by its `skip_upload:` or `if:` does not take its
/// same-tap cask with it: the cask is pushed alone, in one commit, and the
/// tap gains no formula.
#[test]
fn a_gated_off_formula_still_pushes_its_same_tap_cask() {
    type Gate = fn(&mut HomebrewConfig, &mut HomebrewCaskConfig);
    let gates: [(&str, Gate); 2] = [
        ("skip_upload", |formula, cask| {
            formula.skip_upload = Some(StringOrBool::Bool(true));
            cask.skip_upload = Some(StringOrBool::Bool(false));
        }),
        ("if", |formula, cask| {
            formula.if_condition = Some("false".to_string());
            cask.if_condition = Some("true".to_string());
        }),
    ];
    for (gate, set) in gates {
        let (bare_url, bare) = make_bare_cask_tap("main");
        let mut publish = same_tap_publish(&bare_url, "alpha", false);
        set(
            publish.homebrew.as_mut().unwrap(),
            publish.homebrew_cask.as_mut().unwrap(),
        );
        let mut ctx = single_crate_ctx(publish);
        let publisher = homebrew_publisher(&ctx);
        assert!(!publisher.config_fully_inactive(&ctx), "{gate}");
        assert!(!publisher.requirements(&ctx).is_empty(), "{gate}");
        publisher.run(&mut ctx).expect("publish runs");
        assert_eq!(tap_files(&bare_url), "Casks/alpha.rb\nREADME", "{gate}");
        assert_eq!(
            commit_count(&bare_url),
            "2",
            "{gate}: one commit over the seed"
        );
        drop(bare);
    }
}

/// A formula and its same-tap cask that are both gated off push nothing, and
/// the run prints both skip lines, once each.
#[test]
fn a_gated_off_formula_and_cask_push_nothing_and_print_both_skips() {
    let (bare_url, bare) = make_bare_cask_tap("main");
    let mut publish = same_tap_publish(&bare_url, "alpha", false);
    publish.homebrew.as_mut().unwrap().skip_upload = Some(StringOrBool::Bool(true));
    let mut ctx = single_crate_ctx(publish);
    let cap = LogCapture::new();
    ctx.with_log_capture(cap.clone());
    let publisher = homebrew_publisher(&ctx);
    assert!(publisher.config_fully_inactive(&ctx));
    publisher.run(&mut ctx).expect("publish runs");
    assert_eq!(tap_files(&bare_url), "README");
    let skipped: Vec<String> = cap
        .printed_messages(LogLevel::Status)
        .into_iter()
        .filter(|m| m.starts_with("skipped homebrew"))
        .collect();
    assert_eq!(
        skipped,
        [
            "skipped homebrew for 'alpha' upload — skip_upload is set",
            "skipped homebrew cask for 'alpha' upload — skip_upload is set",
        ]
    );
    drop(bare);
}

/// Run every command's passes over a fresh context and count how often
/// `line` printed: once for a command that publishes, zero times for one
/// that only validates.
fn assert_skip_line_prints_once_per_command(
    site: &str,
    make_ctx: &dyn Fn(&str) -> Context,
    publish: &dyn Fn(&mut Context, &StageLogger),
    line: &str,
) {
    for (command, passes) in COMMAND_PASSES {
        let (bare_url, bare) = make_bare_cask_tap("main");
        let mut ctx = make_ctx(&bare_url);
        let cap = LogCapture::new();
        ctx.with_log_capture(cap.clone());
        for pass in *passes {
            run_render_pass(&mut ctx, *pass, publish);
        }
        let printed = cap
            .printed_messages(LogLevel::Status)
            .into_iter()
            .filter(|m| m == line)
            .count();
        let want = usize::from(passes.iter().any(|p| matches!(p, RenderPass::Publisher)));
        assert_eq!(printed, want, "{site}, {command}: {line}");
        drop(bare);
    }
}

/// The skip line of each gated homebrew document prints once per command,
/// from the publisher, for both gates: the same-tap cask beside a pushed
/// formula, a cask on its own tap, the formula, and a top-level cask.
#[test]
fn every_gate_skip_line_prints_once_per_command() {
    for gate in ["skip_upload", "if"] {
        let gated = |skip_upload: &mut Option<StringOrBool>, if_condition: &mut Option<String>| {
            if gate == "skip_upload" {
                *skip_upload = Some(StringOrBool::Bool(true));
            } else {
                *if_condition = Some("false".to_string());
            }
        };
        let skip_line = |label: &str| {
            if gate == "skip_upload" {
                format!("skipped {label} upload — skip_upload is set")
            } else {
                format!("skipped {label} — `if` condition evaluated falsy")
            }
        };

        assert_skip_line_prints_once_per_command(
            "same-tap cask",
            &|bare_url| {
                let mut publish = same_tap_publish(bare_url, "alpha", false);
                let cask = publish.homebrew_cask.as_mut().unwrap();
                gated(&mut cask.skip_upload, &mut cask.if_condition);
                single_crate_ctx(publish)
            },
            &|ctx, log| {
                assert!(super::publish_to_homebrew(ctx, "alpha", log).expect("formula pushes"));
            },
            &skip_line("homebrew cask for 'alpha'"),
        );

        assert_skip_line_prints_once_per_command(
            "own-tap cask",
            &|bare_url| {
                let mut publish = cask_only_publish(bare_url, "alpha");
                let cask = publish.homebrew_cask.as_mut().unwrap();
                gated(&mut cask.skip_upload, &mut cask.if_condition);
                single_crate_ctx(publish)
            },
            &|ctx, log| {
                assert!(!super::publish_cask(ctx, "alpha", log).expect("cask skips"));
            },
            &skip_line("homebrew cask for 'alpha'"),
        );

        assert_skip_line_prints_once_per_command(
            "formula",
            &|bare_url| {
                let mut formula = HomebrewConfig {
                    repository: Some(tap_repository(bare_url)),
                    ..Default::default()
                };
                gated(&mut formula.skip_upload, &mut formula.if_condition);
                single_crate_ctx(PublishConfig {
                    homebrew: Some(formula),
                    ..Default::default()
                })
            },
            &|ctx, log| {
                assert!(!super::publish_to_homebrew(ctx, "alpha", log).expect("formula skips"));
            },
            &if gate == "skip_upload" {
                skip_line("homebrew for 'alpha'")
            } else {
                "skipped homebrew for 'alpha' — `if` condition evaluated falsy".to_string()
            },
        );

        assert_skip_line_prints_once_per_command(
            "top-level cask",
            &|bare_url| {
                let mut top = HomebrewCaskConfig {
                    repository: Some(tap_repository(bare_url)),
                    ..cask("mycask")
                };
                gated(&mut top.skip_upload, &mut top.if_condition);
                release_ctx(
                    Config {
                        project_name: "alpha".to_string(),
                        crates: vec![crate_with("alpha", PublishConfig::default())],
                        homebrew_casks: Some(vec![top]),
                        ..Default::default()
                    },
                    &["alpha"],
                )
            },
            &|ctx, log| {
                let got = super::publish_top_level_homebrew_casks(ctx, log).expect("casks run");
                assert!(!got.pushed_any, "{got:?}");
            },
            &if gate == "skip_upload" {
                skip_line("cask for 'mycask'")
            } else {
                "skipped cask 'mycask' — `if` condition evaluated falsy".to_string()
            },
        );
    }
}

/// A crate whose formula and cask each sit on their own tap, with the
/// formula's gate set by `formula_gate` and the cask's by `cask_gate`.
fn own_tap_ctx(
    formula_gate: fn(&mut HomebrewConfig),
    cask_gate: fn(&mut HomebrewCaskConfig),
) -> Context {
    let mut formula = HomebrewConfig {
        repository: Some(github_tap("homebrew-tap", "FORMULA_TOKEN")),
        ..Default::default()
    };
    formula_gate(&mut formula);
    let mut own = HomebrewCaskConfig {
        repository: Some(github_tap("homebrew-casks", "CASK_TOKEN")),
        ..cask("alpha")
    };
    cask_gate(&mut own);
    single_crate_ctx(PublishConfig {
        homebrew: Some(formula),
        homebrew_cask: Some(own),
        ..Default::default()
    })
}

/// The preflight probe count for `ctx` against a responder that grants
/// push, and the verdict.
fn probes(mut ctx: Context) -> (bool, u32) {
    let body = r#"{"permissions":{"push":true}}"#;
    let (addr, calls) =
        anodizer_core::test_helpers::responder::spawn_oneshot_http_responder(vec![Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .into_boxed_str(),
        )]);
    ctx.set_env_source(
        anodizer_core::MapEnvSource::new()
            .with("ANODIZER_GITHUB_API_BASE", format!("http://{addr}"))
            .with("CASK_TOKEN", "tok"),
    );
    let check = homebrew_publisher(&ctx)
        .preflight(&ctx)
        .expect("preflight runs");
    (
        matches!(check, anodizer_core::PreflightCheck::Pass),
        calls.load(std::sync::atomic::Ordering::SeqCst),
    )
}

/// A cask on its own tap reads each gate field by field: its own value wins,
/// and one it leaves unset (or an empty `if: ""`) inherits the formula's. So
/// a cask `if: ""` under a formula `if: "false"` is inactive, and so is a
/// cask with no `skip_upload:` under a formula `skip_upload: true`; a cask
/// that sets its own passing gate is active. `config_fully_inactive`,
/// `requirements` (the cask's tap token) and `preflight`
/// (one probe, of the cask's tap) all agree.
#[test]
fn an_own_tap_cask_inherits_each_unset_gate_from_the_formula() {
    type FormulaGate = fn(&mut HomebrewConfig);
    type CaskGate = fn(&mut HomebrewCaskConfig);
    let cases: [(&str, FormulaGate, CaskGate, bool); 4] = [
        (
            "cask if: \"\" over formula if: \"false\"",
            |f| f.if_condition = Some("false".to_string()),
            |c| c.if_condition = Some(String::new()),
            false,
        ),
        (
            "cask if: \"true\" over formula if: \"false\"",
            |f| f.if_condition = Some("false".to_string()),
            |c| c.if_condition = Some("true".to_string()),
            true,
        ),
        (
            "cask skip_upload unset over formula skip_upload: true",
            |f| f.skip_upload = Some(StringOrBool::Bool(true)),
            |_| {},
            false,
        ),
        (
            "cask skip_upload: false over formula skip_upload: true",
            |f| f.skip_upload = Some(StringOrBool::Bool(true)),
            |c| c.skip_upload = Some(StringOrBool::Bool(false)),
            true,
        ),
    ];
    for (case, formula_gate, cask_gate, active) in cases {
        let ctx = own_tap_ctx(formula_gate, cask_gate);
        let publisher = homebrew_publisher(&ctx);
        assert_eq!(!publisher.config_fully_inactive(&ctx), active, "{case}");
        let want = if active {
            vec![
                EnvRequirement::Tool {
                    name: "git".to_string(),
                },
                EnvRequirement::EnvAllOf {
                    vars: vec!["CASK_TOKEN".to_string()],
                },
            ]
        } else {
            Vec::new()
        };
        assert_eq!(publisher.requirements(&ctx), want, "{case}");
        assert_eq!(probes(ctx), (true, u32::from(active)), "{case}");
    }
}

/// A cask-only crate needs `git` and the tap token: `HOMEBREW_TAP_TOKEN`,
/// else the GitHub token ladder.
#[test]
fn a_cask_only_crate_requires_the_tap_token() {
    let ctx = single_crate_ctx(PublishConfig {
        homebrew_cask: Some(HomebrewCaskConfig {
            repository: Some(RepositoryConfig {
                token: None,
                ..github_tap("homebrew-tap", "UNUSED")
            }),
            ..cask("alpha")
        }),
        ..Default::default()
    });
    let vars = std::iter::once("HOMEBREW_TAP_TOKEN")
        .chain(anodizer_core::git::GITHUB_TOKEN_ENV_LADDER.iter().copied())
        .map(str::to_string)
        .collect();
    assert_eq!(
        homebrew_publisher(&ctx).requirements(&ctx),
        [
            EnvRequirement::Tool {
                name: "git".to_string()
            },
            EnvRequirement::EnvAnyOf { vars },
        ]
    );
}

/// A cask names the formula's tap when owner and name match without regard
/// to case and the branch, token and `pull_request:` agree; any of those
/// three differing sends the cask to its own commit.
#[test]
fn a_cask_shares_the_formula_tap_only_on_the_same_branch_token_and_pull_request() {
    let formula_tap = github_tap("homebrew-tap", "TAP_TOKEN");
    let with_cask_repo = |repo: RepositoryConfig| PublishConfig {
        homebrew: Some(HomebrewConfig {
            repository: Some(formula_tap.clone()),
            ..Default::default()
        }),
        homebrew_cask: Some(HomebrewCaskConfig {
            repository: Some(repo),
            ..cask("alpha")
        }),
        ..Default::default()
    };
    let cases = [
        ("the same repository", formula_tap.clone(), true),
        (
            "owner and name in another case",
            RepositoryConfig {
                owner: Some("MyOrg".to_string()),
                name: Some("Homebrew-Tap".to_string()),
                ..formula_tap.clone()
            },
            true,
        ),
        (
            "another branch",
            RepositoryConfig {
                branch: Some("casks".to_string()),
                ..formula_tap.clone()
            },
            false,
        ),
        (
            "no branch beside the formula's",
            RepositoryConfig {
                branch: None,
                ..formula_tap.clone()
            },
            false,
        ),
        (
            "another token",
            RepositoryConfig {
                token: Some("{{ .Env.OTHER }}".to_string()),
                ..formula_tap.clone()
            },
            false,
        ),
        (
            "a pull_request block",
            RepositoryConfig {
                pull_request: Some(anodizer_core::config::PullRequestConfig {
                    enabled: Some(true),
                    ..Default::default()
                }),
                ..formula_tap.clone()
            },
            false,
        ),
        (
            "another tap",
            github_tap("homebrew-casks", "TAP_TOKEN"),
            false,
        ),
    ];
    for (case, repo, shared) in cases {
        let publish = with_cask_repo(repo);
        let crate_cask = super::CrateCask::of(&publish).expect("cask configured");
        assert_eq!(crate_cask.same_tap().is_some(), shared, "{case}");
    }
}

/// A cask that spells the formula's tap in another case rides the formula's
/// commit: one commit holds both files.
#[test]
fn a_case_folded_cask_repository_rides_the_formula_commit() {
    let (bare_url, bare) = make_bare_cask_tap("main");
    let mut publish = same_tap_publish(&bare_url, "alpha", false);
    publish.homebrew_cask.as_mut().unwrap().repository = Some(RepositoryConfig {
        owner: Some("MyOrg".to_string()),
        name: Some("Homebrew-Tap".to_string()),
        ..tap_repository(&bare_url)
    });
    let mut ctx = single_crate_ctx(publish);
    homebrew_publisher(&ctx)
        .run(&mut ctx)
        .expect("publish runs");
    assert_eq!(tap_files(&bare_url), "Casks/alpha.rb\nREADME\nalpha.rb");
    assert_eq!(commit_count(&bare_url), "2", "one commit over the seed");
    drop(bare);
}

/// The version `crate_name` publishes under: its own tag scope, the scope
/// reconcile probes in.
fn published_version(ctx: &mut Context, crate_name: &str) -> String {
    crate::publisher_helpers::with_published_crate_scope(
        ctx,
        crate_name,
        &anodizer_core::crate_scope::resolve_crate_tag,
        |ctx| Ok(ctx.version()),
    )
    .expect("crate scope resolves")
}

/// Reconcile a context against a responder that answers the open-PR search
/// with `search_body`, returning the verdict and how many searches ran.
fn reconcile_against(mut ctx: Context, search_body: &str) -> (anodizer_core::ReconcileState, u32) {
    let response = Box::leak(
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{search_body}",
            search_body.len()
        )
        .into_boxed_str(),
    );
    let (addr, calls) =
        anodizer_core::test_helpers::responder::spawn_oneshot_http_responder(vec![response]);
    let _lock = anodizer_core::test_helpers::env::env_mutex()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _base = anodizer_core::test_helpers::env::EnvGuard::set(
        "ANODIZER_GITHUB_API_BASE",
        format!("http://{addr}"),
    );
    let state = homebrew_publisher(&ctx)
        .reconcile(&mut ctx)
        .expect("reconcile runs");
    (state, calls.load(std::sync::atomic::Ordering::SeqCst))
}

/// A crate whose formula is gated off by `skip_upload: true` and whose cask
/// shares the formula's tap, which takes pull requests when `pull_request`
/// is set.
fn gated_formula_with_same_tap_cask(bare_url: &str, pull_request: bool) -> Context {
    let mut publish = same_tap_publish(bare_url, "alpha", false);
    let formula = publish.homebrew.as_mut().unwrap();
    formula.skip_upload = Some(StringOrBool::Bool(true));
    if pull_request {
        formula.repository.as_mut().unwrap().pull_request =
            Some(anodizer_core::config::PullRequestConfig {
                enabled: Some(true),
                ..Default::default()
            });
    }
    publish.homebrew_cask.as_mut().unwrap().skip_upload = Some(StringOrBool::Bool(false));
    single_crate_ctx(publish)
}

/// Reconcile visits a crate whose formula is gated off when its same-tap
/// cask is active: it searches for the cask's own pull request (the title
/// `publish_cask` opens) and reads an open one as already published, and a
/// search that finds none as absent. A tap pushed directly is never
/// reconciled, since a re-run's push converges on its own, so that case
/// searches nothing and reads absent.
#[test]
fn reconcile_visits_the_same_tap_cask_of_a_gated_off_formula() {
    let (bare_url, bare) = make_bare_cask_tap("main");

    let mut ctx = gated_formula_with_same_tap_cask(&bare_url, true);
    let version = published_version(&mut ctx, "alpha");
    let (state, searches) = reconcile_against(
        ctx,
        &format!(
            r#"{{"total_count":1,"items":[{{"title":"Update alpha cask to {version}","html_url":"https://github.com/myorg/homebrew-tap/pull/9"}}]}}"#
        ),
    );
    match state {
        anodizer_core::ReconcileState::Complete { note } => {
            assert_eq!(
                note,
                format!(
                    "open PR for alpha {version}: https://github.com/myorg/homebrew-tap/pull/9"
                )
            );
        }
        other => panic!("expected Complete, got {other:?} after {searches} searches"),
    }
    assert_eq!(searches, 1);

    let (state, searches) = reconcile_against(
        gated_formula_with_same_tap_cask(&bare_url, true),
        r#"{"total_count":0,"items":[]}"#,
    );
    assert!(
        matches!(state, anodizer_core::ReconcileState::Absent),
        "{state:?}"
    );
    assert_eq!(searches, 1);

    let (state, searches) = reconcile_against(
        gated_formula_with_same_tap_cask(&bare_url, false),
        r#"{"total_count":0,"items":[]}"#,
    );
    assert!(
        matches!(state, anodizer_core::ReconcileState::Absent),
        "{state:?}"
    );
    assert_eq!(searches, 0);
    drop(bare);
}
