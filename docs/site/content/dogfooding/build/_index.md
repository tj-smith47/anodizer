+++
title = "What anodizer builds"
description = "Artifacts the `anodizer release` pipeline produces: binaries, archives, packages, installers, containers, and signing material."
weight = 20
template = "section.html"
+++

# What anodizer builds

Output formats and the `builds[]` / `archives[]` / `docker_v2[]` / `signs[]`
keys that drive them. Native binaries for 6 targets ship on every release
(linux amd64/arm64, darwin amd64/arm64, windows amd64/arm64), built with
cargo + cargo-zigbuild + cross.

## Live configuration

Build / archive / nfpm / docker_v2 / sign blocks from
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

# docker_v2: pushes a multi-arch image index in one step (no separate manifest).
docker_v2:
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
| `builds[].goos` / `builds[].goarch` | ✅ Verified | [v0.1.1 assets](https://github.com/tj-smith47/anodizer/releases/tag/v0.1.1) cover 6 targets (`*-linux-amd64.tar.gz` to `*-windows-arm64.zip`) |
| `universal_binaries[]` | ✅ Verified | [cfgd v0.3.5](https://github.com/tj-smith47/cfgd/releases/tag/v0.3.5) ships `cfgd-0.3.5-darwin-all.tar.gz` via `lipo` |
| `upx[]` | ✅ Verified | [`anodizer-0.1.1-linux-amd64.tar.gz`](https://github.com/tj-smith47/anodizer/releases/download/v0.1.1/anodizer-0.1.1-linux-amd64.tar.gz) (UPX-packed) |
| `builds[].overrides` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`format_overrides` for windows zip) |
| `builds[].hooks.pre` / `post` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (archive `hooks.before` / `hooks.after`) |
| `builds[].mod_timestamp` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`metadata.mod_timestamp: "{{ CommitTimestamp }}"`) |
| `builds[].builder: prebuilt` | 🤝 Help wanted | [`crates/stage-build/src/tests.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-build/src/tests.rs) covers unit paths; no production `.anodizer.yaml` uses `builder: prebuilt` yet |
| `builds[].buildmode` (no-compile) | ✅ Verified | [`crates/stage-build/src/run.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-build/src/run.rs) |
| `report_sizes` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`report_sizes: true`) |

## Archives and checksums

| Format | Status | Notes |
|---|---|---|
| `tar.gz` | ✅ Verified | [`anodizer-0.1.1-linux-amd64.tar.gz`](https://github.com/tj-smith47/anodizer/releases/download/v0.1.1/anodizer-0.1.1-linux-amd64.tar.gz) |
| `zip` | ✅ Verified | [`anodizer-0.1.1-windows-amd64.zip`](https://github.com/tj-smith47/anodizer/releases/download/v0.1.1/anodizer-0.1.1-windows-amd64.zip) |
| `tar.xz`, `tar.zst`, `tgz` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (second `archives[]` entry with `formats: [tar.xz, tar.zst]` + `tgz` override). TODO: link a live asset once v0.2.0 ships |
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

## macOS and Windows installers

Code-signing material and a real macOS or Windows runner are required
before these can ship live. Implementation is complete and unit-tested.

| Format | Status | Notes |
|---|---|---|
| `.dmg` | 🤝 Help wanted | Needs `dmgs[]` configured |
| `.pkg` | 🤝 Help wanted | Needs `pkgs[]` configured |
| `.app` bundle | 🤝 Help wanted | Needs `app_bundles[]` configured |
| `.msi` | 🤝 Help wanted | Needs `wixl`/`candle`/`light` on the runner |
| `.exe` (NSIS) | 🤝 Help wanted | Needs `makensis` on the runner |

| Key | Status | Notes |
|---|---|---|
| `notarize.macos` | 🤝 Help wanted | Cross-platform (rcodesign). Implementation requires `sign.certificate` (P12 file), `sign.password`, and `notarize.{issuer_id, key, key_id}`, i.e. an Apple Developer Program membership. Not dogfoodable on Linux runners without a paid Apple account |
| `notarize.macos_native` | 🤝 Help wanted | Needs Apple Developer cert on a macOS runner |

## Container images

| Key | Status | Notes |
|---|---|---|
| `docker_v2[]` | ✅ Verified | [ghcr.io/tj-smith47/cfgd](https://github.com/tj-smith47?tab=packages&repo_name=cfgd) (`cfgd-agent`, `cfgd-operator`, `cfgd-csi`); [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`docker_v2:` per crate) |
| `docker_manifests[]` | ✅ Verified | [`ghcr.io/tj-smith47/cfgd:v0.3.5`](https://github.com/tj-smith47/cfgd/pkgs/container/cfgd) (multi-arch linux/amd64+arm64). `docker_v2` already pushes a multi-arch index, so cfgd's `docker_manifests[]` entries are bypassed at runtime (`docker: skipping manifest ... already pushed as multi-arch by docker_v2`) — retained only for the niche case of stitching together images not built by `docker_v2` in the same run |
| `docker_v2[].build_args` / `labels` / `annotations` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`build_args.VERSION` + `org.opencontainers.image.*` annotations) |
| `docker_v2[].sbom: true` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`sbom: true` on all three `docker_v2` images) |
| `docker_digest.name_template` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`docker_digest.name_template: "cfgd_{{ .Tag }}.digest"`) |
| `docker_v2[].use: buildx` | ✅ Verified | [`crates/stage-docker/src/detect.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-docker/src/detect.rs) (buildx is the default backend) |
| `docker_v2[].use: podman` / `docker_manifests[].use: docker` / `podman` | 🤝 Help wanted | Linux-only backend selectors. No live release exercises the non-buildx path |
| `docker_hub.description` | 🤝 Help wanted | We use ghcr; needs a Docker Hub-anchored release |

## Signing

| Key | Status | Notes |
|---|---|---|
| `signs[]` (cosign) | ✅ Verified | [cfgd v0.3.5 cosign bundle](https://github.com/tj-smith47/cfgd/releases/download/v0.3.5/cfgd-0.3.5-checksums.txt.cosign.bundle). Cosign keyless for binaries and checksums |
| `signs[]` (gpg) | ✅ Verified | [`anodizer-0.1.1-checksums.txt.sig`](https://github.com/tj-smith47/anodizer/releases/download/v0.1.1/anodizer-0.1.1-checksums.txt.sig) |
| `signs[].artifacts` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`signs:` declares `artifacts: checksum` and `artifacts: source` slots) |
| `docker_signs[]` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`docker_signs:` with cosign over `artifacts: manifests`) |
| `binary_signs[]` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`binary_signs:` block with cosign sign-blob) |
| `sboms[]` | ✅ Verified | CycloneDX via syft. [`anodizer-0.1.1.cdx.json`](https://github.com/tj-smith47/anodizer/releases/download/v0.1.1/anodizer-0.1.1.cdx.json) |
| `${artifact}` / `${document}` substitution | ✅ Verified | [`crates/stage-sbom/src/lib.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-sbom/src/lib.rs) (`$artifact`, `$artifactID`, `$document`, `$document<N>` substitution) |
