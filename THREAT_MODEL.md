# Threat Model

This document identifies the security-relevant assets, trust boundaries, and
threats that follow from what anodizer does, and maps each threat to the
control that mitigates it (or labels it an accepted risk where no control
exists). It complements [`SECURITY.md`](./SECURITY.md), which owns the
vulnerability-reporting process, and [`INCIDENT_RESPONSE.md`](./INCIDENT_RESPONSE.md),
which owns the playbook for when one of these threats is realized.

anodizer is release-automation tooling. A single `anodizer release` run can
build binaries, sign artifacts, attest provenance, and publish to crates.io,
npm, Homebrew taps, Scoop buckets, Chocolatey, Winget, AUR, Krew, Snapcraft,
Docker registries, blob storage, and custom publisher commands — then announce
to many channels. The threat surface is the union of every credential it
touches and every subprocess it spawns.

## Asset Inventory

### Critical assets

- **Signing key material** — cosign private keys and GPG secret keys used by
  `stage-sign`, `stage-attest`, and `stage-notarize` to sign binaries,
  archives, checksums, container images, and SBOMs. Compromise lets an attacker
  forge a signature that verifiers will trust.
- **Registry & forge tokens** — `CARGO_REGISTRY_TOKEN`, npm auth tokens, the
  GitHub token tier (`repository.token` → `ANODIZER_GITHUB_TOKEN` →
  `GITHUB_TOKEN`), `MCP_GITHUB_TOKEN`, AUR SSH deploy keys, container-registry
  credentials, blob-storage credentials, and announcer webhook secrets.
- **OIDC identity** — the GitHub Actions OIDC token (`id-token: write`) consumed
  by npm Trusted Publishing and by `actions/attest-build-provenance` via
  `stage-attest`'s `subjects` mode. A forged or misdirected OIDC exchange can
  mint short-lived publish credentials or sign provenance for a release the
  attacker controls.
- **Release artifacts** — the binaries, archives, packages, container images,
  and manifests anodizer produces and publishes. These are what downstream
  users install; tampering with them is the highest-impact outcome.
- **The release configuration** — `.anodizer.yaml` and the Tera templates it
  references. The config is committed and drives every subprocess, every
  publish destination, and every announce message.

### Asset locations

- Local developer machines (`anodizer release --dry-run` / `--snapshot`).
- CI runners (the production release path; GitHub-hosted or self-hosted).
- Public package registries and forge release pages.
- Source control (config + templates live in the repo).

## Actors

- **Maintainers** — trusted operators who author config and trigger releases.
- **Contributors** — submit PRs that may alter config, templates, or hooks.
- **External attackers** — seek to exfiltrate credentials, tamper with
  artifacts, or impersonate a publisher.
- **A compromised CI runner** — a runner (especially self-hosted) whose
  environment an attacker can observe or influence.

## Trust Boundaries

| Boundary | Untrusted side | Why it matters |
|---|---|---|
| **Config & templates** | `.anodizer.yaml` + Tera templates | Templates read `{{ .Env.FOO }}` and emit text into published metadata; a malicious or attacker-influenced template can leak env or inject content. |
| **User hooks & `publisher.cmd`** | `before:` / `after:` hooks, custom publisher commands, `template_files` | Arbitrary code the operator opted into, run with the invoking user's privileges. |
| **Subprocess spawns** | every `Command::new` call site | Each shell-out can write disk, hit the network, and exfiltrate secrets via argv/env. |
| **Network publishers** | crates.io / npm / forge / registry endpoints | A misconfigured destination can publish to a target the operator does not own. |
| **CI runner environment** | the host running the release | Holds every live credential in env for the duration of the run. |

## Threats & Mitigations

Each threat below is mapped to the concrete control that addresses it, with the
implementing module cited. Where no control exists, the threat is labeled
**Accepted risk**.

### T1 — Key / token exfiltration via argv, env, or process list

A user-supplied `publisher.cmd`, a malicious hook, or a compromised tool on the
runner reads release credentials out of the inherited environment, or a token
embedded in a remote URL leaks through a subprocess error message.

**Mitigations:**

