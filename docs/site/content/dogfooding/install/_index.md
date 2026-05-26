+++
title = "Where you install it"
description = "Distribution channels users get the anodize and cfgd binaries from, with a link to each live registry entry."
weight = 10
template = "section.html"
+++

# Where you install it

Distribution channels users get the binary from. Each row links to the live
registry page or release asset.

## Live configuration

Per-crate `publish:` block and top-level `homebrew_casks:` block from
[`cfgd/.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml)
(snapshot 2026-05-26) — every channel in the table below is driven from
this config. Migrated to [`homebrew_casks:`](../../../docs/publish/homebrew-casks/)
on 2026-05-26 per [GoReleaser v2.16](https://goreleaser.com/blog/goreleaser-v2.16/).

```yaml
publish:
  # cargo: inherits index_timeout: 600 from defaults.publish.cargo

  # Deprecated upstream in GoReleaser v2.16 — kept here through anodizer's
  # deprecation lifecycle.
  homebrew:
    required: true
    repository:
      owner: tj-smith47
      name: homebrew-tap
    directory: Formula
    description: "Declarative, GitOps-style machine configuration management"
    homepage: "https://github.com/tj-james47/cfgd"
    license: MIT
    install: |
      bin.install "cfgd"
    post_install: |
      generate_completions_from_executable(bin/"cfgd", "completion")
    test: |
      system "#{bin}/cfgd", "--version"
    caveats: |
      Run `cfgd init` to scaffold a config in your repo.
    dependencies:
      - name: git
        type: required
    commit_msg_template: "cfgd {{ .Tag }}"
    commit_author:
      name: "TJ Smith"
      email: "tj@jarvispro.io"

  scoop:
    repository:
      owner: tj-smith47
      name: scoop-bucket
    depends: [git]
    shortcuts: [["cfgd.exe", "cfgd"]]

  chocolatey:
    repository:
      owner: tj-smith47
      name: cfgd
    authors: "TJ Smith"
    license: MIT
    require_license_acceptance: false
    project_url: "https://github.com/tj-james47/cfgd"
    icon_url: "https://raw.githubusercontent.com/tj-james47/cfgd/main/.github/gear.svg"
    tags: [configuration, gitops, reconciliation, rust]

  winget:
    repository:
      owner: tj-smith47
      name: winget-pkgs
    package_identifier: "TJSmith.cfgd"
    publisher: "TJ Smith"

  krew:
    repository:
      owner: tj-smith47
      name: krew-index
    short_description: "kubectl plugin for cfgd"

  nix:
    repository:
      owner: tj-smith47
      name: nix-pkgs

# Top-level — GR v2.16 supported Homebrew Cask path.
# publish.homebrew: above is kept for backward-compat during the deprecation
# lifecycle; homebrew_casks: is the authoritative config going forward.
homebrew_casks:
  - required: true
    repository:
      owner: tj-smith47
      name: homebrew-tap
    directory: Formula
    description: "Declarative, GitOps-style machine configuration management"
    homepage: "https://github.com/tj-james47/cfgd"
    license: MIT
    binaries:
      - cfgd
    generate_completions_from_executable:
      executable: cfgd
      args:
        - completion
      base_name: cfgd
    caveats: |
      Run `cfgd init` to scaffold a config in your repo.
    dependencies:
      - name: git
        type: required
    commit_msg_template: "cfgd {{ .Tag }}"
    commit_author:
      name: "TJ Smith"
      email: "tj@jarvispro.io"

# Top-level — snap, GHCR images, source archive, makeselfs:
snapcrafts:
  - name: cfgd
    grade: stable
    confinement: classic
dockers:
  - image_templates: ["ghcr.io/tj-james47/cfgd:{{ .Tag }}"]
```

| Channel | Status | Verify |
|---|---|---|
| **GitHub Releases** | ✅ Verified | [anodizer v0.1.1](https://github.com/tj-james47/anodizer/releases/tag/v0.1.1) · [cfgd v0.3.5](https://github.com/tj-james47/cfgd/releases/tag/v0.3.5) |
| **crates.io** | ✅ Verified | [crates.io/crates/anodizer](https://crates.io/crates/anodizer) · [crates.io/crates/cfgd](https://crates.io/crates/cfgd) |
| **Snap Store** | ✅ Verified | [snapcraft.io/anodizer](https://snapcraft.io/anodizer) · [snapcraft.io/cfgd](https://snapcraft.io/cfgd) |
| **Homebrew tap** | ✅ Verified (Formula path — deprecated upstream in GR v2.16; migration target is the Cask row below) | [tj-smith47/homebrew-tap](https://github.com/tj-james47/homebrew-tap/tree/master/Formula) (`anodizer.rb`, `cfgd.rb`) |
| **Chocolatey** | ✅ Verified | [community.chocolatey.org/packages/cfgd](https://community.chocolatey.org/packages/cfgd) |
| **winget** | ✅ Verified | [microsoft/winget-pkgs · TJSmith/cfgd/0.3.5](https://github.com/microsoft/winget-pkgs/tree/master/manifests/t/TJSmith/cfgd/0.3.5) |
| **GHCR container images** | ✅ Verified | [github.com/tj-james47/cfgd/pkgs](https://github.com/tj-james47?tab=packages&repo_name=cfgd) (`cfgd`, `cfgd-operator`, `cfgd-csi`) |
| **Nix flake** | ✅ Verified | [tj-smith47/nix-pkgs](https://github.com/tj-smith47/nix-pkgs) |
| **Scoop bucket** | ✅ Verified | [`anodizer.json`](https://github.com/tj-james47/scoop-bucket/blob/master/anodizer.json), [`cfgd.json`](https://github.com/tj-james47/scoop-bucket/blob/master/cfgd.json) |
| **Krew** | 🤝 Help wanted | PR flow runs in CI; cfgd plugin not yet merged into [kubernetes-sigs/krew-index](https://github.com/kubernetes-sigs/krew-index/tree/master/plugins) |
| **AUR** | 🤝 Help wanted | Needs AUR SSH key; not pushed |
| **Flathub** | 🤝 Help wanted | Needs flatpak runtime + flathub config |
| **Homebrew cask** | 🟡 In progress | Cask block added 2026-05-26 against GR v2.16 alignment; pending the next release to validate the tap-write path. See [`homebrew_casks:` docs](../../../docs/publish/homebrew-casks/) and [GoReleaser v2.16](https://goreleaser.com/blog/goreleaser-v2.16/). |
