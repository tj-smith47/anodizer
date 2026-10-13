//! A crate's cask under `publish.homebrew_cask:` and under the
//! `publish.homebrew.cask:` sub-block: both register the publisher, render
//! and push the same cask.

use super::tests::{art_with_url_sha, make_bare_cask_tap};
use anodizer_core::Publisher;
use anodizer_core::artifact::ArtifactKind;
use anodizer_core::config::{
    Config, CrateConfig, GitRepoConfig, HomebrewCaskConfig, HomebrewConfig, PublishConfig,
    RepositoryConfig, WorkspaceConfig,
};
use anodizer_core::context::{Context, ContextOptions};
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
