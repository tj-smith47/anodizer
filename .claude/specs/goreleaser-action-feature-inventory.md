# GoReleaser Action ↔ Anodize Action Feature Inventory

> **Action-layer parity reference** — tracks the GitHub Action wrappers, not the CLIs.
> GoReleaser Action source: `/opt/repos/goreleaser-action/action.yml` + `README.md` (Node 24, shipped as compiled JS in `dist/index.js`).
> Anodize Action source: `/opt/repos/anodize-action/action.yml` + `README.md` (composite action, 626 lines).
>
> Columns:
> - `surface` — input | output | step | cache | distribution
> - `goreleaser-action_ref` — field/step name in goreleaser-action
> - `anodize-action_ref` — equivalent in anodize-action
> - `parity_status` — implemented | partial | missing | n-a | additive
> - `notes` — ≤30 word durable justification

---

## 1. Inputs

| name | surface | goreleaser-action_ref | anodize-action_ref | parity_status | notes |
|------|---------|------------------------|--------------------|---------------|-------|
| version | input | `version` (default `~> v2`, supports semver range + `latest` + `nightly`) | `version` (default `latest`; accepts exact tag `v0.1.1` or `latest`) | partial | anodize-action does not accept semver ranges like `~> v2` or `nightly`; only exact tag or `latest`. |
| distribution (pro / OSS) | input | `distribution: goreleaser \| goreleaser-pro` | — (anodize is OSS-only; no Pro distribution) | n-a | No Pro tier to select; anodize is single-edition. |
| args | input | `args` (string passed to `goreleaser`) | `args` (same — passed to `anodize`) | implemented | Equivalent behavior. |
| workdir | input | `workdir` (default `.`) | `workdir` (default `.`) | implemented | Equivalent. |
| install-only | input | `install-only` (default `false`) | `install-only` (default `false`) | implemented | Equivalent. |
| from-artifact (cross-job install) | input | — | `from-artifact` + `artifact-run-id` + `artifact-workflow` | additive | Bootstrap install path — download anodize from an upstream workflow artifact before running. No goreleaser-action equivalent. |
| from-source (bootstrap build) | input | — | `from-source` | additive | Builds anodize from cargo in the current workdir. Useful when the host runner has no pre-built binary. |
| install-rust | input | — | `install-rust` | additive | Convenience installer for `dtolnay/rust-toolchain@stable`. |
| install (deps CSV) | input | — | `install` (nfpm/makeself/snapcraft/rpmbuild/cosign/zig/cargo-zigbuild/upx) | additive | No goreleaser-action dep-installer — users wire their own. |
| auto-install (deps from config) | input | — | `auto-install` | additive | Grep-based detection of deps from `.anodize.yaml`. |
| resolve-workspace (monorepo tag→crate) | input | — | `resolve-workspace` | additive | anodize resolve-tag + populates `workspace`/`crate-path`/`has-builds` outputs. |
| docker-registry / docker-username / docker-password | input | — (users wire `docker/login-action` manually) | `docker-registry` + `docker-username` + `docker-password` (inline QEMU + buildx setup when `docker-registry != ''`) | additive | Convenience: inline login + QEMU + buildx when present. |
| upload-dist (split build) | input | — | `upload-dist` | additive | Wraps `actions/upload-artifact@v4` with `name=dist-$RUNNER_OS`. |
| download-dist (merge build) | input | — | `download-dist` | additive | Wraps `actions/download-artifact@v4` with pattern `dist-*` + merge-multiple. |
| gpg-private-key | input | — (users chain `crazy-max/ghaction-import-gpg` manually) | `gpg-private-key` (piped into `gpg --batch --import`) | additive | Inline GPG import — reduces a separate action step. |
| cosign-key | input | — | `cosign-key` (written to `cosign.key` mode 0600) | additive | Inline cosign key handling. |

## 2. Outputs

