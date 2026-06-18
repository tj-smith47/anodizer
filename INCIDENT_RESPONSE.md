# Incident Response Plan

This document describes how anodizer responds to a security incident — a leaked
credential, a tampered release, or a vulnerability in anodizer itself. It is the
operational counterpart to [`SECURITY.md`](./SECURITY.md) (which owns how a
vulnerability is *reported*) and [`THREAT_MODEL.md`](./THREAT_MODEL.md) (which
owns *what can go wrong*). When those two documents describe a threat that has
actually occurred, this is the playbook.

---

## 1. Scope

This plan applies to everything in the
[tj-smith47/anodizer](https://github.com/tj-smith47/anodizer) repository: the
crates, the release artifacts anodizer publishes for itself, the signing keys
and registry tokens it uses, and the GitHub Actions workflows that drive its
releases. It also serves as guidance for **operators of anodizer** who suffer a
release-pipeline incident in their own project.

## 2. Roles & Contacts

- **Incident Lead:** [@tj-smith47](https://github.com/tj-smith47).
- **Reporting channel:** all incidents are reported privately and exclusively
  through
  [GitHub Security Advisories](https://github.com/tj-smith47/anodizer/security/advisories/new),
  per [`SECURITY.md`](./SECURITY.md). Do not disclose through public issues,
  pull requests, or any public channel.

## 3. Severity Classes

| Class | Definition | Example |
|---|---|---|
| **SEV-1 — Critical** | A signing key or a long-lived registry/forge token is exposed, OR a published artifact is known-tampered. A one-way door is, or may be, compromised. | Cosign private key printed to a public CI log; a malicious binary published to crates.io. |
| **SEV-2 — High** | A vulnerability in anodizer that can leak a secret or tamper an artifact, not yet exploited; or a short-lived/OIDC credential exposure with bounded blast radius. | A code path that echoes a token into an unredacted error; an OIDC misconfiguration. |
| **SEV-3 — Moderate** | A defect with security impact but no direct path to credential loss or artifact tampering. | A template that can leak a non-secret env var into release notes. |
| **SEV-4 — Low** | Hardening gap with no demonstrated exploit. | A missing `permissions:` scope in an example workflow. |

The Incident Lead assigns severity within the timeline `SECURITY.md` commits to
(48h acknowledgment, 7-day initial assessment).

## 4. Initial Response (all incidents)

1. **Acknowledge** the report and thank the reporter (within 48h).
2. **Assess** confidentiality / integrity / availability impact and assign a
   severity class.
3. **Contain.** Stop the bleeding before investigating: disable the offending
   workflow, revoke the credential, or pull a draft release as appropriate. For
   SEV-1, containment runs in parallel with — not after — investigation.
4. **Engage** any additional maintainers needed.
5. **Document** every action with timestamps from the first minute; this record
   becomes the post-incident review and any public advisory.

## 5. Playbook — Leaked Signing Key or Registry Token

This is anodizer's highest-frequency worst case: a credential the pipeline holds
is exposed (printed to a log, committed, or read off a compromised runner).

### 5a. Immediate containment

- **Revoke first, investigate second.** Treat the credential as compromised the
  moment exposure is plausible — do not wait for proof of misuse.
  - **GitHub token** (`GITHUB_TOKEN` / `ANODIZER_GITHUB_TOKEN` / a PAT): revoke
    the PAT or rotate the secret; the ephemeral `GITHUB_TOKEN` expires with the
    job but rotate any standing fallback.
  - **`CARGO_REGISTRY_TOKEN`:** revoke at crates.io (Account → API Tokens) and
    issue a new one.
  - **npm token:** revoke at npmjs.com. Prefer migrating the package to
    **Trusted Publishing (OIDC)** so no long-lived npm token exists going
    forward — anodizer's `npm/publish.rs` writes a token-less `.npmrc` and uses
    the OIDC exchange when a Trusted Publisher is configured.
  - **AUR SSH deploy key:** remove the public key from the AUR account and
    rotate; reissue via `gh secret set AUR_SSH_KEY` (preserve the trailing
    newline — a missing newline yields `error in libcrypto`).
  - **Container-registry / blob-storage / announcer-webhook credentials:**
    rotate at the provider and update the CI secret store.
- **Audit access logs** at the registry/forge for any use of the credential in
  the exposure window.

### 5b. Signing-key-specific steps

A leaked **signing** key is more severe than a token, because an attacker can
sign artifacts that verifiers will trust:

1. **Rotate the key.** Generate a new cosign/GPG key, store it in the CI secret
   store, and re-sign subsequent releases with it.
2. **Revoke the compromised key.** For cosign, publish a revocation / mark the
   key compromised so verifiers stop trusting it; for GPG, publish a revocation
   certificate to the keyservers.
3. **Re-attest.** Reissue SLSA attestations (`stage-attest`) for any release that
   must remain trusted, signed under the new key.
4. **Confirm the leaked key was not a harness key.** anodizer's determinism
   harness uses *ephemeral* keys provisioned in a per-run tempdir
   (`crates/core/src/harness_signing.rs`); these are never production keys and
   need no rotation. Verify the exposed key is a real signing key before
   triggering a full rotation.

### 5c. Verify the leak is closed

- Confirm the credential no longer appears in any log, artifact, or config.
- Confirm the new credential works via `anodizer release --dry-run` /
  `--snapshot` (full pipeline, no publish) before the next real release.

## 6. Playbook — Tampered or Compromised Published Release

A published artifact is known or suspected to be malicious or modified. The
hard constraint: **several anodizer publishers are one-way doors.**

### 6a. One-way-door classification

| Publisher | Reversible? | Action available |
|---|---|---|
| **crates.io (cargo)** | No (cannot delete) | `cargo yank` the version |
| **npm** | No (unpublish disallowed after the window) | deprecate the version |
| **Chocolatey** | No (moderation, cannot delete) | unlist / publish a fixed version |
| **Winget** | No (merged into the index) | submit removal/fix PR |
| **Snapcraft** | No | release a fixed revision |
| **GitHub Release / blob storage** | Yes | delete the asset/release |
| **Homebrew tap / Scoop bucket / AUR / Krew** | Yes (you own the repo) | revert the manifest |

anodizer encodes this asymmetry in its pipeline: the pre-publish guard
(`crates/stage-prepublish-guard`) and pre-flight checks
(`crates/stage-publish/src/preflight.rs`) run **before** any irreversible
publisher fires, and the rollback path (`crates/stage-publish/src/rollback.rs`)
yanks crates.io on a failed required submitter rather than pretending the
release can be undone.

### 6b. Response

1. **Do not roll back past a one-way door.** Once any irreversible publisher has
   succeeded, **fix forward** — never attempt to "undo" a published cargo/npm/
   chocolatey/winget/snap release. The correct move is yank-where-possible plus
   a new fixed version.
2. **Yank / unlist where possible:**
   - crates.io: `cargo yank --version <X.Y.Z> <crate>`.
   - npm: `npm deprecate <pkg>@<X.Y.Z> "compromised — use <X.Y.Z+1>"`.
   - Chocolatey: unlist the version; submit a fixed version.
   - Winget / Snapcraft: submit a removal/fix through the upstream channel.
3. **Delete reversible artifacts:** remove the affected GitHub Release assets
   and blob-storage objects.
4. **Revoke and rotate the signing key** if the tampering implies key
   compromise (follow §5b), and **revoke the cosign signature / GPG signature**
   over the bad artifact so verifiers reject it.
5. **Publish a fixed version** immediately, signed under the rotated key, with a
   fresh SBOM (`stage-sbom`) and attestation (`stage-attest`).
6. **Issue an advisory** (see §8) naming the affected versions, the yank/deprecate
   status, and the verification fingerprints of the fixed release. Request a CVE
   if applicable.

## 7. Investigation & Mitigation

- Determine root cause and full blast radius (which versions, which registries,
  which credentials).
- Patch the underlying defect in anodizer if the incident stemmed from one.
- Add a regression test or pipeline gate so the same class of incident is caught
  before a one-way door fires next time (the pre-publish guard and pre-flight
  checks are the home for such gates).

## 8. Communication & Disclosure

All incident communication occurs through the GitHub Security Advisory until a
fix ships. Once resolved:

1. Coordinate disclosure timing with the reporter.
2. Publish a public advisory: affected versions, impact, yank/deprecate status,
   the fixed version, and how to verify it (signature/attestation/checksum).
3. Request a CVE identifier if applicable.
4. Credit the reporter unless they request anonymity.

## 9. Post-Incident Review

1. Review the response: what was detected, when, and how fast it was contained.
2. Update this plan, `SECURITY.md`, `THREAT_MODEL.md`, and the pipeline gates
   with the lessons learned.
3. For any one-way-door incident, record whether a pre-publish gate *could* have
   caught it earlier — and if so, add that gate.

## See Also

- [`SECURITY.md`](./SECURITY.md) — vulnerability reporting process and timelines.
- [`THREAT_MODEL.md`](./THREAT_MODEL.md) — assets, trust boundaries, and threats.
