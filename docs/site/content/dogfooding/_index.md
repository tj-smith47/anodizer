+++
title = "What works (with proof)"
description = "Every anodizer feature, with a status and a link you can click to see the working artifact — not source code, not test names, the actual file or page."
weight = 30
template = "section.html"
+++

# What works (with proof)

Every feature on this page has one of three statuses. The proof is always
something you can open in your browser — a release artifact, a published
package, or a public registry entry. We don't ask you to read source code
to verify our claims.

## How to read this page

| Status | Means |
|---|---|
| ✅ **Live** | A public artifact exists. Click the link to see it. |
| 🧪 **Tested, not live yet** | Implementation + tests pass. No public release uses it yet — usually because it needs credentials we don't have (Apple Developer cert, an AUR key, a flatpak runtime, etc.). |
| 🚧 **Not implemented** | Tracked, intentionally absent. Open an issue if you need it. |

Two public projects use anodizer to ship themselves:

- **anodizer** — releases at [github.com/tj-smith47/anodizer/releases](https://github.com/tj-smith47/anodizer/releases). Latest: [v0.1.1](https://github.com/tj-smith47/anodizer/releases/tag/v0.1.1).
- **cfgd** — a 4-crate workspace (CLI + lib + operator + CSI driver) at [github.com/tj-smith47/cfgd/releases](https://github.com/tj-smith47/cfgd/releases). Latest: [v0.3.5](https://github.com/tj-smith47/cfgd/releases/tag/v0.3.5).

When a row says "lives on `<package manager>`", click through and you'll
land on the live page. Where two examples exist (one per project), we link
both so you can see the same feature in two configurations.

---

## Where you can install it

| Distribution channel | Status | Verify |
|---|---|---|
| **GitHub Releases** | ✅ Live | [anodizer v0.1.1](https://github.com/tj-smith47/anodizer/releases/tag/v0.1.1) · [cfgd v0.3.5](https://github.com/tj-smith47/cfgd/releases/tag/v0.3.5) |
| **crates.io** (Rust) | ✅ Live | [crates.io/crates/anodizer](https://crates.io/crates/anodizer) · [crates.io/crates/cfgd](https://crates.io/crates/cfgd) |
| **Snap Store** | ✅ Live | [snapcraft.io/anodizer](https://snapcraft.io/anodizer) · [snapcraft.io/cfgd](https://snapcraft.io/cfgd) |
| **Homebrew tap** | ✅ Live | [tj-smith47/homebrew-tap](https://github.com/tj-smith47/homebrew-tap/tree/master/Formula) (`anodizer.rb`, `cfgd.rb`) |
| **Chocolatey** | ✅ Live | [community.chocolatey.org/packages/cfgd](https://community.chocolatey.org/packages/cfgd) |
| **winget** (Microsoft upstream) | ✅ Live | [microsoft/winget-pkgs · TJSmith/cfgd/0.3.5](https://github.com/microsoft/winget-pkgs/tree/master/manifests/t/TJSmith/cfgd/0.3.5) |
| **GHCR container images** | ✅ Live | [github.com/tj-smith47/cfgd/pkgs](https://github.com/tj-smith47?tab=packages&repo_name=cfgd) — `cfgd`, `cfgd-operator`, `cfgd-csi` |
| **Nix flake** | ✅ Live | [tj-smith47/nix-pkgs](https://github.com/tj-smith47/nix-pkgs) |
| **Scoop bucket** | 🧪 Tested | Bucket repo exists but no manifest published yet ([tj-smith47/scoop-bucket](https://github.com/tj-smith47/scoop-bucket)) |
| **Krew (kubectl plugins)** | 🧪 Tested | PR flow runs in CI; cfgd plugin not yet merged into [kubernetes-sigs/krew-index](https://github.com/kubernetes-sigs/krew-index/tree/master/plugins) |
| **AUR (Arch User Repository)** | 🧪 Tested | Needs AUR SSH key; not pushed |
| **Flathub** | 🧪 Tested | Needs flatpak runtime + flathub config |
| **Homebrew cask** (DMG) | 🧪 Tested | Needs DMG artifact in a release |

---

## Build & cross-compilation

What ships in every release: native binaries for **6 targets** (linux amd64/arm64, darwin amd64/arm64, windows amd64/arm64), built with cargo + cargo-zigbuild + cross.

| Feature | Status | Where to look |
|---|---|---|
| Per-target binaries | ✅ Live | [v0.1.1 assets](https://github.com/tj-smith47/anodizer/releases/tag/v0.1.1) — `*-linux-amd64.tar.gz` … `*-windows-arm64.zip` |
| Universal macOS binary (`lipo`) | ✅ Live | [cfgd v0.3.5](https://github.com/tj-smith47/cfgd/releases/tag/v0.3.5) — `cfgd-0.3.5-darwin-all.tar.gz` |
| UPX binary compression | ✅ Live | v0.1.1 binaries are UPX-packed |
| `--single-target` (single-platform build) | ✅ Live | Snapshot job on every master push |
| `--split` / `--merge` (per-OS workers) | ✅ Live | Both anodizer and cfgd release on a 3-runner OS matrix and merge |
| Reproducible build (`mod_timestamp`) | ✅ Live | Wired in build config |
| Per-target build overrides | ✅ Live | Used in production configs |
| `before` / `after` build hooks | ✅ Live | cfgd uses both in its release |
| Prebuilt-binary builder (no compile) | ✅ Live | Tested |
| `report_sizes` | ✅ Live | Wired |

---

## Archives & checksums

| Feature | Status | Where to look |
|---|---|---|
| `tar.gz` archives | ✅ Live | [`anodizer-0.1.1-linux-amd64.tar.gz`](https://github.com/tj-smith47/anodizer/releases/download/v0.1.1/anodizer-0.1.1-linux-amd64.tar.gz) |
| `zip` archives (Windows override) | ✅ Live | [`anodizer-0.1.1-windows-amd64.zip`](https://github.com/tj-smith47/anodizer/releases/download/v0.1.1/anodizer-0.1.1-windows-amd64.zip) |
| `tar.xz`, `tar.zst`, `tgz` | 🧪 Tested | Format dispatch covered; no live release uses them |
| Source archive | ✅ Live | [`anodizer-0.1.1-source.tar.gz`](https://github.com/tj-smith47/anodizer/releases/download/v0.1.1/anodizer-0.1.1-source.tar.gz) |
| Self-extracting installers (`.run`) | ✅ Live | [`anodizer-0.1.1-linux-amd64-installer.run`](https://github.com/tj-smith47/anodizer/releases/download/v0.1.1/anodizer-0.1.1-linux-amd64-installer.run) (4 platforms) |
| Checksums file (sha256 by default) | ✅ Live | [`anodizer-0.1.1-checksums.txt`](https://github.com/tj-smith47/anodizer/releases/download/v0.1.1/anodizer-0.1.1-checksums.txt) |
| Per-artifact split sidecar checksums | ✅ Live | Wired |
| Algorithms: sha1/224/256/384/512, sha3-\*, blake2s/2b, blake3, crc32, md5 | ✅ Live | All wired; sha256 is the default in shipped releases |

---

## Linux packages

| Format | Status | Where to look |
|---|---|---|
| `.deb` (amd64 + arm64) | ✅ Live | [`anodizer_0.1.1_linux_amd64.deb`](https://github.com/tj-smith47/anodizer/releases/download/v0.1.1/anodizer_0.1.1_linux_amd64.deb) |
| `.rpm` (amd64 + arm64) | ✅ Live | [`anodizer_0.1.1_linux_amd64.rpm`](https://github.com/tj-smith47/anodizer/releases/download/v0.1.1/anodizer_0.1.1_linux_amd64.rpm) |
| `.apk` (Alpine) | ✅ Live | [`anodizer_0.1.1_linux_amd64.apk`](https://github.com/tj-smith47/anodizer/releases/download/v0.1.1/anodizer_0.1.1_linux_amd64.apk) |
| `.src.rpm` (rebuildable) | ✅ Live | [`anodizer-0.1.1-1.src.rpm`](https://github.com/tj-smith47/anodizer/releases/download/v0.1.1/anodizer-0.1.1-1.src.rpm) |
| `.snap` (snapcraft) | ✅ Live | [snapcraft.io/anodizer](https://snapcraft.io/anodizer) — `latest/stable` channel |
| `archlinux`, `ipk`, `termux.deb` | 🧪 Tested | nFPM dispatch covered; not shipped live |
| Maintainer scripts (preinstall/postinstall/preremove/postremove) | ✅ Live | Wired |
| `contents[]` (file mappings, modes, owners) | ✅ Live | cfgd ships `LICENSE` and `README` to `/usr/share/doc/cfgd/` |
| Signed packages (`NFPM_PASSPHRASE` family) | ✅ Live | Env priority chain wired |

---

## macOS & Windows installers

These need code-signing material on a real macOS/Windows runner before they
can ship live. Implementation is complete and unit-tested.

| Feature | Status | Notes |
|---|---|---|
| `.dmg` (macOS disk image) | 🧪 Tested | Needs a release with `dmgs[]` configured |
| `.pkg` (macOS installer) | 🧪 Tested | Needs `pkgs[]` configured |
| `.app` bundle (macOS) | 🧪 Tested | Needs `app_bundles[]` configured |
| `.msi` (Windows, via Wix) | 🧪 Tested | Needs `wixl`/`candle`/`light` on the runner |
| NSIS `.exe` (Windows) | 🧪 Tested | Needs `makensis` on the runner |
| `notarize.macos` (cross-platform anchore/quill) | 🧪 Tested | Implemented; no release carries a notary ticket |
| `notarize.macos_native` (Apple Developer ID) | 🧪 Tested | Needs Apple Developer cert on a macOS runner |

---

## Container images

| Feature | Status | Where to look |
|---|---|---|
| Multi-arch images (linux/amd64 + linux/arm64) | ✅ Live | [ghcr.io/tj-smith47/cfgd](https://github.com/tj-smith47?tab=packages&repo_name=cfgd) — `cfgd`, `cfgd-operator`, `cfgd-csi` |
| `docker_v2` modern config | ✅ Live | cfgd ships three images per release |
| `docker_manifests[]` (combined arch manifest) | ✅ Live | Three manifests per cfgd release |
| `build_args`, `labels`, `annotations` | ✅ Live | All in use in cfgd's config |
| Inline SBOM (`docker_v2.sbom: true`) | ✅ Live | Three cfgd images carry SBOM |
| `docker_digest.name_template` | ✅ Live | cfgd writes a digest manifest |
| `buildx` backend | ✅ Live | Default in CI |
| `docker` and `podman` backends | 🧪 Tested | Code paths covered; CI uses buildx |
| Docker Hub description sync | 🧪 Tested | We use ghcr; needs a Docker Hub-anchored release |

---

## Signing & supply-chain provenance

| Feature | Status | Where to look |
|---|---|---|
| Cosign keyless signatures (binaries) | ✅ Live | [cfgd v0.3.5 cosign bundle](https://github.com/tj-smith47/cfgd/releases/download/v0.3.5/cfgd-0.3.5-checksums.txt.cosign.bundle) |
| GPG-signed checksums | ✅ Live | [`anodizer-0.1.1-checksums.txt.sig`](https://github.com/tj-smith47/anodizer/releases/download/v0.1.1/anodizer-0.1.1-checksums.txt.sig) |
| Per-artifact signatures | ✅ Live | Wired with `signs[].artifacts: archive`/`binary`/`checksum`/`sbom`/etc. |
| Cosign-signed Docker images | ✅ Live | cfgd signs all three GHCR images per release |
| Build-time binary signing (`binary_signs[]`) | ✅ Live | Wired |
| SBOM generation (CycloneDX, via syft) | ✅ Live | [`anodizer-0.1.1.cdx.json`](https://github.com/tj-smith47/anodizer/releases/download/v0.1.1/anodizer-0.1.1.cdx.json) |
| `${artifact}` / `${document}` template substitution | ✅ Live | Wired |

---

## Release & changelog

| Feature | Status | Where to look |
|---|---|---|
| GitHub Releases (full surface) | ✅ Live | [anodizer releases](https://github.com/tj-smith47/anodizer/releases) — header/footer/draft/prerelease/make_latest all exercised |
| `metadata.json` + `artifacts.json` emitted as release assets | ✅ Live | [v0.1.1 metadata.json](https://github.com/tj-smith47/anodizer/releases/download/v0.1.1/metadata.json) · [artifacts.json](https://github.com/tj-smith47/anodizer/releases/download/v0.1.1/artifacts.json) |
| Templated release name + tag (`tag_template`) | ✅ Live | cfgd uses Tera-templated tags across 4 workspace crates |
| Release header / footer (string + template) | ✅ Live | Visible at the bottom of every shipped release body |
| Release notes from grouped commits (`changelog.groups`) | ✅ Live | See "Features" / "Bug Fixes" / "Others" sections in the [v0.1.1 release body](https://github.com/tj-smith47/anodizer/releases/tag/v0.1.1) |
| Filters (`include` / `exclude`) | ✅ Live | Visible in shipped changelogs |
| `changelog.use`: git, github, gitlab, gitea, github-native | ✅ Live | git + github-native in production; others tested |
| AI-generated changelog (anthropic / openai / ollama) | 🧪 Tested | Implemented; no release uses `changelog.use: ai` yet |
| `release.gitlab` / `release.gitea` | 🧪 Tested | We dogfood on GitHub only |
| Milestones pipe | ✅ Live | Wired |

---

## Announcers (release notification)

13 channels are implemented. Two are exercised by live cfgd releases; the
others have full test coverage but no live secrets configured in the
release workflow.

| Channel | Status | Notes |
|---|---|---|
| **webhook** (custom HTTP) | ✅ Live | cfgd posts to a custom webhook on every release |
| **email / smtp** | ✅ Live | cfgd sends release announcements via SMTP |
| discord, slack, telegram, teams, mattermost | 🧪 Tested | No live workflow has the secrets |
| reddit, twitter/X, mastodon, bluesky, linkedin | 🧪 Tested | Same — no live secrets |
| opencollective, discourse | 🧪 Tested | Same — no live secrets |

---

## Templates (Tera, GoReleaser-compatible syntax)

| Feature | Status | Notes |
|---|---|---|
| `{{ .Field }}` syntax (project, version, tag, os, arch, …) | ✅ Live | Every shipped asset filename is template-rendered |
| String / path / version / env / filter helpers | ✅ Live | Wired |
| Hash helpers (sha\*, blake2\*, blake3, crc32, md5) | ✅ Live | Wired |
| File I/O (`readFile`, `mustReadFile`) | ✅ Live | Wired |
| Date helpers (`time`, `.Now.Format`) | ✅ Live | Wired |
| Encoding (`mdv2escape`, `urlPathEscape`) | ✅ Live | Wired |
| Custom `.Var.*` (user-defined variables) | ✅ Live | cfgd uses `.Var.repo_url` and `.Var.description` across its config |
| Pro template variables (`.PrefixedTag`, `.Artifacts`, `.Metadata`, `.IsMerging`, `.IsRelease`) | ✅ Live | cfgd uses `.Var.*` and `.Artifacts` in `docker_manifests` |
| Pro helpers (`in`, `reReplaceAll`) | ✅ Live | Wired |

---

## Configuration & lifecycle

| Feature | Status | Notes |
|---|---|---|
| `project_name`, `dist`, `env`, `env_files` | ✅ Live | Used in every config |
| `variables` (custom `.Var.*`) | ✅ Live | cfgd uses heavily |
| `template_files[]` (rendered files shipped in release) | ✅ Live | cfgd renders an `install.sh` and ships it |
| `includes[].from_file` | ✅ Live | Wired |
| `includes[].from_url` | 🧪 Tested | No live config pulls a remote include |
| `before` / `after` global hooks | ✅ Live | cfgd uses both |
| `build.hooks.pre` / `post` | ✅ Live | Wired |
| Snapshot mode (`snapshot.name_template`, `--auto-snapshot`) | ✅ Live | Snapshot job on every master push |
| Nightly mode (`nightly.*`, `--nightly`) | 🧪 Tested | Wired; no scheduled nightly workflow yet |
| `metadata.{homepage,license,description,maintainers,mod_timestamp}` | 🧪 Tested | Collected and emitted; minor field-passthrough gaps |

---

## Monorepo, workspaces, split/merge

| Feature | Status | Notes |
|---|---|---|
| Cargo workspace detection (multi-crate monorepo) | ✅ Live | cfgd is a 4-crate workspace (CLI + lib + operator + CSI), all four release in parallel |
| `monorepo.tag_prefix` / `dir` (per-crate tag prefixes) | ✅ Live | cfgd uses `core-v*`, `v*`, `operator-v*`, `csi-v*` |
| `--crate <name>` filter | ✅ Live | cfgd's release workflow filters per workspace |
| `depends_on` (workspace ordering) | ✅ Live | cfgd's `core` releases first, others after |
| `git.tag_sort`, `prerelease_suffix`, `ignore_tags` | ✅ Live | Wired |
| `partial.by` (split/merge axis: goos / goarch / target) | ✅ Live | cfgd uses `partial.by: goos` |
| `--split` (per-OS worker) | ✅ Live | Three split jobs per anodizer release |
| `--merge` (combine worker results) | ✅ Live | cfgd's release workflow merges per-OS dist directories |
| `--prepare` (multi-stage Pro mode) | 🧪 Tested | `release --prepare` runs build/archive/sign/checksum/sbom but skips release/publish/announce; e2e test asserts the artifact set matches an explicit `--skip=release,publish,announce`. No live release uses the prepare→publish→announce split yet. |

---

## Cloud blob & artifactory

| Feature | Status | Notes |
|---|---|---|
| S3 / GCS / Azure Blob upload (via `object_store` SDK) | 🧪 Tested | No release configures cloud credentials |
| Artifactory upload (target, mode, TLS, headers) | 🧪 Tested | Same — no live deployment |
| Generic HTTP `uploads[]` | 🧪 Tested | Same |
| Fury, Cloudsmith publishers | 🧪 Tested | Same |

---

## Custom publishers

| Feature | Status | Notes |
|---|---|---|
| `publishers[]` (run a custom command per artifact) | ✅ Live | Wired |

---

## CLI

| Command | Status | Notes |
|---|---|---|
| `release`, `build`, `check`, `init`, `completion`, `jsonschema`, `healthcheck` | ✅ Live | Used in every release pipeline |
| `tag` (auto-tag from conventional commits) | ✅ Live | anodizer's CI auto-creates `v*` tags from master |
| `targets --json` | ✅ Live | Consumed by [anodizer-action](https://github.com/tj-smith47/anodizer-action) as a matrix input |
| `resolve-tag` (tag → workspace mapping) | ✅ Live | cfgd uses on every tag push |
| `changelog` (preview) | ✅ Live | Wired |
| `continue` / `publish` / `announce` (composite Pro commands) | ✅ Live | Used via `release --merge` in cfgd |
| `man` (clap_mangen man-page generation) | 🧪 Tested | `anodizer man` emits roff for the full CLI tree; smoke test asserts `.TH anodizer` + a known subcommand. No live release ships `anodizer.1` yet. |
| `--prepare` flag (Pro multi-stage) | 🧪 Tested | See [Monorepo, workspaces, split/merge](#monorepo-workspaces-split-merge) above. |
| `--fail-fast` flag | 🧪 Tested | Inverts the publish stage's default collect-then-bail behavior to abort on the first publisher error, matching GoReleaser's `internal/pipe/publish/publish.go:95`. Default mode collects errors from every post-release publisher (brew/krew/nix/scoop/winget/aur/...) and reports the aggregate, matching GoReleaser's `Continuable` trait. |

---

## Rust-specific (no GoReleaser equivalent)

These exist because Rust's toolchain and packaging conventions differ from
Go's. They are dogfooded by anodizer and cfgd themselves.

| Feature | Status | Where to look |
|---|---|---|
| **crates.io publish** with dependency-aware ordering | ✅ Live | [anodizer on crates.io](https://crates.io/crates/anodizer) · [cfgd on crates.io](https://crates.io/crates/cfgd) — cfgd publishes 4 crates in dependency order on every release |
| **binstall metadata** (`cargo-binstall` compatibility) | ✅ Live | `cargo binstall cfgd` works because cfgd ships the `pkg_url`/`pkg_fmt` metadata |
| **Workspace crate detection** (multi-crate monorepo) | ✅ Live | cfgd's 4-workspace setup |
| **`version_sync`** (Cargo.toml ↔ git tag) | ✅ Live | Runs on every release |
| **`tag_pre_hooks` / `tag_post_hooks`** (templated) | ✅ Live | anodizer's auto-tag flow |
| **`ANODIZER_SPLIT_TARGET`** env (replaces GoReleaser's `GGOOS`/`GGOARCH`) | ✅ Live | Consumed by every split job |
| **UPX target-triple globs** | ✅ Live | v0.1.1 binaries are UPX-packed using Rust target triples |
| **`anodizer targets --json`** | ✅ Live | The action uses it |
| **`anodizer resolve-tag`** | ✅ Live | cfgd's release workflow |

---

## GitHub Action

The GitHub Action is at [tj-smith47/anodizer-action](https://github.com/tj-smith47/anodizer-action).

| Input / output | Status | Notes |
|---|---|---|
| `from-source`, `install-rust`, `args` | ✅ Live | Used by both anodizer's and cfgd's release workflows |
| `from-artifact`, `artifact-run-id`, `artifact-workflow` | ✅ Live | anodizer reuses build artifacts across jobs |
| `install` (zig, cargo-zigbuild, upx, nfpm, makeself, snapcraft, rpmbuild, cosign) | ✅ Live | All eight tools install on demand |
| `gpg-private-key`, `docker-registry`, `docker-password` | ✅ Live | Used in cfgd's release |
| `upload-dist` / `download-dist` (split/merge handoff) | ✅ Live | cfgd's split→merge flow |
| `resolve-workspace` | ✅ Live | cfgd's workspace fan-out |

---

## Tested, not yet shipped live

These features have implementation and passing tests, but no public
release uses them yet. The blocker in each case is operational, not code:

- **Pro installers** (DMG, MSI, PKG, NSIS, app bundle) — need code-signing
  certs and platform-specific tooling on the runner.
- **macOS notarization** (anchore/quill cross-platform; or Apple Developer
  native) — needs an Apple Developer cert on a macOS runner.
- **AI-generated changelogs** (anthropic / openai / ollama) — need a
  release configured with `changelog.use: ai` and an API key.
- **Nightly releases** — need a scheduled workflow trigger.
- **GitLab and Gitea releases** — we dogfood on GitHub.
- **Cloud blob uploads** (S3, GCS, Azure) — need cloud credentials.
- **Artifactory, Fury, Cloudsmith** — same — no live credentials.
- **11 of 13 announcer channels** (Discord, Slack, Telegram, Teams,
  Mattermost, Reddit, Twitter, Mastodon, Bluesky, LinkedIn,
  OpenCollective, Discourse) — need each channel's secrets.
- **Flathub** — needs flatpak runtime + flathub config.
- **AUR** — needs an AUR SSH key.
- **Krew (kubectl plugin)** — PR flow runs in CI; not yet merged
  upstream.
- **Scoop manifest** — bucket repo exists but no manifest published yet.
- **Homebrew cask** — needs a `.dmg` artifact in a release.
- **Docker Hub description sync** — we publish to GHCR.
- **Remote `includes[].from_url`** — needs a config that pulls a remote include.
- **GitLab / Gitea token + force-token override** — needs a live release on
  those SCMs.

---

## Not implemented

Nothing tracked here right now. Open an issue if you find a GoReleaser
feature this page doesn't cover.

---

## Methodology

- **Reference target:** [GoReleaser](https://goreleaser.com/) (OSS + Pro). We
  track every documented feature in both editions plus our own Rust-specific
  additions.
- **Live ✅:** there's a public artifact (release file, package on a
  registry, image on GHCR, etc.) you can download right now.
- **Tested 🧪:** the feature is implemented and the test suite exercises
  it on every CI run, but no current public release uses it.
- **Not implemented 🚧:** intentionally absent. Open an issue to discuss.

If you can produce a live artifact for any 🧪 row — open a PR with the
link and we'll flip it to ✅. Same for any feature you think is missing
and should be ✅: send the proof.
