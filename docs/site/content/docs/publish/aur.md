+++
title = "AUR"
description = "Publish to the Arch User Repository"
weight = 7
template = "docs.html"
+++

Anodizer generates Arch Linux `PKGBUILD` and `.SRCINFO` files and pushes them to the [Arch User Repository](https://aur.archlinux.org/) via SSH. Two package types are supported:

- **Binary packages** (`-bin`) install prebuilt binaries from your release archives. Configured via `publish.aur`.
- **Source packages** build from source using `cargo build`. Configured via `publish.aur_source` (per-crate) or the top-level `aur_sources` array (project-wide).

## Classification

| Package type | Group | Required (default) | Rollback | Token scope |
|---|---|---|---|---|
| our-AUR-repos (binary / source) | Manager | false | git revert + push | `AUR_SSH_KEY write` |
| upstream-AUR (force-push) | Submitter | false | none | `AUR_SSH_KEY write` |

See [Release resilience](../advanced/release-resilience.md) for the full classification table and the Submitter gate semantics.

## Authentication

AUR publishing requires SSH access to the AUR git server. See [SSH key setup](#ssh-key-setup) below for setup instructions.

| Config field | Env var | Description |
|---|---|---|
| `private_key` | — | Path to SSH private key file |
| `git_ssh_command` | — | Override the full SSH invocation |
| — | `AUR_SSH_KEY` | SSH key content (used via `private_key: "{{ .Env.AUR_SSH_KEY }}"`) |

## Common gotchas

- **Version string hyphens**: AUR `pkgver` does not allow hyphens. Anodizer replaces hyphens with underscores (e.g. `1.0.0-rc1` → `1.0.0_rc1`). Ensure your PKGBUILD validators account for this.
- **`.SRCINFO` generated automatically**: anodizer generates `.SRCINFO` alongside the PKGBUILD without running `makepkg --printsrcinfo`. If you maintain additional AUR metadata outside anodizer, ensure the committed `.SRCINFO` stays in sync.
- **No `git_url`**: if `git_url` is omitted for source packages, PKGBUILD files are generated in `dist/` but not pushed. Useful for local inspection before AUR submission.
- **Force-push upstream AUR**: upstream AUR push is a force-push that overwrites the branch; it is classified as Submitter because it cannot be rolled back programmatically.

## Republish / update behavior

Not applicable as a config flag — AUR publishing is always a push (binary packages) or force-push (upstream AUR). Re-cutting a version overwrites the previous PKGBUILD commit. For our-AUR-repos (Manager group), rollback reverts the commit via `git revert` + push.

## Full config reference

### Binary package (`publish.aur`)

```yaml
crates:
  - name: myapp
    publish:
      aur:
        git_url: "ssh://aur@aur.archlinux.org/myapp-bin.git"  # required
        name: myapp-bin              # optional; default: <crate>-bin
        description: ""              # optional
        homepage: ""                 # optional
        license: ""                  # optional; SPDX identifier
        depends: []                  # optional; runtime deps
        optdepends: []               # optional
        conflicts: []                # optional; default: [<base_name>]
        provides: []                 # optional; default: [<base_name>]
        replaces: []                 # optional
        backup: []                   # optional; config files to preserve
        maintainers: []              # optional
        contributors: []             # optional
        rel: "1"                     # optional; pkgrel
        ids: []                      # optional; filter by build IDs
        amd64_variant: v1            # optional; v1 | v2 | v3 | v4
        url_template: ""             # optional; override download URL
        package: ""                  # optional; custom package() body
        install: ""                  # optional; .install file content
        directory: ""                # optional; subdirectory in AUR repo
        private_key: ""              # optional; SSH key path (template)
        git_ssh_command: ""          # optional; custom SSH invocation
        commit_author:
          name: ""
          email: ""
        commit_msg_template: ""      # optional
        skip_upload: false           # optional; true | false | "auto"
        disable: false               # optional
```

### Source package (`publish.aur_source`)

```yaml
crates:
  - name: myapp
    publish:
      aur_source:
        git_url: "ssh://aur@aur.archlinux.org/myapp.git"  # optional; write to dist/ if omitted
        name: myapp                  # optional; default: crate name
        description: ""              # optional
        homepage: ""                 # optional
        license: MIT                 # optional
        maintainers: []              # optional
        contributors: []             # optional
        provides: []                 # optional
        conflicts: []                # optional
        depends: []                  # optional
        optdepends: []               # optional
        makedepends: ["rust", "cargo"]  # optional
        backup: []                   # optional
        rel: "1"                     # optional
        prepare: ""                  # optional; custom prepare() body
        build: ""                    # optional; default: cargo build --release --locked
        package: ""                  # optional; custom package() body
        url_template: ""             # optional; source archive URL
        git_ssh_command: ""          # optional
        private_key: ""              # optional
        directory: ""                # optional
        skip_upload: false           # optional; "auto" skips prereleases
        commit_author:
          name: ""
          email: ""
        commit_msg_template: ""      # optional
        arches: []                   # optional; architecture filter
        disable: false               # optional
```

## Binary packages (`publish.aur`)

Binary packages download a prebuilt archive from your release and install the binary. The package name defaults to `<crate>-bin` following AUR convention.

### Minimal config

```yaml
crates:
  - name: myapp
    publish:
      aur:
        git_url: "ssh://aur@aur.archlinux.org/myapp-bin.git"
```

### Config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | `<crate>-bin` | Override the AUR package name |
| `git_url` | string | **required** | AUR SSH git URL (e.g. `ssh://aur@aur.archlinux.org/<pkg>.git`) |
| `description` | string | crate name | Short description for `pkgdesc` |
| `homepage` | string | none | Project homepage URL. Falls back to `url`, then `release.github` owner/name |
| `url` | string | none | Legacy alias for `homepage` |
| `license` | string | none | SPDX license identifier (e.g. `MIT`, `Apache-2.0`) |
| `depends` | string[] | `[]` | Runtime dependencies (e.g. `glibc`) |
| `optdepends` | string[] | `[]` | Optional dependencies with descriptions (e.g. `fzf: fuzzy finder support`) |
| `conflicts` | string[] | `[<base_name>]` | Packages this conflicts with. Defaults to the package name without `-bin` |
| `provides` | string[] | `[<base_name>]` | Virtual packages provided. Defaults to the package name without `-bin` |
| `replaces` | string[] | `[]` | Packages this replaces (for upgrade paths from old names) |
| `backup` | string[] | `[]` | Config files to preserve on upgrade (paths relative to `/`) |
| `maintainers` | string[] | `[]` | Maintainer lines in PKGBUILD (e.g. `Name <email>`) |
| `contributors` | string[] | `[]` | Contributor comment lines in PKGBUILD |
| `rel` | string | `"1"` | Package release number (`pkgrel`) |
| `ids` | string[] | all | Build IDs filter: only include artifacts whose `id` is in this list |
| `amd64_variant` | string | `"v1"` | amd64 microarchitecture variant filter (`v1`, `v2`, `v3`, `v4`) |
| `url_template` | string | release URL | Custom download URL template (overrides artifact URLs) |
| `package` | string | auto | Custom `package()` function body for PKGBUILD |
| `install` | string | none | Content for a `.install` file (post-install/pre-remove hooks) |
| `directory` | string | repo root | Subdirectory in the AUR git repo for committed files. Supports templates |
| `private_key` | string | none | Path to SSH private key file |
| `git_ssh_command` | string | none | Custom SSH command for git operations |
| `commit_author` | object | none | Override commit author (`name`, `email`) |
| `commit_msg_template` | string | `Update to <version>` | Custom commit message template |
| `skip_upload` | bool/string | `false` | Skip publishing. `true` always skips; `"auto"` skips for prereleases |
| `disable` | bool/string | `false` | Disable this config entirely. Accepts a template string for conditional disable |

### Full example

```yaml
crates:
  - name: myapp
    publish:
      aur:
        git_url: "ssh://aur@aur.archlinux.org/myapp-bin.git"
        name: myapp-bin
        description: "A fast CLI tool"
        homepage: "https://github.com/myorg/myapp"
        license: MIT
        depends:
          - glibc
        optdepends:
          - "bash-completion: shell completions"
        maintainers:
          - "Jane Doe <jane@example.com>"
        private_key: "{{ .Env.AUR_SSH_KEY }}"
        skip_upload: auto
```

### Generated PKGBUILD

Anodizer generates a PKGBUILD with:
- Per-architecture source arrays (`source_x86_64`, `source_aarch64`, etc.) with SHA-256 checksums
- Automatic architecture detection from your Linux build artifacts (`x86_64`, `aarch64`, `i686`, `armv7h`)
- Version strings with hyphens replaced by underscores (AUR `pkgver` requirement)
- Download URLs with the version substituted as `${pkgver}` for AUR compatibility
- A `.SRCINFO` file generated alongside the PKGBUILD (no `makepkg --printsrcinfo` dependency)

The default `package()` function installs the binary to `/usr/bin`:

```bash
package() {
    install -Dm755 "$srcdir/myapp" "$pkgdir/usr/bin/myapp"
}
```

Override this with the `package` field when your archive has a different structure.

## Source packages (`publish.aur_source`)

Source packages download a source tarball and build from source using `cargo build`. The package name does **not** have a `-bin` suffix.

### Minimal config

```yaml
crates:
  - name: myapp
    publish:
      aur_source:
        git_url: "ssh://aur@aur.archlinux.org/myapp.git"
```

### Config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | crate name | Override the AUR package name (no `-bin` suffix) |
| `git_url` | string | none | AUR SSH git URL. If omitted, files are written to `dist/` but not pushed |
| `description` | string | crate name | Short description for `pkgdesc` |
| `homepage` | string | none | Project homepage URL |
| `license` | string | `MIT` | SPDX license identifier |
| `depends` | string[] | `[]` | Runtime dependencies |
| `makedepends` | string[] | `["rust", "cargo"]` | Build-time dependencies |
| `optdepends` | string[] | `[]` | Optional dependencies |
| `conflicts` | string[] | `[<name>-bin]` | Packages this conflicts with. Defaults to `<name>-bin` |
| `provides` | string[] | `[<name>]` | Virtual packages provided |
| `backup` | string[] | `[]` | Config files to preserve on upgrade |
| `maintainers` | string[] | `[]` | Maintainer lines in PKGBUILD |
| `contributors` | string[] | `[]` | Contributor comment lines |
| `rel` | string | `"1"` | Package release number |
| `url_template` | string | GitHub archive URL | Custom source tarball URL template |
| `prepare` | string | none | Custom `prepare()` function body |
| `build` | string | `cargo build --release --locked` | Custom `build()` function body |
| `package` | string | `install -Dm755 ...` | Custom `package()` function body |
| `directory` | string | repo root | Subdirectory in the AUR git repo. Supports templates |
| `private_key` | string | none | Path to SSH private key file |
| `git_ssh_command` | string | none | Custom SSH command |
| `commit_author` | object | none | Override commit author |
| `commit_msg_template` | string | `Update to <version>` | Custom commit message template |
| `ids` | string[] | all | Build IDs filter |
| `arches` | string[] | auto | Explicit architecture list |
| `skip_upload` | bool/string | `false` | Skip publishing. `true` always skips; `"auto"` skips for prereleases |
| `disable` | bool/string | `false` | Disable this config entirely |

### Full example

```yaml
crates:
  - name: myapp
    publish:
      aur_source:
        git_url: "ssh://aur@aur.archlinux.org/myapp.git"
        name: myapp
        description: "A fast CLI tool"
        homepage: "https://github.com/myorg/myapp"
        license: MIT
        depends:
          - glibc
        makedepends:
          - rust
          - cargo
        maintainers:
          - "Jane Doe <jane@example.com>"
        private_key: "{{ .Env.AUR_SSH_KEY }}"
        skip_upload: auto
```

### Generated source PKGBUILD

The default source PKGBUILD downloads the GitHub release source tarball and builds with Cargo:

```bash
build() {
  cd "$srcdir/myapp-$pkgver"
  cargo build --release --locked
}

package() {
  cd "$srcdir/myapp-$pkgver"
  install -Dm755 "target/release/myapp" "$pkgdir/usr/bin/myapp"
}
```

If no `url_template` is set, the source URL defaults to:

```
https://github.com/<owner>/<project>/archive/refs/tags/<tag>.tar.gz
```

The owner and project are derived from `GitURL` and `ProjectName` template variables.

## Top-level `aur_sources`

For project-wide source packages not tied to a specific crate, use the top-level `aur_sources` array. This follows the same schema as `publish.aur_source` and accepts multiple entries:

```yaml
aur_sources:
  - name: myapp
    git_url: "ssh://aur@aur.archlinux.org/myapp.git"
    description: "A fast CLI tool"
    license: MIT
    makedepends:
      - rust
      - cargo
  - name: myapp-git
    git_url: "ssh://aur@aur.archlinux.org/myapp-git.git"
    description: "A fast CLI tool (git version)"
    license: MIT
```

Each entry generates its own PKGBUILD and .SRCINFO, written to `dist/aur_source/<name>/` and optionally pushed to the configured AUR git URL.

## SSH key setup

AUR publishing requires SSH access. To configure this:

1. **Generate an SSH key** (if you do not already have one for AUR):
   ```bash
   ssh-keygen -t ed25519 -f ~/.ssh/aur -N ""
   ```

2. **Add the public key to your AUR account** at [https://aur.archlinux.org/account](https://aur.archlinux.org/account) under "SSH Public Key".

3. **Reference the private key** in your config:
   ```yaml
   publish:
     aur:
       git_url: "ssh://aur@aur.archlinux.org/myapp-bin.git"
       private_key: "{{ .Env.AUR_SSH_KEY }}"
   ```

4. **In CI**, store the private key as a secret and expose it as an environment variable:
   ```yaml
   # GitHub Actions example
   env:
     AUR_SSH_KEY: ${{ secrets.AUR_SSH_KEY }}
   ```

Alternatively, use `git_ssh_command` for full control over the SSH invocation:

```yaml
publish:
  aur:
    git_url: "ssh://aur@aur.archlinux.org/myapp-bin.git"
    git_ssh_command: "ssh -i /path/to/key -o StrictHostKeyChecking=no"
```

If neither `private_key` nor `git_ssh_command` is set, Anodizer uses the system default SSH configuration (e.g. `~/.ssh/config`, ssh-agent).

## `skip_upload` behavior

The `skip_upload` field controls whether Anodizer pushes to the AUR git repo:

| Value | Behavior |
|-------|----------|
| `false` (default) | Always publish |
| `true` | Never publish (PKGBUILD is still generated locally) |
| `"auto"` | Skip publishing for prerelease versions (e.g. `1.0.0-rc1`) |
| template string | Evaluated as a template; skips when the result is `"true"` |

The `disable` field works similarly but skips the entire stage, including PKGBUILD generation.
