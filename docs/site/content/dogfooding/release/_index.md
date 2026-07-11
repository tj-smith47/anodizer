+++
title = "Release pipeline"
description = "Release-pipeline config keys: release.*, changelog.*, announce.*, blobs[], publishers[]."
weight = 30
template = "section.html"
+++

# Release pipeline

The keys that drive the release itself: GitHub/GitLab/Gitea release surface,
changelog generation, announcers, cloud uploads, and custom publishers.

## Live configuration

Release / changelog / announce blocks from
[`cfgd/.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml)
(snapshot 2026-05-24) — every key in the tables below is wired here. The
`announce:` block has since moved into an `includes:` fragment,
[`cfgd/.anodizer/announce.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer/announce.yaml).

```yaml
# Per-crate release section:
release:
  github: { owner: tj-smith47, name: cfgd }
  draft: false
  prerelease: auto
  make_latest: auto
  mode: keep-existing
  target_commitish: "{{ .Commit }}"
  discussion_category_name: "Announcements"
  replace_existing_draft: false
  replace_existing_artifacts: true
  name_template: "{{ ProjectName }} {{ Tag }}"
  header: |
    What's new in {{ .ProjectName }} {{ .Tag }}.
  footer: |
    Released with [anodizer](https://github.com/tj-smith47/anodizer).
  include_meta: true
  extra_files:
    - { glob: "./install.sh", name_template: "install.sh" }

# Top-level changelog (groups + filters):
changelog:
  use: git
  groups:
    - { title: "Features",  regexp: "^.*feat[(\\w)]*:+.*$",  order: 0 }
    - { title: "Bug Fixes", regexp: "^.*fix[(\\w)]*:+.*$",   order: 1 }
    - { title: "Others",    order: 999 }
  filters:
    include: ["^feat", "^fix", "^perf", "^revert"]
    exclude: ["^docs:", "^test:", "^chore:", "^ci:"]

# Top-level announce (only the two live channels):
announce:
  webhook:
    enabled: true
    endpoint_url: "https://tj.jarvispro.io/webhooks/anodizer"
    message_template: '{"project":"{{ ProjectName }}","tag":"{{ Tag }}","url":"{{ ReleaseURL }}"}'
  email:
    enabled: true
    host: smtp.gmail.com
    port: 587
    from: toss45@gmail.com
    to: ["tj@jarvispro.io"]
    subject_template: "{{ ProjectName }} {{ Tag }} released"

cloudsmiths:
  - { id: cfgd, repo: tj-smith47/cfgd, package_format: deb, distros: [ubuntu/jammy] }
```

## Release and changelog