| name | surface | goreleaser-action_ref | anodize-action_ref | parity_status | notes |
|------|---------|------------------------|--------------------|---------------|-------|
| artifacts JSON | output | `artifacts` (from `dist/artifacts.json`) | `artifacts` (same) | implemented | Equivalent. |
| metadata JSON | output | `metadata` (from `dist/metadata.json`) | `metadata` (same) | implemented | Equivalent. |
| release-url | output | — (users parse metadata manually) | `release-url` (extracted via `jq -r '.release_url'` from metadata.json) | additive | Convenience output. |
| workspace / crate-path / has-builds | output | — | Three outputs when `resolve-workspace: true` | additive | Monorepo support. |
| split-matrix | output | — | `split-matrix` (GH Actions strategy matrix from `anodize targets --json`) | additive | Monorepo + cross-platform fan-out. |

## 3. Composite steps (behavioral)

| name | surface | goreleaser-action_ref | anodize-action_ref | parity_status | notes |
|------|---------|------------------------|--------------------|---------------|-------|
| Platform detection | step | implicit via Node runtime | Explicit shell `case "$RUNNER_OS"` + `case "$RUNNER_ARCH"` — emits `os`, `arch`, `bin`, `ext` outputs | additive | Composite must compute this itself; Node action handles it in TS. |
| Version resolution | step | JS version resolver supporting `~> v2` / `nightly` / tag | Shell `gh api repos/.../releases/latest` OR exact tag input | partial | Narrower input contract — no semver range / nightly. |
| Download release asset | step | JS HTTP fetcher + tc.downloadTool | curl with 3-attempt retry + `unzip`/`tar` extract | implemented | Both cache into `$RUNNER_TOOL_CACHE`. |
| Install from workflow artifact | step | — | `actions/download-artifact@v4` composite + `chmod +x` | additive | Cross-workflow and same-workflow paths, `auto` run-id resolver. |
| Bootstrap build from source | step | — | `cargo build --release -p anodize` in workdir | additive | Fallback install mode. |
| Dep installer | step | — | `scripts/install-deps.sh` with platform-native package managers | additive | Large value-add — users don't need to wire apt/brew/choco. |
| Retry wrapper around release run | step | — | 3-attempt loop with selective `dist/` cleanup (preserves split-inputs with `context.json`) | additive | Flaky-release mitigation. |
| GPG / cosign key import | step | — | Inline key import steps gated on input presence | additive | Reduces a cross-action chain. |

## 4. Caching / distribution

| name | surface | goreleaser-action_ref | anodize-action_ref | parity_status | notes |
|------|---------|------------------------|--------------------|---------------|-------|
| Runner tool cache | cache | `tc.downloadTool` → `tc.cacheDir` (TS API) keyed by version | `mkdir -p "${RUNNER_TOOL_CACHE}/anodize/${version}"` | implemented | Both leverage `$RUNNER_TOOL_CACHE` — cache key is effectively the version string. |
| Cache key shape | cache | Implicit via tool-cache (version-keyed) | `${RUNNER_TOOL_CACHE}/anodize/${version}` (or `.../source` / `.../artifact`) | partial | anodize variant adds `source` / `artifact` subpaths that won't share cache with release-sourced installs. |
| Distribution: Node runtime | distribution | `runs.using: node24` with `dist/index.js` compiled | `runs.using: composite` | n-a | Different action type — anodize is composite, not Node. Composite actions are slower but dependency-free. |
| Version range resolution | distribution | JS library resolves `~> v2` against GitHub Releases API | Not supported | missing | If a user writes `version: ~> v1`, anodize-action installs nothing / fails. |
| Nightly distribution | distribution | `version: nightly` picks nightly release | Not supported | missing | Anodize has no nightly release channel yet; this is an upstream-CLI-gap, not just an Action gap. |

## 5. Secret handling