- **Env whitelisting for user commands.** `crates/core/src/user_command.rs`
  constructs every user-supplied command (`publisher.cmd`) with `env_clear()`
  followed by re-population from a fixed `ENV_WHITELIST` (`HOME`, `USER`,
  `USERPROFILE`, `TMPDIR`, `TMP`, `TEMP`, `PATH`, `SYSTEMROOT`). Credential-shaped
  variables (`GITHUB_TOKEN`, `COSIGN_*`, `CARGO_REGISTRY_TOKEN`, …) never reach
  the child process. A unit test proves a credential-shaped key is dropped while
  `PATH` forwards verbatim.
- **The `Command::new` allow-list.** `.claude/rules/module-boundaries.md`
  confines all subprocess spawns to a small, named set of modules so the
  exfiltration surface is auditable. Anything outside the allow-list is a
  review-blocker. The current audited count is 71 files / 228 call sites, all
  inside the allow-list.
- **Token redaction in error output.** `crates/stage-publish/src/util/cmd.rs`
  (`run_cmd_in_redacted` / `redact_output_token`) scrubs a secret from any
  captured argv, stdout, and stderr before it is embedded in an error message —
  so a git remote URL of the form
  `https://x-access-token:<TOKEN>@github.com/...` cannot leak through a failed
  command's diagnostics.
- **Tokens are not persisted.** anodizer reads tokens from the environment at
  runtime and does not write them to disk; the per-run npm `.npmrc` is written
  `0600` into a process-private `TempDir` and removed with it.

**Accepted risk:** anodizer cannot constrain what an opted-in `before:`/`after:`
hook does with the whitelisted `PATH` and `HOME` it legitimately needs.
Build/archive hooks run with the operator's full env by design (they need it to
build). Hooks and `publisher.cmd` are CI scripts the operator authored and must
review like any other CI step.

### T2 — Malicious config or template injection

A committed or PR-introduced Tera template reads environment variables and
emits them into release notes, changelog, or a package manifest, or renders
attacker-controlled content into published metadata.

**Mitigations:**

- **Pre-publish render guard.** `crates/stage-prepublish-guard/src/lib.rs`
  renders every publisher manifest and every announcer template in-memory —
  reading no credentials, sending nothing, writing nothing — immediately after
  the release is created and **before any irreversible publisher fires**. A
  template that fails to render aborts the release while it is still reversible,
  instead of after a one-way-door publisher has already run.
- **`anodizer check` and `--dry-run` / `--snapshot`** run the full pipeline
  without publishing, surfacing config and template defects before a tag exists.

**Accepted risk:** a template that *successfully* renders attacker-chosen text
into release notes is not distinguishable from a legitimate one by the guard.
The config and its templates are inside the trust boundary; they must be
reviewed in PR the same way CI workflows are. `SECURITY.md` documents this
explicitly.

### T3 — Supply-chain artifact tampering

An artifact is modified between build and consumption, or a malicious build
produces an artifact that masquerades as a legitimate release.

**Mitigations:**

- **Reproducible builds.** anodizer pins `SOURCE_DATE_EPOCH` and drives
  byte-stable builds; `anodizer check determinism` rebuilds inside hermetic
  per-run worktrees (`crates/core/src/determinism_runner.rs`) to detect
  non-reproducibility. Independent reproduction is a tamper-detection control.
- **Checksums + signatures.** `stage-checksum` computes sha256 over every
  selected artifact; `stage-sign` signs artifacts and checksums with cosign/GPG;
  `stage-notarize` handles macOS notarization. Verifiers can confirm an
  artifact matches a signed checksum.
- **SLSA attestations.** `crates/stage-attest` emits build provenance: the
  `subjects` mode writes `attestation-subjects.json` for
  `actions/attest-build-provenance` (OIDC), and the `emit` mode writes an
  in-toto v1 statement signed by the existing `signs:` loop. `stage-sbom`
  emits a software bill of materials. Together these let a consumer verify what
  was built, from what, and by whom.

**Accepted risk:** these controls are *detective* — they let a careful consumer
verify integrity. anodizer cannot force a downstream installer to check a
signature or an attestation. A consumer who installs without verifying is not
protected.

### T4 — Compromised or self-hosted runner

The host executing the release is observed or controlled by an attacker, who
reads the live credentials present in its environment.

**Mitigations:**

- **OIDC over long-lived tokens.** Where supported, anodizer prefers
  short-lived OIDC credentials to standing secrets — npm Trusted Publishing
  (`crates/stage-publish/src/npm/publish.rs`) writes a **token-less** `.npmrc`
  and lets npm exchange the GitHub Actions OIDC token for a short-lived publish
  credential, so no long-lived npm token sits in the runner env.
