+++
title = "What anodizer builds"
description = "Artifacts the `anodizer release` pipeline produces: binaries, archives, packages, installers, containers, and signing material."
weight = 20
template = "section.html"
+++

# What anodizer builds

Output formats and the `builds[]` / `archives[]` / `dockers_v2[]` / `signs[]`
keys that drive them. Native binaries for 6 targets ship on every release
(linux amd64/arm64, darwin amd64/arm64, windows amd64/arm64), built with
cargo + cargo-zigbuild + cross.

## Live configuration

Build / archive / nfpm / dockers_v2 / sign blocks from
[`cfgd/.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml)
(snapshot 2026-05-24) — every key referenced in the tables below is wired
here.

```yaml
defaults:
  targets:
    - x86_64-unknown-linux-gnu
    - aarch64-unknown-linux-gnu
    - x86_64-apple-darwin
    - aarch64-apple-darwin
    - x86_64-pc-windows-msvc
  cross: auto

# Per-crate (one workspace shown):
builds:
  - binary: cfgd
    mod_timestamp: "{{ CommitTimestamp }}"

archives:
  - name_template: "{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}"
    formats: [tar.gz]
    format_overrides:
      - { os: windows, formats: [zip] }
    files: [LICENSE, README.md]

universal_binaries:
  - { name_template: "{{ ProjectName }}", replace: false }

checksum:
  name_template: "{{ ArtifactName }}.sha256"
  algorithm: sha256
  split: true

# Top-level:
upx:
  - id: default
    enabled: true
    args: ["--best", "--lzma"]
    targets: [x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu,
              x86_64-apple-darwin, x86_64-pc-windows-msvc]

nfpms:
  - id: cfgd
    formats: [deb, rpm, apk]
    maintainer: "TJ Smith <tj@jarvispro.io>"
    contents:
      - { src: LICENSE,   dst: /usr/share/doc/cfgd/LICENSE }
      - { src: README.md, dst: /usr/share/doc/cfgd/README.md }

# dockers_v2: pushes a multi-arch image index in one step (no separate manifest).
dockers_v2:
  - id: cfgd
    dockerfile: Dockerfile.agent.release
    images: ["ghcr.io/tj-smith47/cfgd"]
    tags: ["{{ Version }}", "v{{ Version }}", "latest"]
    sbom: true

signs:
  - { id: cosign-checksum, artifacts: checksum, cmd: cosign }
  - { id: cosign-source,   artifacts: source,   cmd: cosign }
docker_signs:
  - { id: cosign-images, artifacts: manifests, cmd: cosign }
binary_signs:
  - { id: cosign-bin,    artifacts: binary,    cmd: cosign }

sboms:
  - { id: default, cmd: syft, artifacts: archive, documents: ["{{ .ProjectName }}-{{ .Version }}.cdx.json"] }
```

## Build

| Key | Status | Notes |
|---|---|---|
| `builds[].targets` → per-target `os` / `arch` | ✅ Verified | [v0.1.1 assets](https://github.com/tj-smith47/anodizer/releases/tag/v0.1.1) cover 6 targets (`*-linux-amd64.tar.gz` to `*-windows-arm64.zip`) |
| `universal_binaries[]` | ✅ Verified | [cfgd v0.3.5](https://github.com/tj-smith47/cfgd/releases/tag/v0.3.5) ships `cfgd-0.3.5-darwin-all.tar.gz` via `lipo` |
| `upx[]` | ✅ Verified | [`anodizer-0.1.1-linux-amd64.tar.gz`](https://github.com/tj-smith47/anodizer/releases/download/v0.1.1/anodizer-0.1.1-linux-amd64.tar.gz) (UPX-packed) |
| `builds[].overrides` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`format_overrides` for windows zip) |
| `builds[].hooks.pre` / `post` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (archive `hooks.before` / `hooks.after`) |
| `builds[].mod_timestamp` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`metadata.mod_timestamp: "{{ CommitTimestamp }}"`) |
| `builds[].builder: prebuilt` (no-compile) | 🤝 Help wanted | [`crates/stage-build/src/run.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-build/src/run.rs) imports a pre-built binary; [`crates/stage-build/src/tests.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-build/src/tests.rs) covers unit paths; no production `.anodizer.yaml` uses `builder: prebuilt` yet |
| `report_sizes` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`report_sizes: true`) |

## Archives and checksums

| Format | Status | Notes |
|---|---|---|
| `tar.gz` | ✅ Verified | [`anodizer-0.1.1-linux-amd64.tar.gz`](https://github.com/tj-smith47/anodizer/releases/download/v0.1.1/anodizer-0.1.1-linux-amd64.tar.gz) |
| `zip` | ✅ Verified | [`anodizer-0.1.1-windows-amd64.zip`](https://github.com/tj-smith47/anodizer/releases/download/v0.1.1/anodizer-0.1.1-windows-amd64.zip) |
| `tar.xz`, `tar.zst`, `tgz` | ✅ Verified | [`anodizer-0.16.0-linux-amd64-extra.tar.xz`](https://github.com/tj-smith47/anodizer/releases/download/v0.16.0/anodizer-0.16.0-linux-amd64-extra.tar.xz), [`anodizer-0.16.0-linux-amd64-extra.tar.zst`](https://github.com/tj-smith47/anodizer/releases/download/v0.16.0/anodizer-0.16.0-linux-amd64-extra.tar.zst), [`anodizer-0.16.0-windows-amd64-extra.tgz`](https://github.com/tj-smith47/anodizer/releases/download/v0.16.0/anodizer-0.16.0-windows-amd64-extra.tgz) (second `archives[]` entry with `formats: [tar.xz, tar.zst]` + `tgz` override) |
| `source.format` | ✅ Verified | [`anodizer-0.1.1-source.tar.gz`](https://github.com/tj-smith47/anodizer/releases/download/v0.1.1/anodizer-0.1.1-source.tar.gz) |
| `makeselfs[]` | ✅ Verified | [`anodizer-0.1.1-linux-amd64-installer.run`](https://github.com/tj-smith47/anodizer/releases/download/v0.1.1/anodizer-0.1.1-linux-amd64-installer.run) (4 platforms) |

| Key | Status | Notes |
|---|---|---|
| `checksum.algorithm` | ✅ Verified | sha256 default. [`anodizer-0.1.1-checksums.txt`](https://github.com/tj-smith47/anodizer/releases/download/v0.1.1/anodizer-0.1.1-checksums.txt). Full list: sha1/224/256/384/512, sha3-*, blake2s/2b, blake3, crc32, md5 |
| `checksum.split` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`checksum.split: true` per crate) |

## Linux packages

| Format | Status | Notes |
|---|---|---|
| `.deb` | ✅ Verified | [`anodizer_0.1.1_linux_amd64.deb`](https://github.com/tj-smith47/anodizer/releases/download/v0.1.1/anodizer_0.1.1_linux_amd64.deb) (amd64 + arm64) |
| `.rpm` | ✅ Verified | [`anodizer_0.1.1_linux_amd64.rpm`](https://github.com/tj-smith47/anodizer/releases/download/v0.1.1/anodizer_0.1.1_linux_amd64.rpm) (amd64 + arm64) |
| `.apk` | ✅ Verified | [`anodizer_0.1.1_linux_amd64.apk`](https://github.com/tj-smith47/anodizer/releases/download/v0.1.1/anodizer_0.1.1_linux_amd64.apk) |
| `.src.rpm` | ✅ Verified | [`anodizer-0.1.1-1.src.rpm`](https://github.com/tj-smith47/anodizer/releases/download/v0.1.1/anodizer-0.1.1-1.src.rpm) |
| `.snap` | ✅ Verified | [snapcraft.io/anodizer](https://snapcraft.io/anodizer), `latest/stable` channel |
| `archlinux`, `ipk`, `termux.deb` | 🤝 Help wanted | nFPM dispatch covered; not shipped live |

| Key | Status | Notes |
|---|---|---|
| `nfpms[].scripts` | ✅ Verified | [`crates/core/src/config/nfpm.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/core/src/config/nfpm.rs) (`preinstall` / `postinstall` / `preremove` / `postremove` fields) |
| `nfpms[].contents` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`contents:` ships `LICENSE` + `README.md` to `/usr/share/doc/cfgd/`) |
| `NFPM_PASSPHRASE` env chain | ✅ Verified | [`crates/stage-nfpm/src/builders.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-nfpm/src/builders.rs) (three-level lookup chain) |

## macOS and Windows installers (built on Linux CI)

These formats are assembled **on an ordinary Linux runner** — no macOS or
Windows host in the build matrix. Anodizer's own dogfood config wires all five
([`anodizer .anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml),
`app_bundles:` / `dmgs:` / `pkgs:` / `msis:` / `nsis:` blocks), built unsigned
in CI. Code-signing and notarization still require the platform's own
credentials; the bundles themselves do not. As of
[v0.16.0](https://github.com/tj-smith47/anodizer/releases/tag/v0.16.0) all of
them — including the Linux-desktop `.AppImage` and `.flatpak` bundles — ship
as live release assets (amd64 + arm64).

| Format | Status | Built on Linux via |
|---|---|---|
| `.app` bundle | ✅ Verified | [`anodizer_amd64.dmg`](https://github.com/tj-smith47/anodizer/releases/download/v0.16.0/anodizer_amd64.dmg) ships the bundle (in-process directory + `Info.plist` assembly, no external tool); [`app_bundles:`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml). See [app-bundle docs](../../../docs/packages/app-bundles/) |
| `.dmg` | ✅ Verified | [`anodizer_amd64.dmg`](https://github.com/tj-smith47/anodizer/releases/download/v0.16.0/anodizer_amd64.dmg) + [`anodizer_arm64.dmg`](https://github.com/tj-smith47/anodizer/releases/download/v0.16.0/anodizer_arm64.dmg) via `genisoimage` / `mkisofs`; [`dmgs:`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml). See [dmg docs](../../../docs/packages/dmg/) |
| `.pkg` | ✅ Verified | [`anodizer_amd64.pkg`](https://github.com/tj-smith47/anodizer/releases/download/v0.16.0/anodizer_amd64.pkg) + [`anodizer_arm64.pkg`](https://github.com/tj-smith47/anodizer/releases/download/v0.16.0/anodizer_arm64.pkg) via flat XAR toolchain (`xar` + `mkbom`), byte-reproducible TOC; [`pkgs:`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml). See [pkg docs](../../../docs/packages/pkg/) |
| `.msi` | ✅ Verified | [`anodizer_amd64.msi`](https://github.com/tj-smith47/anodizer/releases/download/v0.16.0/anodizer_amd64.msi) + [`anodizer_arm64.msi`](https://github.com/tj-smith47/anodizer/releases/download/v0.16.0/anodizer_arm64.msi) via `wixl` (msitools); [`msis:`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml). See [msi docs](../../../docs/packages/msi/) |
| `.exe` (NSIS) | ✅ Verified | [`anodizer_x64-setup.exe`](https://github.com/tj-smith47/anodizer/releases/download/v0.16.0/anodizer_x64-setup.exe) + [`anodizer_arm64-setup.exe`](https://github.com/tj-smith47/anodizer/releases/download/v0.16.0/anodizer_arm64-setup.exe) via `makensis`; [`nsis:`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml). See [nsis docs](../../../docs/packages/nsis/) |
| `.AppImage` | ✅ Verified | [`anodizer-0.16.0-amd64.AppImage`](https://github.com/tj-smith47/anodizer/releases/download/v0.16.0/anodizer-0.16.0-amd64.AppImage) + [`anodizer-0.16.0-arm64.AppImage`](https://github.com/tj-smith47/anodizer/releases/download/v0.16.0/anodizer-0.16.0-arm64.AppImage), each with a `.zsync` delta-update sidecar, via `linuxdeploy`; [`appimages:`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml). See [appimage docs](../../../docs/packages/appimage/) |
| `.flatpak` | ✅ Verified | [`anodizer_0.16.0_linux_amd64.flatpak`](https://github.com/tj-smith47/anodizer/releases/download/v0.16.0/anodizer_0.16.0_linux_amd64.flatpak) + [`anodizer_0.16.0_linux_arm64.flatpak`](https://github.com/tj-smith47/anodizer/releases/download/v0.16.0/anodizer_0.16.0_linux_arm64.flatpak) single-file bundles; [`flatpaks:`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`org.freedesktop.Platform` 24.08 runtime with host-surface `finish_args`). Flathub distribution itself is still open — see [Where you install it](../install/) |

| Key | Status | Notes |
|---|---|---|
| `notarize.macos` | 🤝 Help wanted | Cross-platform (rcodesign). Implementation requires `sign.certificate` (P12 file), `sign.password`, and `notarize.{issuer_id, key, key_id}`, i.e. an Apple Developer Program membership. Not dogfoodable on Linux runners without a paid Apple account |
| `notarize.macos_native` | 🤝 Help wanted | Needs Apple Developer cert on a macOS runner |

## Container images

| Key | Status | Notes |
|---|---|---|
| `dockers_v2[]` | ✅ Verified | [ghcr.io/tj-smith47/cfgd](https://github.com/tj-smith47?tab=packages&repo_name=cfgd) (`cfgd-agent`, `cfgd-operator`, `cfgd-csi`); [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`dockers_v2:` per crate) |
| `docker_manifests[]` | ✅ Verified | [`ghcr.io/tj-smith47/cfgd:v0.3.5`](https://github.com/tj-smith47/cfgd/pkgs/container/cfgd) (multi-arch linux/amd64+arm64). `dockers_v2` already pushes a multi-arch index, so cfgd's `docker_manifests[]` entries are bypassed at runtime (`docker: skipping manifest ... already pushed as multi-arch by docker_v2`) — retained only for the niche case of stitching together images not built by `dockers_v2` in the same run |
| `dockers_v2[].build_args` / `labels` / `annotations` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`build_args.VERSION` + `org.opencontainers.image.*` annotations) |
| `dockers_v2[].sbom: true` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`sbom: true` on all three `dockers_v2` images) |
| `docker_digest.name_template` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`docker_digest.name_template: "cfgd_{{ .Tag }}.digest"`) |
| `dockers_v2[].use: buildx` | ✅ Verified | [`crates/stage-docker/src/detect.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-docker/src/detect.rs) (buildx is the default backend) |
| `dockers_v2[].use: podman` / `docker_manifests[].use: docker` / `podman` | 🤝 Help wanted | Linux-only backend selectors. No live release exercises the non-buildx path |
| `docker_hub.description` | ✅ Verified | Live at [hub.docker.com/r/tsmthtj/anodizer](https://hub.docker.com/r/tsmthtj/anodizer) — the `dockerhub:` publisher syncs `description` + `full_description` (from `README.md`) each release; at v0.15.5 ([run 28882554907](https://github.com/tj-smith47/anodizer/actions/runs/28882554907)) it read the remote repo and logged `skipped dockerhub PATCH … description / full_description match remote` (summary: `dockerhub … succeeded`) |

## Signing

| Key | Status | Notes |
|---|---|---|
| `signs[]` (cosign) | ✅ Verified | [cfgd v0.3.5 cosign bundle](https://github.com/tj-smith47/cfgd/releases/download/v0.3.5/cfgd-0.3.5-checksums.txt.cosign.bundle). Cosign keyless for binaries and checksums. brontes signs its source tarball keyless (Fulcio OIDC, no key secret): [`brontes-0.2.1-source.tar.gz.sig`](https://github.com/tj-smith47/brontes/releases/download/v0.2.1/brontes-0.2.1-source.tar.gz.sig) via [brontes `.anodizer.yaml`](https://github.com/tj-smith47/brontes/blob/master/.anodizer.yaml) (`signs[].if: "{{ not IsSnapshot }}"`) |
| `signs[]` (gpg) | ✅ Verified | [`anodizer-0.1.1-checksums.txt.sig`](https://github.com/tj-smith47/anodizer/releases/download/v0.1.1/anodizer-0.1.1-checksums.txt.sig) |
| `signs[].artifacts` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`signs:` declares `artifacts: checksum` and `artifacts: source` slots). The `artifacts: source` selector is proven live by brontes: [`brontes-0.2.1-source.tar.gz.sig`](https://github.com/tj-smith47/brontes/releases/download/v0.2.1/brontes-0.2.1-source.tar.gz.sig) |
| `docker_signs[]` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`docker_signs:` with cosign over `artifacts: manifests`) |
| `binary_signs[]` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`binary_signs:` block with cosign sign-blob) |
| Cosign TUF warm-up lock + warm-cache fast path | ✅ Verified (tests) | [`crates/stage-sign/src/tuf_cache.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-sign/src/tuf_cache.rs) — keyless cosign's first run initializes its sigstore TUF trust root under a host-level advisory flock so parallel cold starts don't race; a warm cache (go-tuf or sigstore-go layout) skips the serialized first sign entirely. Validated locally by racing 8 parallel keyless `sign-blob` invocations on a warm cache; awaits a live multi-shard release for public proof |
| `sboms[]` | ✅ Verified | CycloneDX via syft. [`anodizer-0.1.1.cdx.json`](https://github.com/tj-smith47/anodizer/releases/download/v0.1.1/anodizer-0.1.1.cdx.json). `sboms[].artifacts: source` (SBOM over the source tarball instead of build outputs) is proven live by brontes: [`brontes-0.2.1-source.tar.gz.cdx.json`](https://github.com/tj-smith47/brontes/releases/download/v0.2.1/brontes-0.2.1-source.tar.gz.cdx.json) |
| `${artifact}` / `${document}` substitution | ✅ Verified | [`crates/stage-sbom/src/lib.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-sbom/src/lib.rs) (`$artifact`, `$artifactID`, `$document`, `$document<N>` substitution) |
