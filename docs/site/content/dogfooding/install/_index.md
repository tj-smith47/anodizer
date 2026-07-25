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
(snapshot 2026-07-25). Both anodizer's and cfgd's dogfood configs migrated off
the deprecated `publish.homebrew:` Formula path on 2026-05-26 per
[GoReleaser v2.16](https://goreleaser.com/blog/goreleaser-v2.16/); the
[`homebrew_casks:` docs](../../../docs/publish/homebrew-casks/) cover the
migration guide.

```yaml
# Casks live in the tap's Casks/ directory; description and homepage are derived
# from metadata + Cargo, and the cask DSL has no license stanza.
homebrew_casks:
  - required: true
    repository:
      owner: tj-smith47
      name: homebrew-tap
    directory: Casks
    livecheck:
      strategy: github_latest
      url: homepage
      skip: false
    binaries:
      - cfgd
    generate_completions_from_executable:
      executable: cfgd
      args:
        - completion
      base_name: cfgd
    manpages:
      - man/man1/cfgd.1
    caveats: |
      Run `cfgd init` to scaffold a config in your repo.
    dependencies:
      - formula: git
    commit_msg_template: "cfgd {{ .Tag }}"
    commit_author:
      name: "TJ Smith"
      email: "tj@jarvispro.io"
```

| Channel | Status | Verify |
|---|---|---|
| **GitHub Releases** | ✅ Verified | [anodizer v0.23.0](https://github.com/tj-smith47/anodizer/releases/tag/v0.23.0) · [cfgd v0.6.1](https://github.com/tj-smith47/cfgd/releases/tag/v0.6.1) · [brontes v0.3.0](https://github.com/tj-smith47/brontes/releases/tag/v0.3.0) |
| **`curl \| sh` install script** | ✅ Verified | [`install.sh`](https://github.com/tj-smith47/anodizer/releases/download/v0.23.0/install.sh) ships as a release asset on every anodizer release, rendered by `install_scripts:` — the os/arch→asset table, the checksums filename, and the tag prefix are all derived from the configured targets, so its download URLs cannot drift from the archive stage's own asset names |
| **crates.io** | ✅ Verified | [crates.io/crates/anodizer](https://crates.io/crates/anodizer) (0.23.0) · [crates.io/crates/cfgd](https://crates.io/crates/cfgd) · [crates.io/crates/brontes](https://crates.io/crates/brontes) (single-crate, library-only publish) |
| **PyPI** | ✅ Verified | [pypi.org/project/anodizer](https://pypi.org/project/anodizer/) — `pip install anodizer` / `uvx anodizer` resolves 0.23.0 from 9 per-platform wheels (macOS x86_64/arm64/universal2, manylinux + musllinux x86_64/aarch64, Windows amd64/arm64). Published with `auth: oidc` Trusted Publishing, no stored API token. See [`pypis:` docs](../../../docs/publish/pypi/) |
| **npm** | ✅ Verified | [npmjs.com/package/anodizer](https://www.npmjs.com/package/anodizer) — the live metapackage (`npm install anodizer`) declares all 8 per-platform binary packages as `optionalDependencies` (`@tj-smith47/anodizer-{darwin,linux,win32}-*`, each published with provenance). See [`npms:` docs](../../../docs/publish/npm/) |
| **Snap Store** | ⏳ Pending | [snapcraft.io/anodizer](https://snapcraft.io/anodizer) serves 0.9.1 (uploaded 2026-06-13) and [snapcraft.io/cfgd](https://snapcraft.io/cfgd) serves 0.3.5 — the newest revisions either project ever landed. Both `snapcrafts:` blocks now carry `publish: false`: each tool execs host binaries that `strict` confinement blocks, and the Snap Store denied both `classic` requests. Uploads resume if a grant lands |
| **Chocolatey** | ⏳ Pending | [community.chocolatey.org/packages/anodizer](https://community.chocolatey.org/packages/anodizer) — every release submits, but the community feed publishes only after human moderation, so the newest *approved* version (0.9.0) trails the newest *submitted* one. [cfgd](https://community.chocolatey.org/packages/cfgd) sits at 0.4.0 for the same reason |
| **winget** | ✅ Verified | 33 anodizer manifest versions merged into [microsoft/winget-pkgs · TJSmith/Anodizer](https://github.com/microsoft/winget-pkgs/tree/master/manifests/t/TJSmith/Anodizer) (through 0.22.2) · [TJSmith/cfgd/0.6.1](https://github.com/microsoft/winget-pkgs/tree/master/manifests/t/TJSmith/cfgd/0.6.1) |
| **GHCR container images** | ✅ Verified | [ghcr.io/tj-smith47/anodizer](https://github.com/tj-smith47/anodizer/pkgs/container/anodizer) carries 34 semver tags plus `latest`, each accompanied by a cosign `.sig` tag (37 in total) — pushed by `dockers_v2:` and signed by `docker_signs:` on every release · [cfgd's images](https://github.com/tj-smith47?tab=packages&repo_name=cfgd) (`cfgd`, `cfgd-operator`, `cfgd-csi`) |
| **Nix flake** | ✅ Verified | [tj-smith47/nix-pkgs](https://github.com/tj-smith47/nix-pkgs) |
| **Scoop bucket** | ✅ Verified | [`bucket/anodizer.json`](https://github.com/tj-smith47/scoop-bucket/blob/master/bucket/anodizer.json), [`bucket/cfgd.json`](https://github.com/tj-smith47/scoop-bucket/blob/master/bucket/cfgd.json) |
| **Homebrew cask** | ✅ Verified | [`tj-smith47/homebrew-tap` Casks/anodizer.rb](https://github.com/tj-smith47/homebrew-tap/blob/master/Casks/anodizer.rb) (`cask "anodizer"`, version tracks the latest release) — the `homebrew_casks:` block writes the cask on every release (GR v2.16 supported path for plain CLI binaries). See [`homebrew_casks:` docs](../../../docs/publish/homebrew-casks/) and [GoReleaser v2.16](https://goreleaser.com/blog/goreleaser-v2.16/). |
| **Krew** | ✅ Verified | cfgd plugin lives in [kubernetes-sigs/krew-index `plugins/cfgd.yaml`](https://github.com/kubernetes-sigs/krew-index/blob/master/plugins/cfgd.yaml) — merged via [PR #5633](https://github.com/kubernetes-sigs/krew-index/pull/5633) (v0.3.5) and [PR #5815](https://github.com/kubernetes-sigs/krew-index/pull/5815) (v0.4.0); the index now serves v0.6.1 |
| **AUR** | ✅ Verified | Both AUR publishers run on every anodizer release: [`anodizer-bin`](https://aur.archlinux.org/packages/anodizer-bin) via `aur:` (prebuilt binary) and [`anodizer`](https://aur.archlinux.org/packages/anodizer) via `aur_sources:` (cargo build-from-source PKGBUILD), both at 0.23.0-1 (AUR RPC `v5/info`). cfgd ships `cfgd` via `aur_sources:`. See [`aur:` docs](../../../docs/publish/aur/). |
| **SchemaStore catalog** | ✅ Verified | The `Anodizer` entry is live in the public [schemastore.org catalog](https://www.schemastore.org/api/json/catalog.json) (`fileMatch: [".anodizer.yaml", ".anodizer.yml"]`), so any SchemaStore-backed editor resolves completions for a config file with no setup. `schemastore:` writes the entry through the [tj-smith47/schemastore](https://github.com/tj-smith47/schemastore) fork and reconciles it each release against the served [schema.json](https://tj-smith47.github.io/anodizer/schema.json) |
| **MCP registry** | ✅ Verified | [registry.modelcontextprotocol.io](https://registry.modelcontextprotocol.io/v0/servers?search=anodizer) holds 34 published `io.github.tj-smith47/anodizer` versions, 0.6.0 through 0.23.0, each pointing at the matching `ghcr.io/tj-smith47/anodizer` OCI image. Published via GitHub OIDC, no stored registry token |
| **Flathub** | 🤝 Help wanted | Needs flatpak runtime + flathub config |