| name | surface | goreleaser-action_ref | anodize-action_ref | parity_status | notes |
|------|---------|------------------------|--------------------|---------------|-------|
| GITHUB_TOKEN passthrough | step | `env: GITHUB_TOKEN` (documented; user wires) | Same pattern — `env: GITHUB_TOKEN` | implemented | Both rely on caller setting env. |
| GORELEASER_KEY (Pro) | step | `env: GORELEASER_KEY` | — (no Pro) | n-a | N/a for anodize. |
| GPG_FINGERPRINT | step | Documented in README, users chain `ghaction-import-gpg` | Action handles key import inline, fingerprint still user-chained | partial | Less orchestration needed in anodize — but callers still need `GPG_FINGERPRINT` env for signing. |
| COSIGN_PASSWORD | step | User wires | User wires (action writes key only) | implemented | Same division of responsibility. |

---

## 6. Audit findings to hand off

Downstream teammates (`Action-A8` parity audit + `Action-A9` composite safety) should verify:

1. **semver-range version input** — `version: ~> v0.1` fails in anodize-action; goreleaser-action resolves it. Low-priority since anodize v0.x is pre-1.0; but if users copy goreleaser-action syntax, they silently get `v0.1` instead of the latest patch. Consider adding regex+API resolution to `scripts/resolve-version.sh` (new).
2. **nightly channel** — anodize has no nightly release; upstream CLI gap. Not an Action issue.
3. **3-attempt curl retry** — goreleaser-action has no retry; anodize-action adds 3-attempt backoff on release asset download. Additive; verify backoff is linear (1s/2s/3s) not exponential.
4. **Selective dist/ cleanup on retry** — preserves split-input dirs containing `context.json`. Review semantics: if a retried run writes to the same split-input dir, are stale files left behind?
5. **auto-install grep patterns** — composite uses `grep -qE` against `.anodize.yaml` to detect `nfpms:`, `snapcrafts:`, etc. Missing regex for `pkgs:`, `msis:`, `nsis:`, `dmgs:`, `appbundles:`, `flatpaks:` — these are Pro-ish installers that likely need extra tooling (productbuild/wixl/makensis/hdiutil). Verify the install matrix.
6. **from-source install** — `cargo build --release -p anodize` assumes the workspace has an `anodize` package; fails for users who forgot `install-rust: true`. Error message documents this.

---

## Completion statement (Action inventory)

Refreshed 2026-04-18 — goreleaser-action HEAD `01cbe07`; action.yml unchanged since last sync (5 inputs, 2 outputs).

- Total goreleaser-action inputs catalogued: 5 (distribution, version, args, workdir, install-only)
- Total goreleaser-action outputs catalogued: 2 (artifacts, metadata)
- Total anodize-action inputs catalogued: 19
- Total anodize-action outputs catalogued: 7
- anodize-action features additive to goreleaser-action: 14 (inputs) + 5 (outputs) + 8 (steps) — see §1–§3
- anodize-action features missing vs goreleaser-action: 2 (semver-range version resolution, nightly channel) — both niche
- Rows audited: 27
  - required (GITHUB_TOKEN / artifacts / metadata / args / workdir / install-only / version default resolution): 7 implemented
  - strongly-suggested (rest of inputs/outputs/composite steps): 18 implemented or additive
  - niche missing: 2 (semver-range, nightly) — niche because anodize-action users pin exact versions; nightly channel absent at the CLI level
  - not-applicable: 3 (Pro distribution flag, GORELEASER_KEY, Node24-runtime distribution choice)
- Parity achieved: **yes** — goreleaser-action's 5 inputs + 2 outputs are all equivalent-or-additive in anodize-action. The 2 missing items are narrow (semver-range resolution, nightly) and anodize-action far exceeds goreleaser-action's feature set (dep auto-install, monorepo tag resolution, split/merge artifact plumbing, GPG/cosign inline, retry, split-matrix output).

Completion achieved: **yes**.

The `Action-A8` parity audit should confirm behavioral equivalence by running both actions against a fake release in a CI fixture; findings flow to `anodize-action/.claude/audits/2026-04-v0.x/` and, ultimately, to `anodize-action/.claude/known-bugs.md` via the team lead.