- **Runner-capability gating.** The npm provenance path is gated on runner
  capability: on a self-hosted runner that cannot satisfy provenance
  requirements, anodizer degrades with a warning rather than failing or
  emitting forged provenance.

**Accepted risk (significant).** A fully compromised runner with a long-lived
token in its environment (cargo, AUR SSH key, a classic GitHub PAT) can use
that token directly — no application-level control can prevent it, because the
runner *is* the trusted execution environment. The mitigation is operational:
scope `GITHUB_TOKEN` with `permissions:` blocks, prefer OIDC, prefer
GitHub-hosted ephemeral runners for credential-bearing publishers, and rotate
any token a self-hosted runner has handled. This is the documented residual
risk for self-hosted release runners.

### T5 — npm dependency confusion

anodizer's default npm layout publishes one thin per-platform package per
target plus a metapackage that lists them under `optionalDependencies`
(`crates/stage-publish/src/npm/optional_deps.rs`). An attacker who registers a
per-platform package name before the operator does could have npm resolve the
attacker's package instead of the legitimate one.

**Mitigations:**

- **Pre-publish existence probe + Trusted-Publishing semantics.**
  `npm/publish.rs` probes package existence and chooses auth accordingly: a
  brand-new package always uses a token (Trusted Publishing cannot create a
  non-existent package), and an existing package prefers OIDC. This makes the
  first publish of each per-platform name an authenticated, operator-driven act
  rather than an implicit one.
- **Scoped names.** The per-platform packages are emitted under the
  configured scope (`<scope>/<bin>-<os>-<cpu>[-<libc>]`), and npm scopes are
  owned by the publisher — an attacker cannot register a name inside a scope
  they do not control.

**Accepted risk:** anodizer cannot reserve a scope or a per-platform name the
operator has not yet claimed. Operators publishing **unscoped** packages should
register every per-platform name (including future targets) before first
release to foreclose the confusion window.

### T6 — Provenance forgery

An attacker produces a SLSA attestation or signature that verifiers accept for
an artifact they did not legitimately build.

**Mitigations:**

- **OIDC-bound provenance.** In `stage-attest`'s `subjects` mode, provenance is
  produced by `actions/attest-build-provenance` over the artifact digests
  anodizer computed, bound to the workflow's OIDC identity — forging it requires
  forging the OIDC identity, not just controlling a key.
- **Signatures over checksums.** `stage-sign` signs the `stage-checksum` digest
  set, so an attestation's subject digests are independently signed.
- **Hermetic harness keys are never production keys.**
  `crates/core/src/harness_signing.rs` provisions *ephemeral* cosign and GPG
  keypairs inside a per-run tempdir purely for the determinism harness, so a
  test/determinism run never touches — and cannot leak — a production signing
  key.

**Accepted risk:** cosign signatures are non-deterministic (ECDSA random-`k`),
so byte-equality of signatures is not a verification primitive; signature
*verification* downstream is the gate, and anodizer cannot compel a consumer to
perform it.

### T7 — Publishing to an unowned destination

A misconfigured `publish` block pushes to a registry, tap, bucket, or webhook
the operator does not own.

**Mitigations:**

- **`anodizer check` + `--dry-run`/`--snapshot`** surface the resolved
  destinations before any tag is cut, and the pre-publish guard renders every
  manifest before the first publisher fires.

**Accepted risk:** anodizer cannot know which destinations the operator
legitimately owns. Review `publish` blocks before tagging (per `SECURITY.md`).

## Residual Risks

- A fully compromised CI runner holding a long-lived token (T4).
- An opted-in hook or `publisher.cmd` misusing its legitimate env (T1).
- A successfully-rendering but malicious template authored in a trusted PR (T2).
- A downstream consumer that installs without verifying signatures or
  attestations (T3, T6).
- Zero-day vulnerabilities in anodizer's dependencies or in the external tools
  (cosign, gpg, cargo, npm, git, docker) it shells out to.

## See Also

- [`SECURITY.md`](./SECURITY.md) — vulnerability reporting and best practices.
- [`INCIDENT_RESPONSE.md`](./INCIDENT_RESPONSE.md) — playbook for a realized incident.
- [`.claude/rules/module-boundaries.md`](./.claude/rules/module-boundaries.md) — the subprocess allow-list.