| Key | Status | Notes |
|---|---|---|
| `release.github` | ✅ Verified | [anodizer releases](https://github.com/tj-smith47/anodizer/releases). Header/footer/draft/prerelease/make_latest all exercised |
| `release.metadata` | ✅ Verified | [v0.1.1 metadata.json](https://github.com/tj-smith47/anodizer/releases/download/v0.1.1/metadata.json) · [artifacts.json](https://github.com/tj-smith47/anodizer/releases/download/v0.1.1/artifacts.json) |
| `release.name_template` / `tag_template` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`tag_template: "core-v{{ Version }}"` / `"v{{ Version }}"` / `"operator-v{{ Version }}"` / `"csi-v{{ Version }}"`) |
| `release.header` / `footer` | ✅ Verified | [cfgd v0.3.5 release body](https://github.com/tj-smith47/cfgd/releases/tag/v0.3.5) (`What's new` header + `Released with anodizer` footer) |
| `changelog.groups` | ✅ Verified | "Features" / "Bug Fixes" / "Others" sections in the [v0.1.1 release body](https://github.com/tj-smith47/anodizer/releases/tag/v0.1.1) |
| `changelog.filters.include` / `exclude` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`changelog.filters.include` / `exclude` patterns) |
| `changelog.use: git` | ✅ Verified | [`crates/stage-changelog/src/lib.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-changelog/src/lib.rs) (`use: git` branch) |
| `changelog.use: github-native` | ✅ Verified | [brontes `.anodizer.yaml` at v0.2.0](https://github.com/tj-smith47/brontes/blob/v0.2.0/.anodizer.yaml) (`use: github-native`) rendered the [v0.1.0](https://github.com/tj-smith47/brontes/releases/tag/v0.1.0) and [v0.2.0](https://github.com/tj-smith47/brontes/releases/tag/v0.2.0) release bodies (dogfooded through v0.2.0; brontes moved to `use: git` for v0.2.1). Code path: [`crates/stage-changelog/src/lib.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-changelog/src/lib.rs) |
| `changelog.use: github` | ✅ Verified | [`crates/stage-changelog/src/lib.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-changelog/src/lib.rs) (`use: github` branch) |
| `changelog.use: gitlab` / `gitea` | ✅ Verified | [`crates/stage-changelog/src/lib.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-changelog/src/lib.rs) (`gitlab` / `gitea` branches) |
| `changelog.use: ai` | 🤝 Help wanted | anthropic / openai / ollama implemented; no live release uses it |
| `release.gitlab` | 🤝 Help wanted | We dogfood on GitHub only |
| `release.gitea` | 🤝 Help wanted | We dogfood on GitHub only |
| `milestones[]` | ✅ Verified | [`crates/core/src/config/milestone.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/core/src/config/milestone.rs) |

## Release resilience

These features shipped 2026-05-14 in response to the anodize **v0.2.0 cascade
failure** ([Run 25754442852](https://github.com/tj-smith47/anodizer/actions/runs/25754442852)
and four siblings on 2026-05-12, all failing in the publish stage). They form
three-group publisher dispatch (Assets, Manager, Submitter), a Submitter gate
that aborts the Submitter group when required Assets or Manager publishers
fail, opt-in rollback per-publisher, and a `--rollback-only --from-run=<id>`
replay path. Several behaviors have unit/integration test coverage today
(rows marked `✅ Verified (tests)` below); rows that need a live v0.2.x+ tag
to exercise the codepath stay `🤝 Help wanted`.

| Key | Status | Notes |
|---|---|---|
| Three-group Submitter gate (default-on) | ✅ Verified | Fired live in the failed v0.15.1 publish ([run 28809062839](https://github.com/tj-smith47/anodizer/actions/runs/28809062839)): after the required gemfury publisher failed, the log shows `skipping cargo — gated by an earlier required failure (one-way-door protection)` (same for chocolatey and winget) and the run summary records `submitter_gated=true`. [`crates/stage-publish/src/dispatch.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-publish/src/dispatch.rs) |
| `--no-gate-submitter` override | ✅ Verified (tests) | [`crates/stage-publish/src/dispatch.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-publish/src/dispatch.rs) (`dispatch::tests::no_gate_submitter_runs_submitter_anyway`) + CLI parse (`crates/cli/src/main.rs::tests::release_parses_no_gate_submitter_flag`); awaits a live release that flips the override |
| `--rollback=best-effort` | ✅ Verified | The best-effort rollback path fired live (as the default failure policy — the explicit flag override itself is unexercised) in the failed v0.15.1 publish ([run 28809062839](https://github.com/tj-smith47/anodizer/actions/runs/28809062839)): `required failure(s) detected; invoking best-effort rollback` → `dispatching rollback for 12 target(s)` → `rollback complete — 7 rolled back, 0 failed, 3 skipped-no-scope`. [`crates/stage-publish/src/rollback.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-publish/src/rollback.rs) |
| `--rollback-only --from-run=<id>` replay | ✅ Verified (tests) | [`crates/stage-publish/src/rollback_only.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-publish/src/rollback_only.rs) — idempotency + read/dispatch covered by `rollback_only::tests::rollback_only_reads_report_and_dispatches`, `rollback_only_second_invocation_is_noop_for_already_rolled_back_entries`, plus path-traversal guard at the binary surface (`crates/cli/tests/integration.rs::release_from_run_rejects_path_traversal_at_binary_surface`, `release_rollback_only_invokes_replay_from_disk`) and `crates/stage-publish/tests/run_report_persistence.rs::publish_stage_writes_report_and_rollback_only_can_read_it` |
| `--fail-fast` | ✅ Verified (tests) | [anodize `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) plus [release command wiring](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/release/mod.rs) (`fail_fast` opts); dispatcher coverage via `dispatch::tests::fail_fast_aborts_at_first_error` — pre-resilience-work flag, exercised in v0.1.x runs but no live v0.2.x release has tripped it; the default collect-then-bail mode it inverts is live-proven at [run 28809062839](https://github.com/tj-smith47/anodizer/actions/runs/28809062839) |
| `--summary-json=<path>` audit-trail | ✅ Verified (tests) | [`crates/stage-publish/src/run_summary.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-publish/src/run_summary.rs) — JSON schema v1 round-trip + writer covered by `run_summary::tests::run_summary_schema_v1_roundtrips_through_json`, `run_summary_rejects_unknown_fields`, `write_summary_json_creates_parent_dir`; CLI parse at `crates/cli/src/main.rs::tests::release_parses_summary_json`. The summary emission itself is live: both v0.15.5 ([run 28882554907](https://github.com/tj-smith47/anodizer/actions/runs/28882554907), `wrote ./dist/run-v0.15.5/summary.json`) and the failed v0.15.1 run wrote it at the default path, and `release.yml` uploads it as the `run-summary-*` artifact — the explicit `--summary-json=<path>` override flag is what remains unexercised |
| `announce.gate_on` config (default `required_publishers`) | ✅ Verified | Evaluated live at v0.15.5 ([run 28882554907](https://github.com/tj-smith47/anodizer/actions/runs/28882554907)): with `gate_on: required_publishers` set in [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml), the announce stage fired (webhook + email) after all required publishers succeeded and the run summary recorded `announce_gated=false`. The skip-on-failure branch remains test-proven ([`crates/stage-announce/src/run.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-announce/src/run.rs) `announce_skips_when_gate_required_and_required_failure`, `announce_skips_when_gate_all_and_any_failure`) |
| Preflight rollback-scope checks | ✅ Verified (tests) | [`crates/stage-publish/src/preflight.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-publish/src/preflight.rs) — warn / strict-block / best-effort-bail paths covered by `preflight::tests::preflight_warns_on_missing_rollback_scope`, `preflight_blocks_on_missing_rollback_scope_when_strict`, `preflight_bails_when_required_publisher_missing_scope_and_rollback_best_effort`; no live release has tripped them |
| AnnounceStage emit-summary-on-skip | ✅ Verified | [`crates/stage-announce/src/run.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-announce/src/run.rs) — emit-on-gate-skip + emit-when-stage-not-called covered by `run::tests::emit_summary_writes_when_gate_would_fire`, `emit_summary_writes_when_announce_stage_was_not_called`, `emit_summary_writes_summary_when_path_set`, plus integration test `crates/cli/tests/integration.rs::test_release_skip_announce_still_writes_summary_json`. Fired live in the failed v0.15.1 publish ([run 28809062839](https://github.com/tj-smith47/anodizer/actions/runs/28809062839)): the pipeline failed before the announce stage ran, yet `wrote ./dist/run-v0.15.1/summary.json` still emitted the full status table |
| BlobStage writes to `ctx.publish_report` | ✅ Verified | Live on every release since MinIO blob upload went in: v0.15.5 ([run 28882554907](https://github.com/tj-smith47/anodizer/actions/runs/28882554907)) records `blob  Assets  required  succeeded` in the run summary, and the failed v0.15.1 run records `blob  Assets  required  rolled-back` — both only possible via the publish-report entry the stage writes. [`crates/stage-blob/src/run.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-blob/src/run.rs) |
| Snapcraft double-publish fix (`SnapcraftPublisher` unregistered unconditionally) | ✅ Verified (tests) | [`crates/stage-publish/src/registry.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-publish/src/registry.rs) — `registry::tests::snapcraft_unconditionally_unregistered_regardless_of_publish_flag` asserts the registry never re-registers `SnapcraftPublisher` alongside the load-bearing `SnapcraftPublishStage` ([`crates/stage-snapcraft/src/publish_stage.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-snapcraft/src/publish_stage.rs)) regardless of the `publish:` flag, preventing the v0.2.0 double-upload regression (commit `b3791cf`). Live behavior matches: the v0.15.5 publish ([run 28882554907](https://github.com/tj-smith47/anodizer/actions/runs/28882554907)) shows exactly one snap publish stage and a single `snapcraft` row in the run summary |
| Required-failure → non-zero exit gate | ✅ Verified | [`crates/cli/src/commands/release/mod.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/release/mod.rs) (`gate_required_failures`) — short-circuit / snapshot / dry-run / rollback-failed / optional-failure / missing-report branches covered by unit tests in the same module; commit `1d9a13e`. Fired live in both directions: the failed v0.15.1 publish ([run 28809062839](https://github.com/tj-smith47/anodizer/actions/runs/28809062839)) exited non-zero on `1 required publisher(s) failed: gemfury` (job failed with exit code 1), while v0.15.5 ([run 28882554907](https://github.com/tj-smith47/anodizer/actions/runs/28882554907)) stayed green despite an *optional* snapcraft failure — the optional-failure branch live |
| `--strict` / `--strict-preflight` | ✅ Verified (tests) | [`crates/cli/src/commands/release/mod.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/release/mod.rs) (`strict_preflight` opt + `opts.strict \|\| opts.strict_preflight` combiner) — promotion of `PublisherState::Unknown` preflight results to blockers covered by `strict_or_strict_preflight_promotes_unknown_to_blocker`; mutex with `--allow-nondeterministic` covered by `crates/cli/tests/integration.rs::release_strict_conflicts_with_allow_nondeterministic` |
| `--allow-rerun` + end-of-pipeline rerun guard | ✅ Verified (tests) | [`crates/stage-publish/src/lib.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-publish/src/lib.rs) (`refuse_rerun_if_report_exists`) — first-run / re-run / allow-rerun-override / snapshot / dry-run / rollback-only / local-run-id branches covered by sibling unit tests; mutual exclusion with `--rollback-only` asserted by `crates/cli/tests/integration.rs::test_release_allow_rerun_conflicts_with_rollback_only` |
| Real DELETE rollback for `blobs[]` | ⏳ Pending | The rollback path was exercised live in the failed v0.15.1 publish ([run 28809062839](https://github.com/tj-smith47/anodizer/actions/runs/28809062839)): structured evidence drove per-object `DELETE s3://…/v0.15.1/<asset>` attempts against all 47 uploaded keys — but every delete failed against MinIO (`Generic S3 error … builder error`), each surfaced as a `manual cleanup may be required` warning as designed (`blob rollback complete — 0 deleted, 0 already absent, 47 failed`). A successful live deletion is still unproven. Unit coverage: [`crates/stage-blob/src/publisher.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-blob/src/publisher.rs) `blob_publisher_rollback_decodes_structured_targets_and_attempts_delete` et al. (commit `1195ce5`) |
| Real DELETE rollback for `cloudsmiths[]` | ✅ Verified (tests) | [`crates/stage-publish/src/cloudsmith.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-publish/src/cloudsmith.rs) — slug captured at upload time round-trips through `PublishEvidence.extra` so rollback can issue real `DELETE` against the Cloudsmith API; coverage via `cloudsmith_target_serde_roundtrip_with_slug`, `cloudsmith_target_decode_tolerates_missing_slug_field`, `cloudsmith_target_decode_tolerates_null_slug`, `cloudsmith_rollback_falls_back_to_warn_when_slug_missing`, `cloudsmith_rollback_warns_when_no_targets_recorded` (commit `8a79bf1`) |
| `RunSummary` dynamic-width status table | ✅ Verified | [`crates/stage-publish/src/run_summary.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-publish/src/run_summary.rs) (`print_status_table`) — width adapts to the longest publisher name (capped at 40 chars with UTF-8-safe ellipsis truncation); covered by `print_status_table_renders_human_readable`, `print_status_table_widens_for_long_publisher_names`, `print_status_table_truncates_extremely_long_names` (commit `52c51da`). Rendered live at the end of both v0.15.5 ([run 28882554907](https://github.com/tj-smith47/anodizer/actions/runs/28882554907)) and the failed v0.15.1 run — 17-publisher status table with per-group/required/status columns |
| One-way-door burn probes (crates.io / Chocolatey / winget) | ✅ Verified (tests) | [`crates/stage-publish/src/cargo.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-publish/src/cargo.rs), [`crates/stage-publish/src/chocolatey/mod.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-publish/src/chocolatey/mod.rs), [`crates/stage-publish/src/post_publish/winget.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-publish/src/post_publish/winget.rs) — before rollback touches an irreversible registry, a probe checks whether the version already landed (crates.io index, Chocolatey package page scrape — the flat OData feed hides pending moderation — and the winget upstream PR search). Positive evidence refuses rollback; ambiguity stays clear. No live release has tripped a burn probe yet — in the one live failure so far ([run 28809062839](https://github.com/tj-smith47/anodizer/actions/runs/28809062839)) the Submitter gate stopped cargo/chocolatey/winget *before* they published, so there was no landed one-way-door publish for a probe to check |
| `release.on_failure` policy (`rollback` \| `hold`) + auto-degrade past burned doors | ✅ Verified (tests) | [`crates/cli/src/commands/release/failure_policy.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/release/failure_policy.rs) + `crates/cli/tests/failure_policy_integration.rs` — the binary evaluates `release.on_failure` in-process on pipeline failure; `rollback` degrades to `hold` the moment any Submitter-group publisher landed, decided from the run's own summaries. Both paths exit nonzero and record `failure_policy` in the audit summary. The `hold` policy is wired live in [brontes `.anodizer.yaml`](https://github.com/tj-smith47/brontes/blob/master/.anodizer.yaml) (`release.on_failure: hold` — tag-triggered pipeline with no bump commit to revert). The failure path fired live in the failed v0.15.1 publish ([run 28809062839](https://github.com/tj-smith47/anodizer/actions/runs/28809062839)): rollback executed in-process and the publish-only hold rule kept the pre-existing tag + bump commit in place (`publish-only run failed — holding the already-released tag and version-bump commit in place …`); the explicit `hold` policy value and the degrade-past-burned-doors branch remain test-proven only |
| Machine-readable exit contract (`exit 2` + `anodizer-error-class: deterministic`) | ✅ Verified (tests) | [`crates/cli/tests/exit_class_integration.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/tests/exit_class_integration.rs) — config/CLI/flag errors that retrying can never fix exit `2` and stamp a stderr marker; transient failures keep exit `1`. Retry wrappers (including anodizer-action) key off both. Consistent live: the failed v0.15.1 publish ([run 28809062839](https://github.com/tj-smith47/anodizer/actions/runs/28809062839)) exited `1` (transient publisher failure); the deterministic exit-`2` branch has not fired live |
| Run-wide retry budget bounding publisher ladders | ✅ Verified (tests) | [`crates/core/src/config/retry.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/core/src/config/retry.rs) — a `retry.max_elapsed` wall-clock budget (default 15 min, raisable) bounds every publisher's retry ladder and the deadline-aware HTTP retry wrappers, so a transient storm fails cleanly instead of multiplying per-publisher ladders. Wired live in [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`retry.max_elapsed: 15m`); a live retry ladder ran under it at v0.15.5 ([run 28882554907](https://github.com/tj-smith47/anodizer/actions/runs/28882554907), `snapcraft upload attempt 1/10 failed (5xx), retrying…`) — the budget *ceiling* itself has never been reached |
| Retry backoff attribution in the run summary | ✅ Verified (tests) | [`crates/core/src/retry.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/core/src/retry.rs) + [`crates/stage-publish/src/run_summary.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-publish/src/run_summary.rs) — total backoff wait is accounted run-wide and attributed per publisher/stage in the summary, so a slow release is explainable from the audit artifact. v0.15.5's snapcraft retries ([run 28882554907](https://github.com/tj-smith47/anodizer/actions/runs/28882554907)) accrued real backoff, but that run's `summary.json` (written by the pre-attribution v0.15.4 binary) carries no backoff fields — the attribution landed after v0.15.5 and awaits the next release for live proof |
| Liveness heartbeat during slow subprocess waits | ✅ Verified (tests) | [`crates/core/src/progress.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/core/src/progress.rs) — long-running stage subprocesses emit a `still …` line on a fixed cadence so a legitimate slow wait is distinguishable from a hang; cadence override/disable via env |
| Snap Store review-hold surfacing | ✅ Verified (tests) | [`crates/stage-snapcraft/src/publish_stage.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-snapcraft/src/publish_stage.rs) — an upload parked in manual review now reports **HELD** at default visibility (per upload + end-of-stage rollup) and stamps `held_for_review` on the evidence snapshot, instead of riding the success path. Motivated by a real incident: cfgd v0.5.0 went green while the store stayed at 0.3.5 |
| Snap Store channel-map landing probe (`verify_release`) | ✅ Verified (tests) | [`crates/stage-verify-release/src/snap_store.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-verify-release/src/snap_store.rs) — verify-release asserts each uploaded snap is actually live in the store's channel map, catching a review hold that parked the revision outside every channel. Not yet exercised live: at v0.15.5 ([run 28882554907](https://github.com/tj-smith47/anodizer/actions/runs/28882554907)) the snap upload itself failed (Store 5xx + upload-uniqueness error), so verify-release had no landed snap to probe |
| Landed release-asset verification | ✅ Verified | Live at v0.15.5 ([run 28882554907](https://github.com/tj-smith47/anodizer/actions/runs/28882554907)): with `verify_release.assert_assets: true` the verify stage fetched the published asset set via [`crates/stage-release/src/github/lookup.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-release/src/github/lookup.rs) (`fetch_published_assets`) and re-checked the landed set — `Verifying release … all post-publish checks passed`, summary line `verify-release  passed` |
| Emission-validate shard accountability | ✅ Verified (tests) | [`crates/stage-publish/src/snapshot_validation.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-publish/src/snapshot_validation.rs) — sharded / target-restricted builds emit an aggregate `validated emissions … (skipped M expectations …)` result line via a per-expectation skip tally, so a partial-target validation is never mistaken for a full-set one; dry-run URL derivation validates publish URLs without network |

Test-harness-only flags (`--simulate-failure`, `--inject-drift`) are intentionally omitted from this matrix — they exist for regression coverage only and require `ANODIZE_TEST_HARNESS=1` to be honored. Operators will never run them in production.

## Build determinism

Byte-stability contract plus a `check determinism` harness, an operator
`--allow-nondeterministic <name>=<reason>` escape, and a release-body
"Non-deterministic exemptions:" block that lists any waived artifacts. Merged
2026-05-14; rows fill in as v0.2.x+ releases exercise each surface.

| Key | Status | Notes |
|---|---|---|
| `anodize check determinism --runs=N` harness | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/v0.16.0/.github/workflows/release.yml) (`determinism-check:` calls the reusable [`determinism.yml`](https://github.com/tj-smith47/anodizer/blob/v0.16.0/.github/workflows/determinism.yml) 4-shard matrix on every tag; the `release:` job consumes the preserved dist via `release --publish-only`) |
| `anodize check config` (post-restructure) | 🤝 Help wanted | [`crates/cli/src/commands/check/config.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/check/config.rs) - post-restructure config validator; no release has exercised the new surface yet |
| `--allow-nondeterministic <name>=<reason>` | 🤝 Help wanted | Operator escape parsed and threaded through the build stage; rejection paths covered by `crates/cli/tests/integration.rs::release_allow_nondeterministic_rejects_no_eq`, `release_allow_nondeterministic_rejects_empty_reason`, `release_strict_conflicts_with_allow_nondeterministic`; no live release has waived an artifact yet |
| "Non-deterministic exemptions:" block in release body | 🤝 Help wanted | [`crates/stage-release/src/release_body.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-release/src/release_body.rs) - emitter wired; release body fragment unused until an exemption ships |
| `--inject-drift=archive\|sbom` test seam (`ANODIZE_TEST_HARNESS=1` gated) | ✅ Verified (tests) | [`crates/cli/src/determinism_harness/drift.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/determinism_harness/drift.rs) (`inject_drift_byte`) + env-gate in [`crates/cli/src/commands/check/determinism.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/check/determinism.rs) — end-to-end drift detection covered by `crates/cli/tests/check_determinism.rs::inject_drift_archive_reports_drift_on_minimal_workspace` and the unit-level mutation seam at `determinism_harness::drift::tests::inject_drift_byte_mutates_file_so_hash_differs` |
| Snapshot `SOURCE_DATE_EPOCH` resolver | ✅ Verified (tests) | [`crates/core/src/git/snapshot_sde.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/core/src/git/snapshot_sde.rs) (`resolve_snapshot_sde`) — env-override / clean-tree-HEAD / dirty-tree-hash / stability branches covered by `snapshot_sde_uses_env_var_when_set`, `snapshot_sde_uses_head_when_tree_clean`, `snapshot_sde_uses_dirty_tree_hash_when_tree_dirty`, `snapshot_sde_is_stable_for_unchanged_dirty_tree` (commit `5ad6a76`) |
| SBOM byte-stability under `SOURCE_DATE_EPOCH` | ✅ Verified (tests) | [`crates/stage-sbom/src/lib.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-sbom/src/lib.rs) — CycloneDX output byte-stable for the same timestamp + honors / varies with SDE; coverage via `cyclonedx_output_byte_stable_for_same_timestamp`, `sbom_metadata_timestamp_honors_sde`, `sbom_metadata_timestamp_varies_with_sde` (commit `4a34d1a`) |

## Announcers

13 channels implemented. Two are exercised by live cfgd releases; the
others have full test coverage but no live secrets configured.

| Key | Status | Notes |
|---|---|---|
| `announce.webhook` | ✅ Verified | [cfgd `.anodizer/announce.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer/announce.yaml) (`announce.webhook.endpoint_url: https://tj.jarvispro.io/webhooks/anodizer` — cfgd's announce matrix moved into an `includes:` fragment) |
| `announce.smtp` | ✅ Verified | [cfgd `.anodizer/announce.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer/announce.yaml) (`announce.email.host: smtp.gmail.com`) |
| `announce.discord` | 🤝 Help wanted | No live workflow has the secrets |
| `announce.slack` | 🤝 Help wanted | No live workflow has the secrets |
| `announce.telegram` | 🤝 Help wanted | No live workflow has the secrets |
| `announce.teams` | 🤝 Help wanted | No live workflow has the secrets |
| `announce.mattermost` | 🤝 Help wanted | No live workflow has the secrets |
| `announce.reddit` | 🤝 Help wanted | No live workflow has the secrets |
| `announce.twitter` | 🤝 Help wanted | No live workflow has the secrets |
| `announce.mastodon` | 🤝 Help wanted | No live workflow has the secrets |
| `announce.bluesky` | 🤝 Help wanted | No live workflow has the secrets |
| `announce.linkedin` | 🤝 Help wanted | No live workflow has the secrets |
| `announce.opencollective` | 🤝 Help wanted | No live workflow has the secrets |
| `announce.discourse` | 🤝 Help wanted | No live workflow has the secrets |

## Blob and artifactory uploads

| Key | Status | Notes |
|---|---|---|
| `blobs[]` (S3 / GCS / Azure) | ✅ Verified | The S3 provider runs live on every anodizer release against a self-hosted MinIO: v0.15.5 ([run 28882554907](https://github.com/tj-smith47/anodizer/actions/runs/28882554907)) logged `uploaded 47 object(s), skipped 0 (identical) → s3://…/v0.15.5`, and the run summary records per-object `s3://anodizer-releases/v0.15.5/…` evidence. GCS and Azure providers share the `object_store` code path but have no live deployment |
| `artifactories[]` | 🤝 Help wanted | Target, mode, TLS, headers wired and configured (disabled) in [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`artifactories: … skip: true` — no Artifactory instance); live runs evaluate the entry and skip it (`skipped artifactory entry 'production' — skip condition evaluated truthy`) |
| `uploads[]` | ✅ Verified | Live on every anodizer release: the `jarvispro` entry in [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) HTTP-PUTs the CLI archives + checksums to a self-hosted mirror — v0.15.5 ([run 28882554907](https://github.com/tj-smith47/anodizer/actions/runs/28882554907)) logged `uploading 3 artifacts to 'jarvispro' (mode=archive)` → `uploaded 3 artifact(s), skipped 0 (already present) → jarvispro`, summary `uploads  Assets  optional  succeeded` |
| `uploads[].exclude` / per-destination exclude globs | ✅ Verified (tests) | [`crates/core/src/config/upload.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/core/src/config/upload.rs) (`exclude: ["*.sha256", "*.sig", ...]` drops matching assets for one destination without touching the others). No live deployment |
| `gemfury[]` (alias `furies[]`) | 🤝 Help wanted | The publisher live-fired at v0.15.1 ([run 28809062839](https://github.com/tj-smith47/anodizer/actions/runs/28809062839)): deb/rpm/apk pushes to `https://push.fury.io/tj-smith47` were dispatched but every push 403'd (`account access denied` — the free-tier account is over quota), which is the required failure that triggered that run's rollback. The entry is now disabled in [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`gemfury: … skip: true`); a successful live publish needs a paid Fury plan. See [`gemfury:` docs](../../../docs/publish/gemfury/) |
| `cloudsmiths[]` | ✅ Verified | The [jarvispro/anodizer Cloudsmith repo](https://cloudsmith.io/~jarvispro/repos/anodizer/packages/) carries live `anodizer` packages in all three configured formats (deb + rpm + alpine, at the latest release version; via `GET /v1/packages/jarvispro/anodizer/`). Wired in [anodizer's config](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`cloudsmiths:` organization `jarvispro`, repository `anodizer`, per-format distributions). Real `DELETE` rollback is test-covered (see Release resilience below). See [`cloudsmiths:` docs](../../../docs/publish/cloudsmith/) |

## Custom publishers

| Key | Status | Notes |
|---|---|---|
| `publishers[]` | ✅ Verified | [`crates/cli/src/commands/publisher.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/publisher.rs) (custom command per artifact) |
| Submission attribution (PR footers + generated-file headers) | ✅ Verified | The generated-file header is live in the tap: [Casks/anodizer.rb](https://github.com/tj-smith47/homebrew-tap/blob/master/Casks/anodizer.rb) (v0.16.0) opens with `# This file was generated by anodizer (https://github.com/tj-smith47/anodizer). DO NOT EDIT.`. [`crates/stage-publish/src/util/attribution.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-publish/src/util/attribution.rs); the `Automatically submitted by anodizer` PR footer lands on the next external-index submission PR |

## MCP registry

Publishes an MCP server manifest to `https://registry.modelcontextprotocol.io`.
The manifest points at `ghcr.io/tj-smith47/anodizer:<version>`, the multi-arch
OCI image built by [`dockers_v2:`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml).
The image's `ENTRYPOINT` runs the `anodizer` binary and `CMD` defaults to
`mcp` (see [`Dockerfile`](https://github.com/tj-smith47/anodizer/blob/master/Dockerfile)),
so consumers `docker run --rm -i ghcr.io/tj-smith47/anodizer:<ver>` and the
container speaks MCP over stdio out of the box.

The previous "blocked on `dockers:`" status reflected anodizer's lack of an
OCI image; commit `41947cb` shipped both the `dockers_v2:` block and the
`Dockerfile` that unblocks it. The `mcp:` block no longer carries `skip: true`
— the next release after this commit lands publishes the manifest live.

| Key | Status | Notes |
|---|---|---|
| `mcp.name` | ✅ Ready | Wired in [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`name: io.github.tj-smith47/anodizer`); next release publishes |
| `mcp.packages[]` | ✅ Ready | Wired in [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`packages[].registry_type: oci`, `identifier: ghcr.io/tj-smith47/anodizer`); image built by [`dockers_v2:`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) |
| `mcp.auth.type: github-oidc` | ✅ Ready | [`crates/stage-publish/src/mcp/auth.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-publish/src/mcp/auth.rs) (OIDC id-token branch); release workflow declares `id-token: write` and `packages: write` permissions |
| `mcp.auth.type: none` | ✅ Verified (tests) | [`crates/stage-publish/src/mcp/auth.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-publish/src/mcp/auth.rs) (None branch) — unit-tested; private mirrors only |
| `mcp.auth.type: github` | ✅ Verified (tests) | [`crates/stage-publish/src/mcp/auth.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-publish/src/mcp/auth.rs) (PAT exchange branch) — unit-tested; for non-GHA CI |
| `mcp.repository` | ✅ Ready | [`crates/stage-publish/src/mcp/manifest.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-publish/src/mcp/manifest.rs) — inferred from release context (omitted from anodizer config; uses defaults) |
| `mcp.skip` (tera, accepts `disable:` alias) | ✅ Verified (tests) | [`crates/stage-publish/src/mcp/mod.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-publish/src/mcp/mod.rs) — unit-tested; not used in production (block is unconditionally enabled) |
| OCI `version` field omitted | ✅ Verified (tests) | Per commit `596e1a3`: OCI registry types get an empty `version` field on the published manifest — the registry resolves the version from the image tag. Other registry types (npm, pypi, ...) receive the release version verbatim |
