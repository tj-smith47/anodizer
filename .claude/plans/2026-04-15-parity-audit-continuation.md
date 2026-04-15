# Parity Audit Continuation — Handoff (2026-04-15)

## Context

A 5-batch GoReleaser parity audit surfaced ~80 findings. 31 fixes landed this session; ~30 items remain open in `.claude/known-bugs.md`. cfgd's v0.3.5 release exposed that many users hit these bugs. **cfgd cannot ship v0.3.6 until chocolatey and other known-bug-gated items are fixed.**

**Critical lesson from this session**: GoReleaser's canonical field names are authoritative. Do NOT deprecate a GoReleaser field name in favor of an anodize-specific rename — that pushes users away from drop-in compat. When in doubt, grep `pkg/config/config.go` for the `yaml:"..."` tag to see GoReleaser's canonical spelling.

## What's resolved

See `.claude/known-bugs.md` → `## Resolved` section dated 2026-04-15. In summary:
- `MonorepoConfig` / `WorkspaceConfig` got `deny_unknown_fields`.
- `HooksConfig.pre` → `HooksConfig.hooks` (canonical rename, no alias) + `HomebrewConfig.folder` → `HomebrewConfig.directory`. Matches GoReleaser.
- Deprecation detection wired for `snapshot.name_template`, `nfpms[].builds`, `snapcrafts[].builds` (all correct-direction GoReleaser deprecations).
- 13 release/sign/changelog/sbom/announce/snap/docker/publisher fixes (see `known-bugs.md` for details).
- 15 build/archive/source/nfpm/makeself/checksum fixes.
- 4 CLI/pipeline fixes (build pipeline additions, project_name inference, skip=before).

cfgd's `.anodize.yaml` updated: `folder: Formula` → `directory: Formula`. Validates clean with zero deprecations.

Test baseline: 3148 tests passing (unchanged count; some rewrote, net-new coverage added).

## What's still open (priority order)

Open items live in `.claude/known-bugs.md` → `## Active`. Grouped by urgency:

### Tier 1 — Correctness bugs that affect cfgd releases

1. **GitLab JOB-TOKEN safety** — `stage-release/src/gitlab.rs`. Only send `JOB-TOKEN` header when the token matches `CI_JOB_TOKEN`. Failing auth otherwise.
2. **Release `include_meta` semantics** — `stage-release/src/lib.rs:1160-1165`. anodize always uploads `Metadata` kind; GoReleaser only uploads when `include_meta: true`. Also anodize reads files from disk; GoReleaser uploads the `Metadata`-typed artifact. Rework needed.
3. **Sign `artifacts: all` scope** — `stage-sign/src/lib.rs`. Align with GoReleaser `ReleaseUploadableTypes` list (use `anodize_core::artifact::release_uploadable_kinds()` which this session added to `core/src/artifact.rs`). Currently over-matches (Snap/Installer/DiskImage) and under-matches (missing Signature/Certificate).
4. **Milestone repo/provider resolution** — `cli/src/commands/release/milestones.rs:112-139,143-165`. First-crate fallback is wrong for mixed-provider configs. Use `ctx.token_type` first, then fall back.
5. **AUR URL `${pkgver}` shell-substitution in Rust** — `stage-publish/src/aur.rs:153-164`. GoReleaser preserves the literal URL. Anodize substitutes the version string with `${pkgver}` — fine for simple URLs but breaks when the version appears inside a path segment that shouldn't be variabilized.
6. **AUR push branch** — `stage-publish/src/aur.rs:552`. Hardcode `"master"` (AUR repos require master; currently relies on clone default).

### Tier 2 — Publisher / stage correctness

7. **Krew description/short_description required validation** — prior session claimed fixed; re-verify.
8. **Custom publisher default filter scope** — `cli/src/commands/publisher.rs:291-332`. GoReleaser curates a 10-type list; anodize allows all non-Metadata. Narrow to match.
9. **SBOM dedup for UniversalBinary + source Darwin binaries** — `stage-sbom/src/lib.rs:391-414`. Currently 3 SBOMs for 1 conceptual artifact.
10. **Docker ID uniqueness validation** — D1/D2/D3 claimed done in prior sessions; re-verify with code.
11. **Docker V2 retry scope** — `stage-docker/src/lib.rs`. Narrow to manifest-verification errors only.
12. **Docker `manifest rm` error tolerance** — `stage-docker/src/lib.rs`. Ignore all errors from `docker manifest rm`; anodize currently bails on non-"not found".

### Tier 3 — Config completeness

