use super::*;
use anodizer_core::config::{AurConfig, CrateConfig, PublishConfig, StringOrBool};
use anodizer_core::test_helpers::TestContextBuilder;
use anodizer_core::{PreflightCheck, PublishEvidence, Publisher, PublisherGroup};

fn aur_crate(name: &str) -> CrateConfig {
    CrateConfig {
        name: name.to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        publish: Some(PublishConfig {
            aur: Some(AurConfig {
                git_url: Some(format!("ssh://aur@aur.archlinux.org/{name}-bin.git")),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[test]
fn aur_publisher_classification() {
    let p = AurOurPublisher::new();
    assert_eq!(p.name(), "aur");
    assert_eq!(p.group(), PublisherGroup::Manager);
    assert!(!p.required());
    assert_eq!(p.rollback_scope_needed(), Some("AUR_SSH_KEY write"));
}

/// `--crate x` selects only the skip:true entry; an active sibling `y`
/// outside the selection must not keep the publisher live.
#[test]
fn config_fully_inactive_true_when_selected_crate_is_skipped_sibling_active() {
    let mut skipped = aur_crate("x");
    skipped.publish.as_mut().unwrap().aur.as_mut().unwrap().skip = Some(StringOrBool::Bool(true));
    let ctx = TestContextBuilder::new()
        .crates(vec![skipped, aur_crate("y")])
        .selected_crates(vec!["x".to_string()])
        .build();

    assert!(
        AurOurPublisher::new().config_fully_inactive(&ctx),
        "--crate x selects only the skip:true entry; active sibling y is out of \
             scope and must not keep the publisher live"
    );
}

/// Empty `--crate` selection means "all crates" — an active entry with
/// no `--crate` filter applied must keep the publisher live.
#[test]
fn config_fully_inactive_false_with_empty_selection_and_active_entry() {
    let ctx = TestContextBuilder::new()
        .crates(vec![aur_crate("x")])
        .build();

    assert!(
        !AurOurPublisher::new().config_fully_inactive(&ctx),
        "empty selection means \"all crates\"; an active entry must keep the \
             publisher live"
    );
}

#[test]
fn aur_preflight_defaults_to_pass() {
    let ctx = TestContextBuilder::new().build();
    let p = AurOurPublisher::new();
    assert!(matches!(
        p.preflight(&ctx).expect("preflight ok"),
        PreflightCheck::Pass
    ));
}

/// The AUR schema floor runs `bash -n` over the rendered PKGBUILD when
/// the tool is present and warn+skips otherwise, so it is ADVISORY:
/// recommended to the auto-install layer, never a blocker.
#[test]
fn aur_advisory_requirements_emit_bash_when_active() {
    let ctx = TestContextBuilder::new()
        .crates(vec![aur_crate("demo")])
        .build();
    let reqs = AurOurPublisher::new().advisory_requirements(&ctx);
    assert!(
        reqs.iter().any(|r| matches!(
            r,
            anodizer_core::EnvRequirement::Tool { name } if name == "bash"
        )),
        "active aur entry must recommend bash: {reqs:?}"
    );
}

#[test]
fn aur_advisory_requirements_empty_when_all_entries_skipped() {
    let mut c = aur_crate("demo");
    if let Some(a) = c.publish.as_mut().and_then(|p| p.aur.as_mut()) {
        a.skip = Some(StringOrBool::Bool(true));
    }
    let ctx = TestContextBuilder::new().crates(vec![c]).build();
    let reqs = AurOurPublisher::new().advisory_requirements(&ctx);
    assert!(
        reqs.is_empty(),
        "every entry skipped ⇒ no advisory recommendations: {reqs:?}"
    );
}

#[test]
fn aur_rollback_warns_when_no_targets_recorded() {
    let capture = anodizer_core::log::LogCapture::new();
    let mut ctx = TestContextBuilder::new().build();
    ctx.with_log_capture(capture.clone());
    let evidence = PublishEvidence::new("aur");
    let p = AurOurPublisher::new();
    assert!(p.rollback(&mut ctx, &evidence).is_ok());

    let warns = capture.warn_messages();
    assert!(
        warns.iter().any(|m| m.contains("aur")
            && m.contains("AUR repo clone targets")
            && m.contains("verify")),
        "expected captured warn naming publisher + target-noun + 'verify'; got: {warns:?}"
    );
}

#[test]
fn aur_target_extra_roundtrips() {
    let original = vec![AurOurTarget {
        target: "demo-bin".into(),
        git_url: "ssh://aur@aur.archlinux.org/demo-bin.git".into(),
        private_key: None,
        git_ssh_command: None,
    }];
    let extra =
        anodizer_core::PublishEvidenceExtra::Aur(anodizer_core::publish_evidence::AurExtra {
            aur_our_targets: original.iter().map(Into::into).collect(),
        });
    let decoded = decode_aur_our_targets(&extra);
    assert_eq!(decoded, original);
}

#[test]
fn aur_collect_run_targets_uses_default_bin_suffix() {
    let ctx = TestContextBuilder::new()
        .crates(vec![aur_crate("demo")])
        .build();
    let targets =
        collect_aur_our_run_targets(&ctx, &ctx.logger("publish")).expect("collect run targets");
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].target, "demo-bin");
    assert!(targets[0].git_url.ends_with("demo-bin.git"));
}

#[test]
fn aur_effective_publish_crates_implicit_all_when_selection_empty() {
    // Regression pin for the `selected_crates = Vec::new()` failure
    // mode: the run path used to iterate the empty Vec and silently
    // skip every configured AUR repo. The helper now resolves to
    // implicit-all over `publish.aur`-carrying crates.
    let ctx = TestContextBuilder::new()
        .crates(vec![
            aur_crate("alpha"),
            aur_crate("beta"),
            CrateConfig {
                name: "gamma".to_string(),
                path: ".".to_string(),
                tag_template: Some("v{{ .Version }}".to_string()),
                publish: Some(PublishConfig::default()),
                ..Default::default()
            },
        ])
        .build();
    let names =
        crate::publisher_helpers::effective_publish_crates(&ctx, is_aur_per_crate_configured);
    assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);
}

#[test]
fn aur_effective_publish_crates_honors_non_empty_selection() {
    let ctx = TestContextBuilder::new()
        .crates(vec![aur_crate("alpha"), aur_crate("beta")])
        .selected_crates(vec!["beta".to_string()])
        .build();
    let names =
        crate::publisher_helpers::effective_publish_crates(&ctx, is_aur_per_crate_configured);
    assert_eq!(names, vec!["beta".to_string()]);
}

#[test]
fn aur_our_target_extra_omits_private_key_after_serde_roundtrip() {
    // SECURITY: persisting `private_key` / `git_ssh_command` into
    // `dist/run-<id>/report.json`, the run summary
    // (`--summary-json`), or the announce-time release-body text
    // would leak the SSH key publicly. The
    // `AurTargetSnapshot` core type has no field for either
    // credential, so the type system rejects any future leak
    // attempt at the encode boundary. This test pins the
    // resulting wire shape: a populated AurOurTarget converts
    // into the snapshot WITHOUT carrying the secret bytes.
    let with_secrets = AurOurTarget {
        target: "demo-bin".into(),
        git_url: "ssh://aur@aur.archlinux.org/demo-bin.git".into(),
        private_key: Some("PRIVATE-KEY-CONTENTS".into()),
        git_ssh_command: Some("ssh -i /tmp/key".into()),
    };
    let extra =
        anodizer_core::PublishEvidenceExtra::Aur(anodizer_core::publish_evidence::AurExtra {
            aur_our_targets: vec![(&with_secrets).into()],
        });
    let serialized = serde_json::to_string(&extra).expect("serialize");
    assert!(
        !serialized.contains("PRIVATE-KEY-CONTENTS"),
        "private_key leaked into serialized evidence: {serialized}"
    );
    assert!(
        !serialized.contains("/tmp/key"),
        "git_ssh_command leaked into serialized evidence: {serialized}"
    );
    let parsed: serde_json::Value = serde_json::from_str(&serialized).expect("parse");
    let first = &parsed["aur_our_targets"][0];
    assert!(
        first.get("private_key").is_none(),
        "private_key field present in evidence: {first}"
    );
    assert!(
        first.get("git_ssh_command").is_none(),
        "git_ssh_command field present in evidence: {first}"
    );
    // Positive shape: operator-public coordinates survive the
    // conversion.
    assert_eq!(first["target"], "demo-bin");
    assert_eq!(first["git_url"], "ssh://aur@aur.archlinux.org/demo-bin.git");
}

#[test]
fn aur_our_rollback_re_reads_private_key_from_config() {
    // `#[serde(skip)]` means decoded evidence has no credentials.
    // Rollback must re-resolve them from `ctx.config.crates[*].
    // publish.aur.private_key` keyed by `git_url`. Verify the
    // resolver returns the live config's key + ssh command.
    let mut c = aur_crate("demo");
    if let Some(p) = c.publish.as_mut()
        && let Some(a) = p.aur.as_mut()
    {
        a.private_key = Some("ROTATED-KEY".into());
        a.git_ssh_command = Some("ssh -i /tmp/rotated".into());
    }
    let ctx = TestContextBuilder::new().crates(vec![c]).build();
    let (pk, ssh) =
        resolve_aur_credentials_from_config(&ctx, "ssh://aur@aur.archlinux.org/demo-bin.git")
            .unwrap();
    assert_eq!(pk.as_deref(), Some("ROTATED-KEY"));
    assert_eq!(ssh.as_deref(), Some("ssh -i /tmp/rotated"));

    // Unknown URL: returns (None, None) so the warn helper fires
    // and points the operator at publish.aur.private_key.
    let (pk, ssh) = resolve_aur_credentials_from_config(&ctx, "ssh://aur@x/y.git").unwrap();
    assert!(pk.is_none());
    assert!(ssh.is_none());
}

#[test]
fn aur_branch_constant_matches_publish_path() {
    // Pin the constant so both publish and rollback are guaranteed
    // to push to the same branch name; a stray rename (e.g. to
    // `main`) would have to edit this line.
    assert_eq!(AUR_REPO_BRANCH, "master");
}

#[test]
fn aur_dedup_targets_collapses_shared_git_url() {
    // Two recorded targets that happen to share a git_url collapse
    // to one entry. A second `git revert HEAD` against the same
    // AUR repo would revert the first revert and restore the bad
    // release — the dedup guards that.
    let targets = vec![
        AurOurTarget {
            target: "demo-bin".into(),
            git_url: "ssh://aur@aur.archlinux.org/demo-bin.git".into(),
            private_key: None,
            git_ssh_command: None,
        },
        AurOurTarget {
            target: "demo-alias".into(),
            git_url: "ssh://aur@aur.archlinux.org/demo-bin.git".into(),
            private_key: None,
            git_ssh_command: None,
        },
    ];
    let unique = dedup_aur_targets(&targets);
    assert_eq!(unique.len(), 1);
    assert_eq!(unique[0].target, "demo-bin");
}

#[test]
fn aur_collect_run_targets_records_derived_url_when_git_url_absent() {
    // No git_url: the live push derives the canonical AUR remote and
    // pushes, so the rollback collector must record that same derived
    // target — not skip it (else a pushed package has no rollback entry).
    let mut crate_cfg = aur_crate("demo");
    if let Some(p) = crate_cfg.publish.as_mut()
        && let Some(a) = p.aur.as_mut()
    {
        a.git_url = None;
    }
    let ctx = TestContextBuilder::new().crates(vec![crate_cfg]).build();
    let targets =
        collect_aur_our_run_targets(&ctx, &ctx.logger("publish")).expect("collect run targets");
    assert_eq!(targets.len(), 1, "expected one target, got {targets:?}");
    assert_eq!(targets[0].target, "demo-bin");
    assert_eq!(
        targets[0].git_url,
        "ssh://aur@aur.archlinux.org/demo-bin.git",
    );
}

// -----------------------------------------------------------------------
// Log-message helpers — the operator-facing log strings the publisher
// emits at each boundary.

#[test]
fn run_per_crate_start_message_names_crate() {
    let msg = run_per_crate_start_message("demo");
    assert!(msg.starts_with("starting per-crate aur publish"), "{msg}");
    assert!(msg.contains("'demo'"), "{msg}");
}

#[test]
fn run_done_message_reports_processed_count() {
    let msg = run_done_message(2);
    assert!(msg.starts_with("finished aur publish"), "{msg}");
    assert!(msg.contains("2 configured crate(s) processed"), "{msg}");
}

#[test]
fn run_no_eligible_crates_warning_names_remediation() {
    let msg = run_no_eligible_crates_warning(5);
    assert!(msg.starts_with("aur publisher registered"), "{msg}");
    assert!(msg.contains("0 of 5 effective"), "{msg}");
    assert!(msg.contains("nothing pushed"), "{msg}");
    assert!(msg.contains("--crate"), "{msg}");
    assert!(msg.contains("--all"), "{msg}");
}

/// The no-eligible-crates warning must fire only when the iteration
/// loop's configured-predicate filtered every selected crate out — not
/// when the publish path was reached successfully.
#[test]
fn should_warn_no_eligible_only_fires_when_predicate_filtered_everything() {
    // One configured crate reached the publish path → no warning.
    assert!(!should_warn_no_eligible(1, 1));
    // True positive: none configured.
    assert!(should_warn_no_eligible(0, 3));
    // Empty selection → no warning.
    assert!(!should_warn_no_eligible(0, 0));
    // Partial-skip → no warning.
    assert!(!should_warn_no_eligible(1, 3));
}

/// Run the publisher end-to-end in dry-run mode against a context that
/// selects an aur-configured crate. Verifies the run path is wired
/// (returns Ok). The log lines are written to stderr and asserted
/// indirectly via the helper-string tests above.
#[test]
fn aur_publisher_run_dry_run_returns_ok() {
    let repo = crate::testing::hermetic_tagged_repo();
    let mut ctx = TestContextBuilder::new()
        .crates(vec![aur_crate("demo")])
        .selected_crates(vec!["demo".to_string()])
        .dry_run(true)
        .project_root(repo.path().to_path_buf())
        .build();
    let p = AurOurPublisher::new();
    let evidence = p.run(&mut ctx).expect("dry-run publisher.run");
    // dry-run publish_to_aur short-circuits before git push; no actual
    // push occurred so evidence.extra must be empty (no phantom targets).
    let targets = decode_aur_our_targets(&evidence.extra);
    assert!(
        targets.is_empty(),
        "dry-run must not record rollback targets: {targets:?}"
    );
}

/// When the publisher is registered (a crate has an aur block) but the
/// selected-crates filter excludes every aur-configured crate, the run
/// path must still return Ok and the processed count is zero.
#[test]
fn aur_publisher_run_no_eligible_crates_returns_ok() {
    let mut ctx = TestContextBuilder::new()
        .crates(vec![
            aur_crate("demo"),
            CrateConfig {
                name: "other".to_string(),
                path: ".".to_string(),
                tag_template: Some("v{{ .Version }}".to_string()),
                publish: Some(PublishConfig::default()),
                ..Default::default()
            },
        ])
        // Select only the non-aur crate — publisher registered but
        // run path will iterate zero aur-configured crates.
        .selected_crates(vec!["other".to_string()])
        .dry_run(true)
        .build();
    let p = AurOurPublisher::new();
    // Must return Ok even when no aur-configured crate is selected.
    p.run(&mut ctx).expect("publisher.run ok");
}

/// Implicit-all selection (empty `selected_crates`) + dry-run must
/// produce empty evidence. The implicit-all path resolves through
/// `effective_publish_crates` to every aur-configured crate, so this
/// pins the gate where phantom rollback targets used to leak.
#[test]
fn test_publish_to_aur_dry_run_implicit_all_produces_empty_evidence() {
    let repo = crate::testing::hermetic_tagged_repo();
    let mut ctx = TestContextBuilder::new()
        .crates(vec![aur_crate("demo"), aur_crate("other")])
        // No selected_crates → implicit-all resolves to both aur crates.
        .dry_run(true)
        .project_root(repo.path().to_path_buf())
        .build();
    let p = AurOurPublisher::new();
    let evidence = p.run(&mut ctx).expect("dry-run implicit-all publisher.run");
    let targets = decode_aur_our_targets(&evidence.extra);
    assert!(
        targets.is_empty(),
        "dry-run + implicit-all must not record rollback targets: {targets:?}"
    );
}

/// skip_upload path must produce empty evidence — no push occurred.
#[test]
fn aur_publisher_run_skip_upload_produces_empty_evidence() {
    let mut crate_with_skip = aur_crate("demo");
    if let Some(ref mut publish) = crate_with_skip.publish
        && let Some(ref mut aur) = publish.aur
    {
        aur.skip_upload = Some(StringOrBool::Bool(true));
    }
    let repo = crate::testing::hermetic_tagged_repo();
    let mut ctx = TestContextBuilder::new()
        .crates(vec![crate_with_skip])
        .selected_crates(vec!["demo".to_string()])
        .project_root(repo.path().to_path_buf())
        .build();
    let p = AurOurPublisher::new();
    let evidence = p.run(&mut ctx).expect("skip_upload publisher.run");
    let targets = decode_aur_our_targets(&evidence.extra);
    assert!(
        targets.is_empty(),
        "skip_upload must not record rollback targets: {targets:?}"
    );
}

#[test]
fn aur_publisher_visible_work_contract() {
    use crate::testing::assert_publisher_visible_work_contract;
    let repo = crate::testing::hermetic_tagged_repo();
    let mut ctx = TestContextBuilder::new()
        .crates(vec![aur_crate("demo")])
        .selected_crates(vec!["demo".to_string()])
        .dry_run(true)
        .project_root(repo.path().to_path_buf())
        .build();
    let p = AurOurPublisher::new();
    assert_publisher_visible_work_contract(&p, &mut ctx);
}

/// Building an AUR PKGBUILD for a linux artifact whose `sha256`
/// metadata is empty must bail with an actionable error. Defaulting
/// to `""` would emit `sha256sums_<arch>=('')` in the generated
/// PKGBUILD, which silently disables makepkg's integrity check and
/// ships an unverified tarball. The bail message must name the
/// publisher, the field, the offending artifact context, and a
/// next-step hint.
#[test]
fn aur_sha256_empty_metadata_bails_with_actionable_error() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::AurConfig;
    let mut c = aur_crate("mytool");
    if let Some(ref mut publish) = c.publish
        && let Some(ref mut aur) = publish.aur
    {
        *aur = AurConfig {
            git_url: Some("ssh://aur@aur.archlinux.org/mytool-bin.git".to_string()),
            license: Some("MIT".to_string()),
            homepage: Some("https://example.com/mytool".to_string()),
            ..Default::default()
        };
    }
    let mut ctx = TestContextBuilder::new()
        .crates(vec![c])
        .selected_crates(vec!["mytool".to_string()])
        .build();
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        path: std::path::PathBuf::from("/tmp/mytool-linux-amd64.tar.gz"),
        name: "mytool-linux-amd64.tar.gz".to_string(),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "mytool".to_string(),
        metadata: {
            let mut m = std::collections::HashMap::new();
            m.insert(
                "url".to_string(),
                "https://example.com/mytool-linux-amd64.tar.gz".to_string(),
            );
            m
        },
        size: None,
    });
    let log = anodizer_core::log::StageLogger::new("publish", anodizer_core::log::Verbosity::Quiet);
    let err = publish_to_aur(&ctx, "mytool", &log).expect_err("missing sha256 must bail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("missing sha256 metadata"),
        "error must mention missing sha256; got: {msg}"
    );
    assert!(
        msg.contains("mytool"),
        "error must name the offending crate; got: {msg}"
    );
    assert!(
        msg.contains("checksum stage"),
        "error must mention the checksum stage; got: {msg}"
    );
}
