# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.20.0] - 2026-07-14

### Features

* 2e8e602ba1b5 add split_format (bare|coreutils) for shasum -c sidecars ([@tj-smith47](https://github.com/tj-smith47))

---
### Others

* c3d93f5b31a0 resume a completable partial crates.io publish instead of aborting ([@tj-smith47](https://github.com/tj-smith47))

## [0.19.0] - 2026-07-13

### Features

* bb3eb89d0e08 batteries-included curl|sh installer on the installer.rs engine ([@tj-smith47](https://github.com/tj-smith47))
* 664fae3e2dc2 cross-publisher track promotion without rebuild ([@tj-smith47](https://github.com/tj-smith47))

---
### Bug Fixes

* bbbae08a82f0 tag fully-static linux-gnu binaries as manylinux, not hard-error ([@tj-smith47](https://github.com/tj-smith47))
* 363702951032 run before-hooks in --split and --merge modes ([@tj-smith47](https://github.com/tj-smith47))

## [0.18.0] - 2026-07-13

### Features

* 83cac3f9df1c finish the formula-bump publisher — tests, docs, indexes ([@tj-smith47](https://github.com/tj-smith47))
* 2564553652da skip_metapackage + platform_name_template for platform-only distribution ([@tj-smith47](https://github.com/tj-smith47))
* ce5f3a6c87e6 crates.io OIDC Trusted Publishing; split cargo.rs into cargo/ ([@tj-smith47](https://github.com/tj-smith47))
* 7e09a5fd8ab9 npm multi-command bins, PyPI wheels + Trusted Publishing (OIDC), homebrew-core ([@tj-smith47](https://github.com/tj-smith47))
* 539b0ec40309 native binary wheels + maturin sdist publisher ([@tj-smith47](https://github.com/tj-smith47))
* 9b34a450dcb2 sign version tags with the git-config signing key ([@tj-smith47](https://github.com/tj-smith47))

---
### Bug Fixes

* a4691ee79b19 drain review findings — git-form bump correctness, lazy repo_info, rollback token fidelity, npm preflight+token ([@tj-smith47](https://github.com/tj-smith47))
* 131f29bc02a7 review fixes — single-source naming vars, uniform validation, mode gate, preflight skip_metapackage ([@tj-smith47](https://github.com/tj-smith47))
* ba3d5eaa50f8 homebrew git-form detection survives an inline-commented url line ([@tj-smith47](https://github.com/tj-smith47))
* d3c57c68a8ef make partial-rollback test hermetic #none ([@tj-smith47](https://github.com/tj-smith47))
* 99e25c964db7 regroup npm + pypi to Submitter so the rollback guard sees their burn #C14 ([@tj-smith47](https://github.com/tj-smith47))
* 640f60c59a87 drain review findings — tags, metadata, uploads, shared helpers ([@tj-smith47](https://github.com/tj-smith47))
* f5b54152c8cf close three burn-guard false-negatives that let a poisoning re-cut through ([@tj-smith47](https://github.com/tj-smith47))
* 689eb3306434 probe npm + pypi for burned versions in the unsummarized guard path ([@tj-smith47](https://github.com/tj-smith47))

## [0.17.0] - 2026-07-12

### Features

* 5a21a5ae1464 fail fast on provably-doomed plain-cargo cross gnu builds ([@tj-smith47](https://github.com/tj-smith47))
* 9d14a5a2e8c5 msix format and Termux-native arch naming (GoReleaser cd5f16b, 99a7173) ([@tj-smith47](https://github.com/tj-smith47))
* c9c9cc872257 mirror GoReleaser upstream — docker build retry breadth, release-repo token, versioned PR branches, winget default_locale ([@tj-smith47](https://github.com/tj-smith47))

---
### Bug Fixes

* 247f062ef2d3 dry-run must not abort on a doomed cross-gnu plan ([@tj-smith47](https://github.com/tj-smith47))
* 42c014f216b6 live-acceptance fixes for termux.deb and msix ([@tj-smith47](https://github.com/tj-smith47))
* e817829d897b gate verify-release install-smoke to OS-package publishers ([@tj-smith47](https://github.com/tj-smith47))
* 97a05071b795 gate verify/preflight axes to the selected publish surface ([@tj-smith47](https://github.com/tj-smith47))
* e5dbbefbc069 one gpg predicate, a faked-system-time preflight, and honest skips ([@tj-smith47](https://github.com/tj-smith47))
* fda1fdfb3a22 close the surface-gating and vacuous-verdict holes #minor ([@tj-smith47](https://github.com/tj-smith47))
* 25ee7153d259 stamp no verdict when the OS-package axes inspect zero packages ([@tj-smith47](https://github.com/tj-smith47))
* 4141cd8ae479 remove the vestigial gpg --faked-system-time preflight probe ([@tj-smith47](https://github.com/tj-smith47))

## [0.16.1] - 2026-07-10

### Bug Fixes

* 373b8af9f5c2 chocolatey nuspec drops the unsupported license element (CHCU0002) ([@tj-smith47](https://github.com/tj-smith47))
* 7400c1241b67 scoop manifests default into bucket/ (root manifests are invisible to scoop) ([@tj-smith47](https://github.com/tj-smith47))
* 3a576f321ade scope preflight state-probes to selected publishers ([@tj-smith47](https://github.com/tj-smith47))

## [0.16.0] - 2026-07-10

### Features

* dd75f87b05e7 classify deterministic failures with a machine-readable exit contract ([@tj-smith47](https://github.com/tj-smith47))
* 934cd7f64880 liveness heartbeat during slow subprocess waits ([@tj-smith47](https://github.com/tj-smith47))
* ea0cdaef2174 gate one-way-door publishers on burn probes and changelog provenance ([@tj-smith47](https://github.com/tj-smith47))
* 16b341e40f6b link anodizer in submission footers and generated-file headers ([@tj-smith47](https://github.com/tj-smith47))
* 380c392d8066 verify landed release assets and harden the tagless build path ([@tj-smith47](https://github.com/tj-smith47))
* c59a4b26df1e account run-wide retry backoff and surface it in the summary ([@tj-smith47](https://github.com/tj-smith47))
* f7279ea0fe38 add RetryStep step-retry engine with unified log lifecycle ([@tj-smith47](https://github.com/tj-smith47))
* 48e34f50791e add deadline-aware HTTP retry wrappers ([@tj-smith47](https://github.com/tj-smith47))
* f17957d2cd56 attribute retry backoff per publisher/stage in the summary ([@tj-smith47](https://github.com/tj-smith47))
* 333b361f22bc bound publisher retry ladders by the run's retry budget ([@tj-smith47](https://github.com/tj-smith47))
* e13ef744a394 resolve a raisable default retry budget and add an async deadline ([@tj-smith47](https://github.com/tj-smith47))
* 8ee354f2fb5e host-level TUF warm-up lock and warm-cache fast path ([@tj-smith47](https://github.com/tj-smith47))
* 79c859dadc5a surface Snap Store review holds and verify store landing ([@tj-smith47](https://github.com/tj-smith47))
* b292588a2a0d wire the liveness heartbeat into slow stage waits ([@tj-smith47](https://github.com/tj-smith47))
* daf93cbb6e42 emission-validate accountability on shards and dry-run URL derivation ([@tj-smith47](https://github.com/tj-smith47))

---
### Bug Fixes

* 77f4885d8f96 re-verify cargo publish discriminators on 1.97 and deflake the ticker test ([@tj-smith47](https://github.com/tj-smith47))
* c04a6eaa151c force 0755 on binaries staged into the dockers_v2 build context ([@tj-smith47](https://github.com/tj-smith47))
* 3e99866f4ee7 accept crates.io policy-denial 403s as proof of token validity ([@tj-smith47](https://github.com/tj-smith47))
* 2c48944c6857 harden tagging, rollback, retry visibility, and failure-path reporting ([@tj-smith47](https://github.com/tj-smith47))
* 96e42bde6d5b make bare `tag` fully local in every config mode ([@tj-smith47](https://github.com/tj-smith47))
* d1a235ad9bd8 attribute release cleanup on rollback and keep API tagging local when not pushing ([@tj-smith47](https://github.com/tj-smith47))
* f38944515e47 gate snapcraft review-hold stub tests to unix ([@tj-smith47](https://github.com/tj-smith47))
* ded0164d4a08 distinguish indeterminate landing probes and refuse identity-widening keyless verify ([@tj-smith47](https://github.com/tj-smith47))
* 402a9ea7d663 route all six retry forks through the shared step engine ([@tj-smith47](https://github.com/tj-smith47))

## [0.15.5] - 2026-07-07

### Bug Fixes

* 22571b14e0d6 declare every run-path tool in publisher requirements ([@tj-smith47](https://github.com/tj-smith47))

## [0.15.4] - 2026-07-07

### Bug Fixes

* 3afd0f214e79 serialize keyless cosign TUF init and retry transient failures with jittered backoff ([@tj-smith47](https://github.com/tj-smith47))

## [0.15.3] - 2026-07-07

### Bug Fixes

* edaa7f88cb07 make same-version re-cuts survivable end to end ([@tj-smith47](https://github.com/tj-smith47))

## [0.15.1] - 2026-07-06

### Bug Fixes

* 20f3db081207 allow http scheme in blob rollback delete client ([@tj-smith47](https://github.com/tj-smith47))
* 72daf41a9c1a anchor .cargo_vcs_info.json normalization to the crate root ([@tj-smith47](https://github.com/tj-smith47))
* 68f887b3f747 bound npm retry wall-time under the publish-npm job timeout ([@tj-smith47](https://github.com/tj-smith47))
* 4abf66e56f7d compare re-cut crates modulo .cargo_vcs_info.json so same-source re-cuts skip clean ([@tj-smith47](https://github.com/tj-smith47))
* 57eb14a1345a count only Normal path components for the crate-root vcs-info gate ([@tj-smith47](https://github.com/tj-smith47))
* 5eb94287af6b give git-revert rollback a commit identity ([@tj-smith47](https://github.com/tj-smith47))
* 38229cd0a3c9 skip schemastore schemas not owned by the current publish leg ([@tj-smith47](https://github.com/tj-smith47))
* 5e74097f11e9 surface npm retry-budget exhaustion and correct the metapackage install docs ([@tj-smith47](https://github.com/tj-smith47))
* 034b9b0f171e neutralize git config before clone in revert no-identity test ([@tj-smith47](https://github.com/tj-smith47))

## [0.15.0] - 2026-07-06

### Features

* 5f37d22b9b7c add per-publisher on_rollback failure hook ([@tj-smith47](https://github.com/tj-smith47))
* a54143cb7658 expose rollback trigger reason to on_rollback hooks ([@tj-smith47](https://github.com/tj-smith47))

---
### Bug Fixes

* 42790451f866 provision clang-cl+nasm defensively; correct guard comment; drop stray test assert ([@tj-smith47](https://github.com/tj-smith47))
* 2f8ed2c7823b keep --split --single-target composable (revert over-broad conflict) ([@tj-smith47](https://github.com/tj-smith47))
* a36a24b4db7d make required_failure_reason message extraction exhaustive ([@tj-smith47](https://github.com/tj-smith47))
* fe42673455fc add clang-cl C-toolchain pin primitive for windows-msvc ([@tj-smith47](https://github.com/tj-smith47))
* 75e77b27cc64 pin clang-cl in stage-build for windows-msvc release builds ([@tj-smith47](https://github.com/tj-smith47))
* 2d6e41c9bff1 pin clang-cl in the harness child env + hard-require it for windows-msvc ([@tj-smith47](https://github.com/tj-smith47))
* dcc962211412 per-shard self-skip, not wholesale skip ([@tj-smith47](https://github.com/tj-smith47))
* 38b618520256 OS-filter formula artifacts + gate emission-validate skip on partial shard ([@tj-smith47](https://github.com/tj-smith47))
* bbe4ef0623f1 wire the documented $ANODIZER_ARTIFACT env channel ([@tj-smith47](https://github.com/tj-smith47))
* 48f914cd3627 close AUR + homebrew failure-hiding on full builds lacking eligible archives ([@tj-smith47](https://github.com/tj-smith47))
* c50e732284ca exclude Apple-non-macOS archives from nix/krew/cask/npm ([@tj-smith47](https://github.com/tj-smith47))
* d9ca19d88500 gate index/manifest validator no-artifact skips on restricted builds ([@tj-smith47](https://github.com/tj-smith47))
* ee51d58ccf79 gate nix validator no-artifact skip on restricted builds ([@tj-smith47](https://github.com/tj-smith47))
* 150b5c8936e1 drop dead per-target CARGO_TARGET_<T>_RUSTFLAGS injection ([@tj-smith47](https://github.com/tj-smith47))
* 742a31d93983 route cask darwin-selection through is_macos ([@tj-smith47](https://github.com/tj-smith47))

## [0.14.0] - 2026-07-04

### Features

* 9660d545de42 migrate tera 1.20 -> 2.0 via JSON-boundary adapter ([@tj-smith47](https://github.com/tj-smith47))
* 315fa84bc67f verify TLS against the system trust store (reqwest 0.13, object_store 0.14) ([@tj-smith47](https://github.com/tj-smith47))

---
### Bug Fixes

* 383235b500c4 register .AppImage.zsync sidecar so it ships ([@tj-smith47](https://github.com/tj-smith47))
* b6f025d9b09f centralize conventional classifier, config discovery, tag-prefix fallback, release-commit subjects ([@tj-smith47](https://github.com/tj-smith47))
* 9f8723d8164e bump previews root-level crates; unify config discovery + token hints ([@tj-smith47](https://github.com/tj-smith47))
* 98cb49f06f6b finish the crate-universe conversion across the core + CLI tier ([@tj-smith47](https://github.com/tj-smith47))
* b6ae0325207e one shared aggregate-name rule for tag/changelog; harden --crate surfaces ([@tj-smith47](https://github.com/tj-smith47))
* 073e80c430b2 scope --workspace runs to the workspace; validate --crate selections; aggregate mixed-shape shared-prefix tags ([@tj-smith47](https://github.com/tj-smith47))
* 735f4654b981 validate --crate selections everywhere, scope workspace runs, group tag aggregation by prefix ([@tj-smith47](https://github.com/tj-smith47))
* eec56c00e405 one GitHub rate-limit detector; stop misclassifying 403s ([@tj-smith47](https://github.com/tj-smith47))
* f5197c73a782 walk the crate universe in per-crate hooks + release notes; centralize find_crate and archive shaping ([@tj-smith47](https://github.com/tj-smith47))
* 41437e5eeda8 build configured dockers_v2 dockerfile + extra_files, not repo-root Dockerfile ([@tj-smith47](https://github.com/tj-smith47))
* 49e29fd9440f mirror child build's skip_stages in docker config resolve ([@tj-smith47](https://github.com/tj-smith47))
* 125c8ae4094d align config-time env projection with the build planner ([@tj-smith47](https://github.com/tj-smith47))
* 99d994fc8d40 archives render the group's amd64 micro-arch variant ([@tj-smith47](https://github.com/tj-smith47))
* d27bd23a3dac centralize arch-token and variant-var policies; kill doubled mips filenames ([@tj-smith47](https://github.com/tj-smith47))
* d3b6b5c0d217 derive amd64 micro-arch variant at config time; cross-check all derived-name consumers ([@tj-smith47](https://github.com/tj-smith47))
* eb92234d8cc5 mark defaulted variant selectors in no-match publisher errors ([@tj-smith47](https://github.com/tj-smith47))
* b7b85dab8544 type amd64_variant as enum + model process-env RUSTFLAGS tiers ([@tj-smith47](https://github.com/tj-smith47))
* cb6297f8e822 type the whole amd64_variant config domain as the Amd64Variant enum ([@tj-smith47](https://github.com/tj-smith47))
* 225dea7b49c7 unify micro-arch variant seeding on the core SSOT #minor ([@tj-smith47](https://github.com/tj-smith47))
* 1b1c7b2d05d4 extend guard_no_unrendered backstop to winget/krew/nix/npm ([@tj-smith47](https://github.com/tj-smith47))
* d75fed20e89d finish the crate-universe conversion past dispatch into publisher bodies ([@tj-smith47](https://github.com/tj-smith47))
* cff96ec574d8 hard-fail on residual template delimiters before an irreversible publish ([@tj-smith47](https://github.com/tj-smith47))
* ae88cbf6adfd one crate-universe SSOT; registry gates + required/retain collapse see workspace crates ([@tj-smith47](https://github.com/tj-smith47))
* 10b1f9d7db76 shared rollback candidacy unstrands RollbackSkippedNoScope; one required-failure gate ([@tj-smith47](https://github.com/tj-smith47))
* 51dc140c41f4 test winget installer guard, reorder before clone, cover npm metapackage guard ([@tj-smith47](https://github.com/tj-smith47))
* e92bd4895edd derive prepare-skip, side-effect, and preflight stage sets from one source ([@tj-smith47](https://github.com/tj-smith47))
* ce623c92c3ac guard GitLab link-probe token exposure, GHES-aware GitHub probes, honest skip logging ([@tj-smith47](https://github.com/tj-smith47))
* 7e96e82ebdf5 require scheme match in GitLab probe host guard ([@tj-smith47](https://github.com/tj-smith47))
* 1947f74e94e3 finish the crate-universe conversion across every stage run loop ([@tj-smith47](https://github.com/tj-smith47))
* 90c5e69e8205 align preprocessor string scanning to the engine's raw boundary rule ([@tj-smith47](https://github.com/tj-smith47))
* f29d0d5d0e75 complete tera 2.0 migration — homebrew .0 index, preprocessor .N compat, date filter ([@tj-smith47](https://github.com/tj-smith47))
* b980399e29b6 fold shim boundary scan into string_lit; multiline go-control regexes; escape table pipes ([@tj-smith47](https://github.com/tj-smith47))
* 4c257a4cb17c restore date filter timezone= kwarg (tera 1.x parity) ([@tj-smith47](https://github.com/tj-smith47))
* 49497b3b32bd tri-state tool probe SSOT, one PATH-lookup primitive, token hints rendered from the env ladder ([@tj-smith47](https://github.com/tj-smith47))
* 91f68dd106ec single toml→yaml transcode route + shared discovery walk ([@tj-smith47](https://github.com/tj-smith47))
* 5a05d31b1e43 one RepoProbe->PreflightCheck mapper for both preflights ([@tj-smith47](https://github.com/tj-smith47))
* 05149b2bf99d single github_api_base resolver in core::http ([@tj-smith47](https://github.com/tj-smith47))
* 4ef8d97c9062 centralize operator lines, dockerhub username ladder, retriable-status rule, run-dir prefix ([@tj-smith47](https://github.com/tj-smith47))
* cb9612953a87 one forge upload driver; fix GitLab resume idempotency ([@tj-smith47](https://github.com/tj-smith47))

## [0.13.1] - 2026-07-02

### Bug Fixes

* 9b771a74891d drop AppDir scaffolding from dist; zero-pad zsync MTime ([@tj-smith47](https://github.com/tj-smith47))
* 4bb830ab20d2 pin .zsync MTime to SOURCE_DATE_EPOCH for determinism ([@tj-smith47](https://github.com/tj-smith47))
* dbcaafe12b4a strict-fail vanished/unmatched archive entries; propagate ELF+scan errors #133 ([@tj-smith47](https://github.com/tj-smith47))
* 6859d34782de stop silently masking write/mtime/stdin/IO failures #134 ([@tj-smith47](https://github.com/tj-smith47))
* f44720f56268 thread allow_http through ClientOptions so disable_ssl endpoints work ([@tj-smith47](https://github.com/tj-smith47))
* a000fd2d767b size hdiutil image explicitly to prevent spurious ENOSPC ([@tj-smith47](https://github.com/tj-smith47))
* b7bbd986b35e derive curl-sh asset names from the engine, not hardcoded shell ([@tj-smith47](https://github.com/tj-smith47))
* cb1aa93565a6 declare the configured formatter binary in publisher requirements ([@tj-smith47](https://github.com/tj-smith47))
* f7fbbc6d637f stop cargo dry-run spawn tests flaking on ETXTBSY #129 ([@tj-smith47](https://github.com/tj-smith47))
* 013aa262affb cloudsmith unverifiable-checksum uploads; npm token/extra-file errors surface #132 ([@tj-smith47](https://github.com/tj-smith47))
* d23cbe334ae3 gate ALL one-way doors on a required failure, not just Submitter #F1 ([@tj-smith47](https://github.com/tj-smith47))
* 1bafb9503234 make blob rollback reachable via dedicated rollback_publishers ([@tj-smith47](https://github.com/tj-smith47))
* cb9e449df5ad nix ELF inspection failure must fail the publish, not drop autoPatchelfHook #133 ([@tj-smith47](https://github.com/tj-smith47))
* 7e91f26c2d94 render templated secret_name for dockerhub and gemfury ([@tj-smith47](https://github.com/tj-smith47))
* 1fccbe0c189b upload blob assets before the one-way-door publishers ([@tj-smith47](https://github.com/tj-smith47))
* 0eef96a22523 fail on unreadable checksum artifact; warn on write-path template fallback #131 ([@tj-smith47](https://github.com/tj-smith47))
* 9d4305869cbe propagate config+workspace load errors, never cut a wrong version silently #130 ([@tj-smith47](https://github.com/tj-smith47))
* 9b2f088e378e self-skip when github-release is deselected ([@tj-smith47](https://github.com/tj-smith47))
* 2811e400e06c eliminate failure-hiding across the repo (stubs, swallowed errors, auto-pass) ([@tj-smith47](https://github.com/tj-smith47))
* c757ec4d4c08 centralize three drifting patterns onto core types ([@tj-smith47](https://github.com/tj-smith47))
* fe804e899de8 single-source the ELF dynamic-linking probe, fix class-gate divergence #136 ([@tj-smith47](https://github.com/tj-smith47))
* df57ff9a89fa make StageId the bidirectional stage-vocabulary SSOT ([@tj-smith47](https://github.com/tj-smith47))
* f00908967618 single-source skip-upload log, token rollback, poll eligibility ([@tj-smith47](https://github.com/tj-smith47))
* efc0e586166b single-source the GitHub-publisher preflight loop and gh probe ([@tj-smith47](https://github.com/tj-smith47))
* 588a30ca06bb centralize octocrab transport-error classification ([@tj-smith47](https://github.com/tj-smith47))
* be134cdc2060 single-source dist sidecar basenames and the run-<id> dir ([@tj-smith47](https://github.com/tj-smith47))

## [0.13.0] - 2026-06-27

### Features

* d14eeca0e63f exclude glob filter to drop assets per destination ([@tj-smith47](https://github.com/tj-smith47))

---
### Bug Fixes

* a8b40e4b7b92 align Warning/Error/Note with the enclosing header, not body depth ([@tj-smith47](https://github.com/tj-smith47))
* d33dc3cc7f8d root-cause v0.12.3 publish hang + signing/upload UX ([@tj-smith47](https://github.com/tj-smith47))
* c97861d281e2 bound every remote subprocess and HTTP client against indefinite hangs ([@tj-smith47](https://github.com/tj-smith47))

## [0.12.3] - 2026-06-26

### Bug Fixes

* 94c4a1a32d5f bound the stage so a slow notification can never fail a published release ([@tj-smith47](https://github.com/tj-smith47))
* c3a4b7854ad1 only synthesize default --bin when a bin named after the crate exists #51 ([@tj-smith47](https://github.com/tj-smith47))
* ba075f01b083 kill the whole process subtree on a windows timeout ([@tj-smith47](https://github.com/tj-smith47))
* fabad9f2268d land the cargo group bump — provider, rsa vuln, schemars/toml/rcgen ([@tj-smith47](https://github.com/tj-smith47))
* f45b93b77bf5 pin lifecycle-script mtime so signed apk packages are reproducible ([@tj-smith47](https://github.com/tj-smith47))

## [0.12.2] - 2026-06-25

### Bug Fixes

* 3b6a859203ea exclude a cancelled determinism shard from the publish gate ([@tj-smith47](https://github.com/tj-smith47))
* 7dbfc22f759b notify must not require a clean tree or a tag at HEAD ([@tj-smith47](https://github.com/tj-smith47))
* 54dc39f5732e patch RUSTSEC advisories; add cargo-audit gate + dependabot ([@tj-smith47](https://github.com/tj-smith47))
* 55fe9c71e646 stop chocolatey 403 force-retry; surface the real registry error ([@tj-smith47](https://github.com/tj-smith47))
* d6f43db359fb github-release auto-resumes a stale leftover draft ([@tj-smith47](https://github.com/tj-smith47))
* e5cbdce13768 name the before-hook in logs; run env-preflight before hooks ([@tj-smith47](https://github.com/tj-smith47))
* bff0f192a82a push the revert commit before deleting the tag on rollback ([@tj-smith47](https://github.com/tj-smith47))

## [0.12.1] - 2026-06-24

### Bug Fixes

* 5cd973d4a46b publish-only log noise, hangs, dup artifacts, nsis reproducibility #patch ([@tj-smith47](https://github.com/tj-smith47))
* 9b2a2782daee pass bash ${#...} through Tera rendering like GoReleaser ([@tj-smith47](https://github.com/tj-smith47))

## [0.12.0] - 2026-06-22

### Features

* 9320c98c1156 ship per-arch macOS/Windows installers via shard routing with deterministic MSI ProductCode #minor ([@tj-smith47](https://github.com/tj-smith47))
* 66719950e3a3 cargo content-vs-version poison guard prevents re-publishing divergent crates ([@tj-smith47](https://github.com/tj-smith47))

---
### Bug Fixes

* 54787807d24a provision nfpm in-repo for the linux determinism test, fail-loud not skip ([@tj-smith47](https://github.com/tj-smith47))
* e8e9a4017ae1 gate installer stages by PATH-existence, not --version exit ([@tj-smith47](https://github.com/tj-smith47))
* eb0ff7615fdf wrap webhook body in a valid JSON envelope ([@tj-smith47](https://github.com/tj-smith47))
* 45d82f87d5ed cargo poison guard fails closed when content identity is unverifiable ([@tj-smith47](https://github.com/tj-smith47))
* f9d94ac8d9be never read a macOS .app bundle directory as a file ([@tj-smith47](https://github.com/tj-smith47))

---
### Others

* 1b42c5e50159 Revert "chore(release): bump workspace → 0.12.0" ([@tj-smith47](https://github.com/tj-smith47))
* 9ebc6f14838e Revert "chore(release): bump workspace → 0.12.0" ([@tj-smith47](https://github.com/tj-smith47))
* 98e3a5eaf500 Revert "chore(release): bump workspace → 0.12.0" ([@tj-smith47](https://github.com/tj-smith47))
* f5899b853b0d Revert "chore(release): bump workspace → 0.12.0" ([@tj-smith47](https://github.com/tj-smith47))

## [0.11.3] - 2026-06-19

### Bug Fixes

* 21462094b585 scope schema validation to the selected publisher surface #none ([@tj-smith47](https://github.com/tj-smith47))
* 542717fa7a81 gate signing-key preflight on resolved publisher surface #none ([@tj-smith47](https://github.com/tj-smith47))

## [0.11.2] - 2026-06-18

### Bug Fixes

* 9fcf93cfabc6 scope --publishers to its true surface; close custom-publisher allowlist escape ([@tj-smith47](https://github.com/tj-smith47))
* d79b4efbfb68 tighten release-pipeline default output to header+RESULT ([@tj-smith47](https://github.com/tj-smith47))

## [0.11.1] - 2026-06-18

### Bug Fixes

* 7415e789d825 route per-crate no-op skips through skip_line so default output is clean ([@tj-smith47](https://github.com/tj-smith47))
* f0f0a718678c honor --publishers/--skip publisher selection in env requirement collection ([@tj-smith47](https://github.com/tj-smith47))
* 38e54b15bb0c restore skip-output tests under --show-skipped; widen sign artifact-filter ([@tj-smith47](https://github.com/tj-smith47))

## [0.11.0] - 2026-06-17

### Features

* a77311e95c11 --publishers allowlist + GR-parity skip vocab (homebrew/chocolatey canonical) + deselect observability ([@tj-smith47](https://github.com/tj-smith47))
* aa051889ed18 core foundation for per-publisher selection ([@tj-smith47](https://github.com/tj-smith47))
* f83e607adb2e uniform per-publisher --skip/--publishers filter at dispatch ([@tj-smith47](https://github.com/tj-smith47))

---
### Bug Fixes

* 61f1e9b3df55 continue honors and validates --publishers / --skip selectors ([@tj-smith47](https://github.com/tj-smith47))
* 07a55d666c8a tighten check-config --publishers to configured set + unify help + strengthen accept tests ([@tj-smith47](https://github.com/tj-smith47))
* af5972956f69 disable rollback in the hermetic harness child ([@tj-smith47](https://github.com/tj-smith47))
* daf08c6c1f94 export cosign key as a path, not only contents ([@tj-smith47](https://github.com/tj-smith47))
* 489861307239 skip keyless cosign in the harness (no ambient OIDC) ([@tj-smith47](https://github.com/tj-smith47))
* de308e1f6137 demote subprocess command echoes to verbose across all stages ([@tj-smith47](https://github.com/tj-smith47))
* d36692cbd8be demote subprocess-command echo to verbose; keep concise default results ([@tj-smith47](https://github.com/tj-smith47))
* a48412794798 skip a zero-match config under restricted builds ([@tj-smith47](https://github.com/tj-smith47))
* 6b9469dd0793 seal env in two auth/provenance tests to kill ambient-env flake ([@tj-smith47](https://github.com/tj-smith47))
* 47dc025d6524 address T2 review — poller-deselect test, real non-invocation proof, simplify winget guard, log sibling skips ([@tj-smith47](https://github.com/tj-smith47))
* 4419b14d6661 gate out-of-dispatch publish stages on --publishers allowlist ([@tj-smith47](https://github.com/tj-smith47))
* 4e107f773580 govern announce stage by the publisher allowlist ([@tj-smith47](https://github.com/tj-smith47))
* bdeee1a91b84 fold deselect gate into AnnounceDecision ([@tj-smith47](https://github.com/tj-smith47))
* fe9af0bf9dbc DRY publisher-selection validation + doc should_skip ([@tj-smith47](https://github.com/tj-smith47))
* e5234a05b412 use sealed_env() for hermetic-env publish tests ([@tj-smith47](https://github.com/tj-smith47))

---
### Others

* 818e2582c30b normalize dry-run indentation to one nesting level ([@tj-smith47](https://github.com/tj-smith47))

## [0.10.0] - 2026-06-15

### Features

* e590ff1923b7 complete published-crate metadata for crates.io parity ([@tj-smith47](https://github.com/tj-smith47))
* 700abc99eb1e warn when an announce template references a secret-named env var ([@tj-smith47](https://github.com/tj-smith47))
* c34dce89f356 add keep_versions retention pruning ([@tj-smith47](https://github.com/tj-smith47))
* 3b570db91705 auto-inject OCI image labels (deterministic created) + derive nfpm vendor ([@tj-smith47](https://github.com/tj-smith47))
* 552b6bb121c6 build app_bundle/dmg/pkg/msi/nsis on Linux CI (unsigned); base-image tooling ([@tj-smith47](https://github.com/tj-smith47))
* aa38db2368c6 genuine before_publish gate, uploads, custom publisher; fix if_condition rustdoc ([@tj-smith47](https://github.com/tj-smith47))
* 24630a7abc54 cask livecheck support; dogfood via cask, drop dual-publish formula ([@tj-smith47](https://github.com/tj-smith47))
* 09fbb5d898dc add --raw to send messages without Tera rendering (gate the provider-side render too) ([@tj-smith47](https://github.com/tj-smith47))
* 9bea42ce2d66 redact secrets in outbound body by default; --allow-secrets opt-out ([@tj-smith47](https://github.com/tj-smith47))
* 23ac370305a3 per-package auth mode (auto/token/oidc) with OIDC fallback ([@tj-smith47](https://github.com/tj-smith47))
* 491930a2dbd8 tokenless Trusted Publishing via GitHub OIDC; enable npms dogfood ([@tj-smith47](https://github.com/tj-smith47))
* 0298ae3e4e1a refuse to publish snapshot/dev/0.0.0 versions ([@tj-smith47](https://github.com/tj-smith47))
* 2b55ecf3d976 wire generic HTTP upload stage (config.uploads was dead config) ([@tj-smith47](https://github.com/tj-smith47))

---
### Bug Fixes

* 3abd9c0121e1 map arch correctly (no x86_64 relabel); install LICENSE/man/completions; license array; derive source arch ([@tj-smith47](https://github.com/tj-smith47))
* c6cbd5e7cf95 per-crate metadata in source PKGBUILD; warn-skip unrepresentable scoop arch; document nfpm vendor ([@tj-smith47](https://github.com/tj-smith47))
* 1e68db37ae06 emit metadata on the publish-only path so cargo binstall fetches prebuilt ([@tj-smith47](https://github.com/tj-smith47))
* 3bb1ef270621 scope command-line-utilities to the cli; per-crate keywords for platform stages ([@tj-smith47](https://github.com/tj-smith47))
* 28d72e01548c mailmap-driven login back-fill across author aliases ([@tj-smith47](https://github.com/tj-smith47))
* efb370f60166 license expression + real LICENSE url; route install by artifact type (msi/nsis/zip); add projectSourceUrl/bugTrackerUrl ([@tj-smith47](https://github.com/tj-smith47))
* a421129999be select installer artifact by format so use:msi/nsis can't cross-wire ([@tj-smith47](https://github.com/tj-smith47))
* ec4645f85622 unblock v0.10.0 — coverage floor 92.5, macOS pkg test gate, doc anchors ([@tj-smith47](https://github.com/tj-smith47))
* 8e9beb04d68f hard-fail empty maintainer for deb/apk; emit Artifactory deb matrix params + Cloudsmith distribution so debs index ([@tj-smith47](https://github.com/tj-smith47))
* ec6c81f31efe reject empty/unknown deb matrix slugs; require maintainer for ipk ([@tj-smith47](https://github.com/tj-smith47))
* 9a1480d499a6 validate deb matrix-param slugs; require maintainer only when a deb/apk is actually built ([@tj-smith47](https://github.com/tj-smith47))
* 1ff12c19d8fa platform-aware msi/pkg tool gates; add srpm to ubuntu shard ([@tj-smith47](https://github.com/tj-smith47))
* aee518b2f885 restore *.pkg allowlist tuple; honest reproducibility comment ([@tj-smith47](https://github.com/tj-smith47))
* 8d4ba7f6dc5b wire Metadata.Documentation template var; explicit per-crate no-leakage assertion ([@tj-smith47](https://github.com/tj-smith47))
* c44627d79968 un-skip dogfood bundle; dedup collapsing arches; absolutize bundle path ([@tj-smith47](https://github.com/tj-smith47))
* 0fee01001ca4 emit every os/arch in cask, count casks, retire stale formula ([@tj-smith47](https://github.com/tj-smith47))
* 9b9920a113e9 install completions/manpages + livecheck + test block in formula; render dual-license via any_of ([@tj-smith47](https://github.com/tj-smith47))
* 9c837c669655 unleak completions doc link; warn on ignored livecheck opt-in ([@tj-smith47](https://github.com/tj-smith47))
* a6e4b6e19beb emit per-platform files: (binary + LICENSE/README) so nothing is dropped; validate shortDescription length ([@tj-smith47](https://github.com/tj-smith47))
* fb6e6c1d74de exclude CHANGELOG.md and dedup LICENSE.md from files: list ([@tj-smith47](https://github.com/tj-smith47))
* 1ed53cf49a57 collapse blob/cloudsmith per-file upload firehose to a summary ([@tj-smith47](https://github.com/tj-smith47))
* 44a3b812b928 finish skip-wording and arrow-glyph uniformity sweep ([@tj-smith47](https://github.com/tj-smith47))
* 21d6622c360b polish six user-facing log-quality issues ([@tj-smith47](https://github.com/tj-smith47))
* 67cd8d72af34 uniform stage-header indent; coherent retry warn + closing line ([@tj-smith47](https://github.com/tj-smith47))
* 62cd6f484415 drop dead strict-license resolver; validate maintainer handles; correct license doc ([@tj-smith47](https://github.com/tj-smith47))
* 7d1aa2f33492 emit each meta.platforms entry once; dedup archives by nix system ([@tj-smith47](https://github.com/tj-smith47))
* bbea7e1a6246 emit meta.maintainers/changelog/longDescription, license list with lib.licenses mapping, install completions+man ([@tj-smith47](https://github.com/tj-smith47))
* d2c2fd4f666e repair postinstall ReferenceError; derive description/homepage/license/author from Cargo.toml; add files/engines/provenance ([@tj-smith47](https://github.com/tj-smith47))
* 3ae18fbae378 byte-reproducible xar TOC; appbundle copy; shared symlink-safe dir-copy; installer PATH ([@tj-smith47](https://github.com/tj-smith47))
* 0019f0063906 blob workspace targets, announcer doc, guard test/doc ([@tj-smith47](https://github.com/tj-smith47))
* 2784bc5b0c79 enforce non-release guard in blob + announce stages ([@tj-smith47](https://github.com/tj-smith47))
* d193c6c9ac15 harden residual-delimiter guard; render cask service/app; gate snapcraft pre-publish ([@tj-smith47](https://github.com/tj-smith47))
* 39eac0b5a9bd render user-templated config fields in all manifest generators ([@tj-smith47](https://github.com/tj-smith47))
* 640ea67fa873 stop blocking -dev pre-releases; pin guard wiring ([@tj-smith47](https://github.com/tj-smith47))
* fc797e138bf6 match docs lint sample to real output; dedupe gh-stderr redaction onto with_env ([@tj-smith47](https://github.com/tj-smith47))
* 4ff4ac08952e key catalog identity on fileMatch, not name ([@tj-smith47](https://github.com/tj-smith47))
* 1e77fd8b938a label NoOp refresh as Update; key versions carry-forward on fileMatch ([@tj-smith47](https://github.com/tj-smith47))
* 579acb917c0d emit extract_dir, checkver, and autoupdate for bucket-ready manifests ([@tj-smith47](https://github.com/tj-smith47))
* ee76499f2f10 harden sidecar-suffix derivation; truthful checksum-algorithm doc; drop dead legacy path ([@tj-smith47](https://github.com/tj-smith47))
* eeabec7b7fa6 derive license like every other publisher; tidy license follow-ups ([@tj-smith47](https://github.com/tj-smith47))
* 4893e15fee0b resolve extra_files specs (GR parity); correct error label; doc/comment cleanups ([@tj-smith47](https://github.com/tj-smith47))
* efe38e3e2df1 correct Moniker, default UpgradeBehavior, add Documentations + InstallerSwitches ([@tj-smith47](https://github.com/tj-smith47))
* 0922ad71be13 address review findings ([@tj-smith47](https://github.com/tj-smith47))
* 581d52f8622a use house SPDX parser for licenseUrl suppression ([@tj-smith47](https://github.com/tj-smith47))
* 5382501fa528 rename default push-token env var FURY_TOKEN to FURY_PUSH_TOKEN ([@tj-smith47](https://github.com/tj-smith47))
* 1c9c7095047d use shared http::blocking_client for existence probe; test encode_package_path ([@tj-smith47](https://github.com/tj-smith47))
* aa9746571ab5 share rollback-target collection between artifactory + uploads ([@tj-smith47](https://github.com/tj-smith47))

---
### Others

* b7381c77ecbf dual-license MIT OR Apache-2.0; single-source derivable metadata ([@tj-smith47](https://github.com/tj-smith47))
* fa39bc6fd78d document outbound secret redaction, --allow-secrets, and the check-config lint ([@tj-smith47](https://github.com/tj-smith47))

## [0.9.1] - 2026-06-13

### Features

* ce8ce6034072 consolidated run helper with verbose live-stream + emit-on-failure ([@tj-smith47](https://github.com/tj-smith47))
* 5a32ab7bf996 route per-crate no-config skips to debug; add --show-skipped ([@tj-smith47](https://github.com/tj-smith47))
* 75da7615ca30 proactive GitHub upload pace + secondary-RL exhaustion proof ([@tj-smith47](https://github.com/tj-smith47))

---
### Bug Fixes

* 784d18178ddb ship a runnable musl binary in the apk package ([@tj-smith47](https://github.com/tj-smith47))
* 52869be8b97d kill recursive sidecar chains via primary-subject taxonomy ([@tj-smith47](https://github.com/tj-smith47))
* bebbde927855 bind every build-consuming Linux surface to the gnu build ([@tj-smith47](https://github.com/tj-smith47))
* 1be521c3eb4f route live tee to stderr, concurrent stdin, dedup stream methods ([@tj-smith47](https://github.com/tj-smith47))
* 5d3a37425a54 sign by digest, never by a movable tag ([@tj-smith47](https://github.com/tj-smith47))
* 82639a0428da suppress false nothing-pushed warning on cask-only configs ([@tj-smith47](https://github.com/tj-smith47))
* 06e33eb62c64 make derivation formatting mandatory and fail loud, no unformatted push ([@tj-smith47](https://github.com/tj-smith47))
* 1a590e37ce4d parity — artifacts:all signs the combined checksums file ([@tj-smith47](https://github.com/tj-smith47))
* 6f77bf4ae887 restore GR parity — sign every Checksum kind, not combined-only ([@tj-smith47](https://github.com/tj-smith47))
* 28d100d1c270 surface verify-release findings in the end-of-release Summary ([@tj-smith47](https://github.com/tj-smith47))
* cda1eb7bd14d label container-start failures and anchor the smoke marker ([@tj-smith47](https://github.com/tj-smith47))
* 52e55ee29d64 make install smoke-test failures diagnosable ([@tj-smith47](https://github.com/tj-smith47))

---
### Others

* cd87602f30ed document recursion detector's name-suffix assumption ([@tj-smith47](https://github.com/tj-smith47))
* 9786400ff5a3 drop vestigial VerifyReleaseSummary.ran field ([@tj-smith47](https://github.com/tj-smith47))
* a08507ceafa3 pin Debug-verbosity cell of no-config skip matrix ([@tj-smith47](https://github.com/tj-smith47))
* 003776f7eb45 make upload_pace_zero_is_a_no_op deterministic (relative pacing compare) ([@tj-smith47](https://github.com/tj-smith47))
* 2fc21f14abe4 cover docker-sign digest-pin edge cases and the missing-digest path ([@tj-smith47](https://github.com/tj-smith47))
* f3761b63cdbf fix Windows fake-cosign arg capture in docker-sign digest test ([@tj-smith47](https://github.com/tj-smith47))

## [0.9.0] - 2026-06-11

### Bug Fixes

* 9ae4ed392be8 inject Is* template vars and NightlyBuild as typed bools/number (TJ Smith)
* decfe86b6618 review fixes — unset eviction, NightlyBuild truthiness note, test typing fidelity (TJ Smith)
* a6be5873b787 always write run summary; gate rollback on publish state (TJ Smith)
* ad49ce3fb5c7 review fixes — summary clobber guard, probe fail-closed, kms PATH seam (TJ Smith)
* 859e2f5860cf reject multi-document typed configs in builtin mode too (TJ Smith)
* bc2e553ead8c stop replacing PATH wholesale in spawn-failure tests (TJ Smith)
* fb7e5a166ad1 fail on missing expected signature/SBOM assets (TJ Smith)
* 9157a4919ff1 pin install-smoke containers to the package arch; drop apk self-provides (TJ Smith)
* 7634ac80354a re-review fixes — transitive ids verdict for derived subjects, typed multi-doc pin, docker_signs warning (TJ Smith)
* f79b7ebc5089 review fixes — resolved-name filter keying, probe pinning, probe diagnostics, lock recovery (TJ Smith)
* 727284f7957e review fixes — sbom derivation equivalence, release.ids sig inheritance (TJ Smith)

---
### Others

* cf02d17fcae2 rollback v0.8.0 [skip ci] (anodize-rollback)
* b73a16855234 "chore(release): rollback v0.8.0 [skip ci]" (TJ Smith)

## [0.8.0] - 2026-06-11

### Features

* 6a628185b1ba add --allow-rerun flag to anodizer publish (TJ Smith)
* cece142186c7 config-declarable on_error/on_rollback hooks (TJ Smith)
* 9e27a01471c9 expose failure-hook context as ANODIZER_* env vars (TJ Smith)
* 3eca978cea09 gate all irreversible publishers on any required failure (TJ Smith)
* 8f17ca07b47d preflight guard for publish-set dependency completeness (TJ Smith)
* e1c4c083e2de retain_on_rollback, on_error hooks, anodizer notify (TJ Smith)
* bdef8a2f957c universal publisher idempotency for safe re-runs (TJ Smith)
* a6665ddb2daf explicit --version override for autotag (TJ Smith)
* ca8155a6fef5 auto-detect dind for install_smoke; wire blobs to MinIO (TJ Smith)

---
### Bug Fixes

* c4031cd1a757 set ArtifactName from archive filename in url_template render (TJ Smith)
* 6e2b2387e422 unique SSH key filename per clone to prevent EEXIST on retry (TJ Smith)
* 39523204f1fb gate zigbuild routing on a reachable zig toolchain (TJ Smith)
* 173d8b4318b4 route host linux-gnu builds through zigbuild for a hermetic glibc floor (TJ Smith)
* ecc74f794f74 make republish_in_moderation actually re-push (TJ Smith)
* 3beec78e7866 add on_error to PublishDefaults with append-merge semantics; wire retain_on_rollback on cargo, schemastore, mcp (TJ Smith)
* 83da4f3add02 correct submitter required-gating warning text (TJ Smith)
* c0e19c2df6da durability fixes W1-W3, F1-F2, S1-S2, F3, GHA#1-2, #58-59 (TJ Smith)
* 532096abd458 error loudly when artifact URL absent in publish mode; tolerate in snapshot (TJ Smith)
* a29bb53a6a91 guarantee trailing newline on written SSH key (TJ Smith)
* cef07f0a4861 key the workspace-root dep cache by resolved root path (TJ Smith)
* 6e1980a40a5e propagate render errors in AUR rollback creds + add render tests (TJ Smith)
* 7e63afe5b5c5 redact custom header values and target URL in artifactory dry-run log (TJ Smith)
* 2a22c30de1f9 rehydrate sha256 via ChecksumStage in publish/continue pipeline (TJ Smith)
* 4de628a767e3 render npm registry/tag/metadata and dockerhub username templates (TJ Smith)
* 84f84dd40efd render secret/url/branch/token config templates before use (TJ Smith)
* e236fb1782ac resolve package renames in publish-set preflight (TJ Smith)
* 17bc6953a4dc resolve renames in the dependency wait gate (TJ Smith)
* 343d4f6c2a2b require all live publishers and restore install_smoke (TJ Smith)

---
### Others

* 4bd2c68379cd correct on_error timing and RolledBack semantics (TJ Smith)
* 099797b072ae DepEntry struct, alias in guard errors, shared root cache (TJ Smith)
* e1f5b88d14bd single-source failure vars; pin env-channel exhaustiveness (TJ Smith)
* 5c24cb2dfdc7 normalize hook output path for Windows in on_error test (TJ Smith)
* b9ad18ff3708 verify retain_on_rollback skips rollback dispatch (TJ Smith)
* 14d788e6014c test+fix: address v0.8.0 review findings (TJ Smith)

## [0.6.0] - 2026-06-08

Changes since `v0.5.0`. Will be cut as the next release.

### Features

- **`anodize tag rollback`** — new subcommand that deletes anodize-managed
  tags at a SHA and reverts (or resets past) the bump commit they point at.
  Failure-recovery counterpart to `anodize tag`. Flags: `--dry-run`,
  `--no-push`, `--scope={all,lockstep,per-crate}`,
  `--mode={revert,reset}`, `--branch <name>`. SHA-derived branch
  resolution is race-immune to default-branch movement. Safety check
  hard-fails when non-bump commits sit between HEAD and the target SHA
  under `--mode=revert`. (`3a27f92`, `5948253`, `ba81b6e`, `41947cb`)
- **`docker_v2:` graduates to canonical Docker API** with full GoReleaser
  v2.16 surface — Platforms metadata, pre/post hook contract
  (`{{ .Images }}` / `{{ .Dockerfile }}` / `{{ .ContextDir }}` /
  `{{ .Digest }}` / `{{ .BaseImage[Digest] }}`), podman backend (Linux-only),
  cleaner `images` + `tags` separation. Legacy `dockers:` block is now
  rejected at config-load time with a migration error. (`166e3a7`,
  `9e6f452`, `dbc87b7`)
- **Anodizer publishes itself as an MCP server.** The repo's own
  `.anodizer.yaml` declares `mcp:` + per-crate `docker_v2:`; the
  distroless OCI image at `ghcr.io/tj-smith47/anodizer:<version>` carries
  `ENTRYPOINT ["/usr/local/bin/anodizer"]` + `CMD ["mcp"]` so MCP clients
  `docker run` it as a stdio server. (`596e1a3`, `41947cb`)
- **Per-crate workspace-aware tag** — `anodizer tag` dispatches per-crate
  in workspaces with per-crate `[package].version`, emits `crates` (JSON
  array) and `versions` (JSON object) step outputs, propagates bumps to
  intra-workspace `path + version` dep specs. (`7735448`, `475109e`,
  `ba82aa1`, `0135f56`)
- **Per-crate dist subdir layout for workspace release** —
  `release --publish-only` consumes `preserved-dist/<crate>/` subdirs
  emitted by per-crate determinism shards. (`9c13daf`, `76cb613`,
  `9562bc3`)
- **`publishers[].required:` field** — every publisher accepts a
  `required:` boolean that wires through `resolved_required()` so the
  release pipeline knows whether a publisher's failure should block the
  Submitter gate / non-zero exit. Submitter-group publishers (cargo,
  chocolatey, winget, snapcraft, upstream-AUR) warn loudly when set to
  `true` since their failure cannot be recovered. (`a90f8ac`, `948dd4a`,
  `7de69a4`, `d035aaf`)
- **`if:` template-conditional gates** across publishers, hooks,
  announcers, archives, blob entries — when the rendered result is falsy
  (`"false"` / `"0"` / `"no"` / empty), the entry is skipped. Render
  failure hard-errors. (`10af9cf`)
- **AI release-note enhancement** — `changelog.use: ai` wires
  anthropic / openai / ollama as backends; produces a polished release
  note from the raw commit log. (`c8342c5`)
- **GoReleaser v2.16 parity** — nightly `tag_name:` templates, srpm
  Format/Ext overrides, immutable releases policy, `homebrew_casks:` as
  the canonical Homebrew surface (deprecated `publish.homebrew`),
  v2.12.6→v2.15.3 deprecation aliases for renamed fields. (`f9ec8d5`,
  `63bc5fc`, `1868af6`, `d0aff91`)
- **Pre-publishing hooks** (`before_publish:`) and per-artifact iteration
  with `ids` / `artifacts` filters. (`2e55c3f`, `a94ab91`)
- **Recursive config includes**, strict `template_vars:`, `meta_`
  propagation. (`42eb1ff`)
- **npm + gemfury publishers** — full implementation with idempotency
  probe, retry, templated extra files, rollback (npm) and `furies:`
  alias (gemfury). (`e3d7264`, `94e139d`, `2335dae`)
- **Single-target build, split/merge, nightly builds** audit closures —
  scheduled nightly workflow, version_template, keep_single_release with
  safety + dry-run visibility. (`aa11201`, `bc35263`, `b314c59`,
  `35e8d31`)
- **Per-publisher Pre-image SHA tracking** for `KrewExtra::bot_template_pre_image_shas`
  — rollback drift-detection for krew bot-template mode (Unchanged /
  Drifted / Missing / Unreadable). (`5948253`)
- **`actions: read` permission** required when the release job downloads
  artifacts from a sibling workflow (`from-artifact: anodizer-linux`,
  cross-workflow patterns). (workflow hardening)

### Fixes

- **Unblock cfgd release** — `--publish-only` resume_release auto-enable,
  per-iteration skip_stages propagation from `workspaces[].skip`,
  per-crate manifest path re-anchor, OCI `version` field omitted from the
  wire, BotTemplate pre-image SHA recording, cargo intra-workspace dep
  pin propagation. (`596e1a3`, `58b4e7a`, `76e766f`, `aec8eef`,
  `7f26c9f`, `6ca21a9`)
- **Source archive: extra-files mode normalized to 0o644 under SDE** for
  cross-OS determinism. (`c224627`)
- **Tag bump commits omit `[skip ci]` on the primary commit** so the
  tag-push trigger fires downstream `release.yml`. Side-effect
  `version_sync` propagation commits still carry the marker. (`a4d55d5`)
- **`wait_for_workspace_deps` gate** prevents cross-crate publish race
  during topo-ordered workspace publish. (`f756834`)
- **Detached-HEAD push** — `git push HEAD via refs/heads/<branch>`
  refspec, resolve detached HEAD before push. (`292af2d`, `68de654`)
- **Per-crate bump idempotent** when manifests are already at the target
  version. (`0135f56`)
- **`.anodizer.yaml workspaces:` takes precedence** over
  `[workspace.package].version` — authoritative signal for
  per-crate-with-grouping intent. (`e6a9ee9`)
- **`check`: fall back to `GITHUB_REF_NAME`** for tag_override when
  triggered by tag push. (`4b8d5c8`)
- **Audit follow-ups** drained across B1–B24 — pkg, msi, nsis, dmg,
  appbundle, changelog, build, release, git, docker, publish modules.

### CI / Workflows

- **Switched to cargo-nextest + sccache** layered atop rust-cache for
  faster CI. (`7d5573e`)
- **Scheduled nightly workflow** with date-based versioning. (`35e8d31`)
- **Sharded determinism matrix** (Linux + macOS + Windows-x86_64 +
  Windows-aarch64) — each shard validates only its own targets;
  cross-shard hash comparison is intentionally relaxed.
- **`Rollback on release failure` step** — workflow integrates
  `anodizer tag rollback "$GITHUB_SHA"` as the
  `if: (failure() || cancelled())` recovery hook.

### Docs

- New [Release Workflow Strategies](docs/site/content/docs/ci/release-workflows.md)
  page covering single-crate / lockstep / per-crate / hybrid / split-CI
  shapes with the decision tree. (`ef17e7d`)
- New `## crates[].docker_v2`, `## crates[].publish.krew`, `## mcp`
  schema sections in the auto-generated configuration reference.
- `tag rollback` documented in README + release-resilience guide +
  auto-tagging guide.
- `_preserved-bin/` layout documented in the determinism guide.
- `docker_v2:` page rewritten end-to-end; legacy `dockers:` references
  removed across packages, retry, dogfooding, and CI docs.
- MCP registry page: "Wiring the OCI image" subsection added.
- Krew page: "Rollback semantics for bot-template mode" + graceful
  degradation note for `project_root` auto-detect.
- anodizer-action page: 7 previously-undocumented inputs added
  (`apk-private-key`, `preserve-dist`, `shard-label`, `determinism`,
  `determinism-runs`, `determinism-stages`, `determinism-targets`);
  retry behavior callout updated to flag stateful
  `--publish-only` / `--rollback-only` / `tag rollback`.

[Unreleased]: https://github.com/tj-smith47/anodizer/compare/v0.20.0...HEAD
[0.20.0]: https://github.com/tj-smith47/anodizer/compare/v0.19.0...v0.20.0
[0.19.0]: https://github.com/tj-smith47/anodizer/compare/v0.18.0...v0.19.0
[0.18.0]: https://github.com/tj-smith47/anodizer/compare/v0.17.0...v0.18.0
[0.17.0]: https://github.com/tj-smith47/anodizer/compare/v0.16.1...v0.17.0
[0.16.1]: https://github.com/tj-smith47/anodizer/compare/v0.16.0...v0.16.1
[0.16.0]: https://github.com/tj-smith47/anodizer/compare/v0.15.5...v0.16.0
[0.15.5]: https://github.com/tj-smith47/anodizer/compare/v0.15.4...v0.15.5
[0.15.4]: https://github.com/tj-smith47/anodizer/compare/v0.15.3...v0.15.4
[0.15.3]: https://github.com/tj-smith47/anodizer/compare/v0.15.1...v0.15.3
[0.15.1]: https://github.com/tj-smith47/anodizer/compare/v0.15.0...v0.15.1
[0.15.0]: https://github.com/tj-smith47/anodizer/compare/v0.14.0...v0.15.0
[0.14.0]: https://github.com/tj-smith47/anodizer/compare/v0.13.1...v0.14.0
[0.13.1]: https://github.com/tj-smith47/anodizer/compare/v0.13.0...v0.13.1
[0.13.0]: https://github.com/tj-smith47/anodizer/compare/v0.12.3...v0.13.0
[0.12.3]: https://github.com/tj-smith47/anodizer/compare/v0.12.2...v0.12.3
[0.12.2]: https://github.com/tj-smith47/anodizer/compare/v0.12.1...v0.12.2
[0.12.1]: https://github.com/tj-smith47/anodizer/compare/v0.12.0...v0.12.1
[0.12.0]: https://github.com/tj-smith47/anodizer/compare/v0.11.3...v0.12.0
[0.11.3]: https://github.com/tj-smith47/anodizer/compare/v0.11.2...v0.11.3
[0.11.2]: https://github.com/tj-smith47/anodizer/compare/v0.11.1...v0.11.2
[0.11.1]: https://github.com/tj-smith47/anodizer/compare/v0.11.0...v0.11.1
[0.11.0]: https://github.com/tj-smith47/anodizer/compare/v0.10.0...v0.11.0
[0.10.0]: https://github.com/tj-smith47/anodizer/compare/v0.9.1...v0.10.0
[0.9.1]: https://github.com/tj-smith47/anodizer/compare/v0.9.0...v0.9.1
[0.9.0]: https://github.com/tj-smith47/anodizer/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/tj-smith47/anodizer/compare/v0.6.0...v0.8.0
[0.6.0]: https://github.com/tj-smith47/anodizer/compare/v0.5.0...v0.6.0
