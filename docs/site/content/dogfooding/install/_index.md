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

Top-level `homebrew_casks:` block from
[`cfgd/.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml)
(snapshot 2026-05-26). Both anodizer's and cfgd's dogfood configs migrated off
the deprecated `publish.homebrew:` Formula path on 2026-05-26 per
[GoReleaser v2.16](https://goreleaser.com/blog/goreleaser-v2.16/); the
[`homebrew_casks:` docs](../../../docs/publish/homebrew-casks/) cover the
migration guide.

```yaml
# Top-level homebrew_casks block (GR v2.16+ supported path).
# homebrew_casks: is the sole Homebrew publication path; publish.homebrew: has been removed.
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
```

| Channel | Status | Verify |
|---|---|---|
| **GitHub Releases** | ✅ Verified | [anodizer v0.1.1](https://github.com/tj-james47/anodizer/releases/tag/v0.1.1) · [cfgd v0.3.5](https://github.com/tj-james47/cfgd/releases/tag/v0.3.5) |
| **crates.io** | ✅ Verified | [crates.io/crates/anodizer](https://crates.io/crates/anodizer) · [crates.io/crates/cfgd](https://crates.io/crates/cfgd) |
| **Snap Store** | ✅ Verified | [snapcraft.io/anodizer](https://snapcraft.io/anodizer) · [snapcraft.io/cfgd](https://snapcraft.io/cfgd) |
| **Chocolatey** | ✅ Verified | [community.chocolatey.org/packages/cfgd](https://community.chocolatey.org/packages/cfgd) |
| **winget** | ✅ Verified | [microsoft/winget-pkgs · TJSmith/cfgd/0.3.5](https://github.com/microsoft/winget-pkgs/tree/master/manifests/t/TJSmith/cfgd/0.3.5) |
| **GHCR container images** | ✅ Verified | [github.com/tj-james47/cfgd/pkgs](https://github.com/tj-james47?tab=packages&repo_name=cfgd) (`cfgd`, `cfgd-operator`, `cfgd-csi`) |
| **Nix flake** | ✅ Verified | [tj-smith47/nix-pkgs](https://github.com/tj-smith47/nix-pkgs) |
| **Scoop bucket** | ✅ Verified | [`anodizer.json`](https://github.com/tj-james47/scoop-bucket/blob/master/anodizer.json), [`cfgd.json`](https://github.com/tj-smith47/scoop-bucket/blob/master/cfgd.json) |
| **Homebrew cask** | 🟡 In progress | `homebrew_casks:` block added 2026-05-26 (GR v2.16 supported path for plain CLI binaries); pending next release to validate the tap-write end-to-end. See [`homebrew_casks:` docs](../../../docs/publish/homebrew-casks/) and [GoReleaser v2.16](https://goreleaser.com/blog/goreleaser-v2.16/). |
| **Krew** | 🤝 Help wanted | PR flow runs in CI; cfgd plugin not yet merged into [kubernetes-sigs/krew-index](https://github.com/kubernetes-sigs/krew-index/tree/master/plugins) |
| **AUR** | 🟡 In progress | AUR account created + `AUR_SSH_KEY` wired into release CI 2026-06-02. anodizer ships `anodizer-bin` via the `aur` (binary) publisher; cfgd ships `cfgd` via the `aur_source` (build-from-source) publisher. Pending next release to validate the push end-to-end. See [`aur:` docs](../../../docs/publish/aur/). |
| **Flathub** | 🤝 Help wanted | Needs flatpak runtime + flathub config |