13. **`ctx.deprecate` remaining alias sites** — `cli/src/pipeline.rs::detect_deprecated_aliases`. The current list is small (snapshot, nfpms/snapcrafts builds, gemfury). Sweep `config.rs` for ALL `#[serde(alias = ...)]` attributes and decide per-site: is this an anodize rename of a GoReleaser-deprecated field (keep + warn) or an anodize-specific convenience alias (just accept, no warn).
14. **nfpm Deb `arch_variant`** — `core/src/config.rs:2991-3008` (NfpmDebConfig) + `stage-nfpm/src/lib.rs:222-239` (NfpmYamlDeb). Add `arch_variant: Option<String>` to both; pass `Goamd64` from platform metadata so v2/v3 variants encode correctly.
15. **nfpm `ConventionalFileName` template var matches nfpm's per-format logic** — `stage-nfpm/src/lib.rs:1256-1259`. Currently hand-rolled; deb/rpm/apk/archlinux/ipk all have different naming rules + arch translations. Either shell out to nfpm for the name, or reimplement the per-packager logic (tedious but mechanical — see nfpm v2.44 source for spec).
16. **UniversalBinary full defaults pass** — prior session added `id` field. Verify `ids` defaulting to `[id]` works in all code paths.

### Tier 4 — Nice-to-haves (not blocking cfgd)

17. **Parallelism** — nfpm/snapcraft/makeself/flatpak/blob loops serial. Wire `ctx.parallelism` with `std::thread::scope` chunks (pattern exists in `stage-upx/src/lib.rs`).
18. **tag_pre_hooks / tag_post_hooks** (Pro) — not critical for cfgd.
19. **Nightly `name_template` default** (Pro).
20. **Git snapshot short-circuit** — `core/src/git.rs:245`. `rev-parse HEAD` bubbles in non-repo snapshot mode.

### Known false positives (do NOT waste time on)
- `Now.Format` — works via `template_preprocess.rs:770` preprocessor.
- `github_urls.skip_tls_verify` — fully wired in `stage-release/src/lib.rs:770-876` (`build_octocrab_client_insecure`).
- `ANODIZE_CURRENT_TAG` + HEAD validation — matches GoReleaser (their validate runs unconditionally outside snapshot/skip-validate).
- `--skip=unknown` — already errors in `main.rs:133-136,188-191`; only dead warn-loop remains at `pipeline.rs` (cleanup lower priority).
- Homebrew Goarm default "6" — matches GoReleaser `experimental.DefaultGOARM`.
- HTTP upload retry for artifactory/fury/cloudsmith/upload — GoReleaser doesn't retry these either.
- Mastodon form-encoded POST — matches go-mastodon library.
- B6 Archive ids filter — matches GoReleaser.
- B8 Artifact paths absolute — matches GoReleaser.

## Working constraints (read before editing)

- `.claude/hooks/post-edit.sh` flags any file containing `.unwrap()/.expect()` in non-test code or `anyhow::` outside `main.rs`/`bin/`. These are pre-existing violations; hook exit 2 is informational and the edit still applies. **Don't spend scope expanding to clean up unrelated violations** unless the user asks.
- GoReleaser reference at `/opt/repos/goreleaser`. `pkg/config/config.go` is the source-of-truth for canonical field names. `internal/pipe/<pipe>/<pipe>.go` files are per-stage behavior.
- cfgd at `/opt/repos/cfgd` with `.anodize.yaml` as integration test. After each significant anodize change, rebuild `anodize` (`cargo build --release --bin anodize`) and run `anodize check` from `/opt/repos/cfgd` to validate.
- Push is blocked while `known-bugs.md` has unchecked items. Don't fight it — fix the items.

## Execution approach

For each finding:
1. Read GoReleaser's source for the behavior.
2. Read anodize's current code.
3. Verify the finding is real (prior audits had ~30% false positive rate when spot-checked).
4. Apply fix.
5. Update the known-bugs entry to `Resolved` with date + brief note.
6. After every 3-5 fixes, run `cargo check --workspace && cargo test --workspace`.
7. After significant config-surface changes, `anodize check` from `/opt/repos/cfgd`.
8. When Tier 1 is empty, ask the user if they want to cut v0.3.6 or continue into Tier 2.

## Session artifacts

- Audit aggregation: `/tmp/parity-audit-findings/aggregated.md` (may be gone next session — regenerate from `known-bugs.md` + git log if needed).
- Resolved items ledger: `.claude/known-bugs.md` → `## Resolved` section.
- Test baseline: 3148 passing at session start, 3148 at session end (same count, net-new tests added, some refactored).

## Entry point for next session

```
/pickup
```

Then read this file and pick up at Tier 1 item 1 (GitLab JOB-TOKEN).
