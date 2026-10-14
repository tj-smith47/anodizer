+++
title = "GitHub Releases"
description = "Create GitHub releases with uploaded assets"
weight = 1
template = "docs.html"
+++

The release stage creates a GitHub release and uploads all artifacts as assets.

## Classification

| Group | Required (default) | Rollback | Token scope |
|---|---|---|---|
| Assets | true | delete release + delete assets (tag ref owned by `tag rollback`) | `contents:write` |

See [Release resilience](../advanced/release-resilience.md) for the full classification table and the Submitter gate semantics.

## The `required:` field

Default: **`true`** — a GitHub Releases failure fails the release.

Set `required: false` to log failures but continue:

```yaml
crates:
  - name: myapp
    release:
      required: false   # continue release even if GitHub Release upload fails
      github:
        owner: myorg
        name: myapp
```

See [Publish overview — the `required:` field](../) for the full semantics.

## Release preflight

Before anything is built or published, the preflight asks the forge the release is created on whether this run can create every release the selected crates name. A failed check stops the run by default.

| Forge | Check |
|---|---|
| GitHub | the token can push to the release repository (`contents:write`), and a release that already exists at the tag is not immutable |
| GitLab | the token has Developer access or higher on the project (the higher of its project and group access) |
| Gitea | the token can push to the repository |
| every forge | a token is resolved at all; a repository that does not exist or the token cannot see fails too |

A check that cannot reach a verdict (the forge answers with a 5xx, rate-limits the request, or cannot be reached) is a warning, and [`preflight.strict`](../general/preflight.md) makes it a blocker. On GitLab, a run that authenticates with a CI job token (`gitlab_urls.use_job_token`) skips the access check, since a job token cannot read the project.

An immutable GitHub release cannot be updated and its assets cannot be replaced, so a run releasing onto one would fail at release creation; the preflight reports it before the build instead. A tag with no release yet passes.

`release.preflight.fail_on_error: false` reports the failure as a warning and lets the run continue:

```yaml
crates:
  - name: myapp
    release:
      github:
        owner: myorg
        name: myapp
      preflight:
        fail_on_error: false   # default: true
```

With the default, `anodizer preflight` (and `anodizer release`, which runs the same check) stops:

```text
   • 0 preflight checks passed
   • Reconcile state
   • github-release  absent — will publish
       Error preflight: 1 resilience blocker: github-release: GitHub token cannot push to myorg/myapp (needs contents:write); the release create / asset upload will fail
```

With `fail_on_error: false` the same check prints a warning and the run goes on:

```text
   • 0 preflight checks passed
     Warning github-release: GitHub token cannot push to myorg/myapp (needs contents:write); the release create / asset upload will fail; continuing because release.preflight.fail_on_error is false
   • preflight found 0 publishers clean
   • Reconcile state
   • github-release  absent — will publish
```

If the problem is real, the release then fails when it creates the release. The value:

- is read per crate: a crate's own `release.preflight.fail_on_error` wins over a top-level `release.preflight.fail_on_error`, which applies to every crate that sets none, in a single-crate project, a lockstep workspace and a per-crate workspace alike. An empty value (`fail_on_error: ""`) counts as unset;
- accepts a template that renders to `true`, `false`, `1` or `0` in any letter case, e.g. `fail_on_error: "{{ not IsNightly }}"` to only warn on nightly runs. Any other render, an empty one included (an unset `{{ Env.X }}`), stops the run with an error naming the field and the rendered value, and that error carries the failed check's own message when there is one;
- also covers a check that [`preflight.strict`](../general/preflight.md) turned into a failure.

The token is also an environment requirement of the release: `ANODIZER_GITHUB_TOKEN`, `GITHUB_TOKEN` or `GH_TOKEN` on GitHub, `GITLAB_TOKEN` on GitLab, `GITEA_TOKEN` on Gitea, the forge's token file, or `--token`. A run with no token therefore still stops at the environment check whatever `fail_on_error` says; with `false` the release preflight reports the missing token as a warning beside that failure.

`--quiet` and `--json` keep the warning off stderr; `--json` lists it under `publishers.warnings`.

## Minimal config

```yaml
crates:
  - name: myapp
    release:
      github:
        owner: myorg
        name: myapp
```

If `github.owner` and `github.name` are omitted, anodizer auto-detects them from the `origin` git remote URL. That holds for a block that names only a token, so a token override needs no repeated owner/name:

```yaml
release:
  github:
    token: "{{ .Env.RELEASE_GITHUB_TOKEN }}"   # owner/name come from the origin remote
```

A field written empty (`owner: ""`) counts as omitted and is inferred the same way. The block is filled only from a remote on GitHub's own host: github.com (`ssh.github.com` counts), or the host the top-level `github_urls.api` names, so a GitHub Enterprise remote (`git@github.mycorp.com:acme/widget.git`) is read once `github_urls.api` is `https://github.mycorp.com/api/v3`; a port on the remote or on the API base is ignored when the hosts are compared. A remote on any other host fills nothing, and the refusal names that host and the `github_urls.api` override. The same host rule holds for [`gitlab:`](scm/gitlab.md) and [`gitea:`](scm/gitea.md) blocks; a `gitlab:` block whose `name` is a numeric project id or a full path with an empty `owner` is left as written.

The inference runs in the pipeline commands, in `anodizer check config` and in `anodizer promote`. Under `--snapshot` an unfilled block on any forge is a warning, since nothing publishes. With no `origin` remote to read, a block whose owner or name is still empty is refused before any API URL is built:

```
$ anodizer check config
       Error crate 'myapp': release.github leaves owner and name empty and the `origin` remote could not supply them: set release.github.owner and release.github.name or add an `origin` remote
```

## Release config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `provider` | string | detected | Forge the release goes to: `github`, `gitlab` or `gitea`. See [Choosing the forge](#choosing-the-forge). |
| `github.owner` | string | auto-detected | GitHub owner/org |
| `github.name` | string | auto-detected | Repository name |
| `github.token` | string | pipeline token | Token override for this release repository (templates allowed). Useful when the release repo needs a different token than the build repo — e.g. releasing to another organization. |
| `draft` | bool | `false` | Create as draft release |
| `prerelease` | string/bool | `auto` | Mark as prerelease: `auto` (detect from version), `true` or `1`, `false` or `0` (unquoted or quoted); any other string is a load error |
| `make_latest` | string/bool | `auto` | Mark as latest: `auto`, `true` or `1`, `false` or `0`; a template is rendered and read the same way, in any letter case, and any other non-empty render marks it latest (see [bool-or-template values](@/docs/general/templates.md#bool-or-template-values)) |
| `name_template` | string | `{{ Tag }}` | Release title |
| `header` | string | none | Text prepended to release body |
| `footer` | string | `Released with [anodizer](…) 🦀` | Text appended to release body. REPLACES the default attribution line; set to `""` for no footer |
| `full_changelog_link` | bool | `true` | Append a derived `**Full Changelog**: …/compare/<prev>...<tag>` line below the changelog and above the footer |
| `extra_files` | list | none | Additional files to upload (glob patterns) |
| `skip_upload` | bool or template | `false` | Create release without uploading assets: `true` or `1` skips, `auto` skips on a snapshot, `false`, `0` or empty uploads, any other render is an error; read in any letter case (see [bool-or-template values](@/docs/general/templates.md#bool-or-template-values)) |
| `replace_existing_draft` | bool | `false` | Replace existing draft release. See [Recovery flags](../advanced/recovery-flags.md#release-replace-existing-draft). |
| `use_existing_draft` | bool | `false` | Reuse an existing draft release whose title matches the rendered `name_template` and update it in place. See [Reusing a draft](#draft-reuse). |
| `replace_existing_artifacts` | bool | `false` | Overwrite existing assets. See [Recovery flags](../advanced/recovery-flags.md#release-replace-existing-artifacts). |
| `preflight.fail_on_error` | bool/template | `true` | Stop the run when the release preflight fails; `false` reports it as a warning. See [Release preflight](#release-preflight). |
| `on_failure` | string | `hold` | What a pipeline failure leaves behind. `hold` is the only accepted value — `rollback` is a hard config-validation error. See [Release resilience](../advanced/release-resilience.md#release-on-failure). |

## Full config reference

```yaml
release:
  provider: github            # optional; github | gitlab | gitea (detected when unset)
  github:
    owner: myorg              # auto-detected from git remote if omitted
    name: myapp
    token: "{{ .Env.RELEASE_GITHUB_TOKEN }}"  # optional; overrides the default token
  draft: false
  prerelease: auto            # auto | true | false
  make_latest: auto           # auto | true | false
  name_template: "{{ Tag }}"
  header: ""                  # text prepended to release body
  footer: ""                  # "" = no footer; unset = the default anodizer attribution line
  full_changelog_link: true   # append the derived compare link
  extra_files: []             # glob patterns for additional uploads
  skip_upload: false          # bool or template string
  replace_existing_draft: false
  replace_existing_artifacts: false
  mode: ""                    # keep-existing | append | prepend | replace
  ids: []
  exclude: []                 # drop assets whose name matches a glob
  skip: false
  on_failure: hold             # the only accepted value; also the default
  preflight:
    fail_on_error: true        # false: a failed release preflight only warns
```

## Release body layout

anodizer composes the release body from four pieces. The middle two are always
present; the outer two are configurable:

```
<release.header>

<changelog>

---
**Full Changelog**: https://github.com/myorg/myapp/compare/v1.2.2...v1.2.3
Released with [anodizer](https://github.com/tj-smith47/anodizer) 🦀
```

The `**Full Changelog**` line is derived — from the repository the release
publishes to and the previous tag in this release's tag family. You never write
it. It is omitted on a first release (no previous tag) and when the changelog
already carries one (`changelog.use: github-native`).

The last line is the default footer. A `footer:` you set **replaces** it; it
never stacks:

```yaml
release:
  footer: "Questions? open an issue."   # replaces the attribution line
```

Turn either off independently:

```yaml
release:
  full_changelog_link: false   # no derived compare link
  footer: ""                   # no footer at all
```

## Sidecar exclusion with `exclude`

`exclude` is a list of globs matched against each release asset's **file
name**; anodizer drops every asset whose name matches at least one glob before
attaching it to **this GitHub release only** (a mirror configured elsewhere is
unaffected). Use it to keep heavy sidecars (checksums, signatures, SBOMs) off
the GitHub release while archives still attach.

```yaml
release:
  github: { owner: my-org, name: my-repo }
  exclude:
    - "*.sha256"
    - "*.sig"
    - "*.cdx.json"
```

`exclude` composes with `ids:` — an asset attaches only when it passes both
filters. An empty or unset `exclude` keeps everything. Globs are validated at
config-load; an `exclude` that drops every candidate raises a warning so a
typo'd glob is never a silent empty release. The `verify-release` gate applies
the same `exclude`, so a deliberately-excluded signature or SBOM is not flagged
as a missing asset.

## Authentication

Set `GITHUB_TOKEN` as an environment variable or pass it via `--token`:

```bash
export GITHUB_TOKEN="ghp_..."
anodizer release
```

On GitHub the token is read from `--token`, then `ANODIZER_GITHUB_TOKEN`,
`GITHUB_TOKEN` and `GH_TOKEN`, the first one set and not blank, then the
GitHub token file. On GitLab it is `GITLAB_TOKEN`, then the GitLab token file,
and on Gitea `GITEA_TOKEN`, then the Gitea token file.

The token file is the path `env_files.github_token` (or `gitlab_token`,
`gitea_token`) names, else `~/.config/anodizer/github_token`, else
`~/.config/goreleaser/github_token`. Its first line is the token. A file is
read only when every variable before it is unset or blank, and a file that
exists but cannot be read stops the run:

```yaml
env_files:
  github_token: ~/.secrets/release_token
```

### Forge selection {#choosing-the-forge}

The first of these that is set decides whether the release goes to GitHub,
GitLab or Gitea:

| Order | Source | Values |
|---|---|---|
| 1 | `release.provider` | `github`, `gitlab`, `gitea` |
| 2 | top-level `force_token` | `github`, `gitlab`, `gitea` |
| 3 | `ANODIZER_FORCE_TOKEN`; when it is unset or blank, `GORELEASER_FORCE_TOKEN`; both are trimmed | the same three, in any letter case; any other value is ignored |
| 4 | `GITLAB_TOKEN` in the environment, or a GitLab token file | GitLab |
| 5 | `GITEA_TOKEN` in the environment, or a Gitea token file | Gitea |
| 6 | nothing set | GitHub |

A project whose CI holds a `GITLAB_TOKEN` for another job and releases to
GitHub sets `provider: github`:

```yaml
release:
  provider: github
  github:
    owner: myorg
    name: myapp
```

## Common gotchas

- **Draft vs published**: a draft release is only visible to repo collaborators. Set `draft: false` (the default) for publicly visible releases.
- **`make_latest: auto`**: by default, anodizer marks a release as latest only when the version is not a prerelease. Override with `make_latest: true` or `make_latest: false`.
- **Asset name collisions**: an asset already on the release under the same file name with different bytes fails the upload unless `replace_existing_artifacts: true` is set on the `release:` block. See [Re-uploading onto an existing release](#re-uploads-onto-an-existing-release).
- **Renamed or transferred repository**: GitHub answers an asset upload for a moved repository with a redirect that names no new location. anodizer does not follow it and fails the upload naming the repository:

  ```text
  release: upload artifact 'myapp_1.2.3_linux_amd64.tar.gz' to release 'v1.2.3' — GitHub redirected the upload (307); the repository myorg/myapp may have been renamed or transferred, update the git remote or release.github to its current location
  ```

## Republish / update behavior

Use `replace_existing_draft: true` and `replace_existing_artifacts: true` on the `release:` block for re-runnable workflows. See [Recovery flags](../advanced/recovery-flags.md#release-replace-existing-draft) for the full mechanism.

## Re-uploads onto an existing release

A re-run that meets an asset already attached to the release under the same file name decides per asset:

| Asset on the release | `replace_existing_artifacts: false` (default) | `replace_existing_artifacts: true` |
|---|---|---|
| same bytes (digest, or size when GitHub reports no digest) | skipped as already uploaded | skipped as already uploaded |
| a partial upload left by an interrupted run | deleted and uploaded again | deleted and uploaded again |
| different bytes | the upload fails, naming the asset and the flag | deleted and uploaded again |

```yaml
release:
  replace_existing_artifacts: true   # overwrite an asset whose bytes differ
```

The delete and the second upload happen inside the same attempt, so a conflict met on the last attempt is still replaced. The second upload is judged like the first: a 4xx other than a rate limit fails at once, and a rate limit waits its own slot.

anodizer reads the status of every delete it sends (the conflicting asset, a replaced draft, a nightly retention prune). A delete that GitHub refuses fails the upload naming the asset and the release, since keeping the old asset would leave another release's bytes under this tag.

GitHub's asset delete is eventually consistent, so a deleted asset can reappear moments later. The next attempt deletes it again and uploads, within the upload's attempt budget. When every attempt meets the reappeared asset, the error says so:

```text
release: upload artifact 'myapp_1.2.3_linux_amd64.tar.gz' to release 'v1.2.3' failed after 10 attempts: asset reappeared after its delete and re-upload: …
```

## Draft releases

```yaml
release:
  draft: true
```

### Draft reuse

`use_existing_draft: true` looks for a draft release whose title is the rendered `name_template` and updates it in place, composing its body per `mode`. A draft whose title is only the tag is passed over when `name_template` renders something else. `replace_existing_draft: true` deletes such a draft before the new release is created.

```yaml
release:
  name_template: "{{ ProjectName }} {{ Version }}"
  use_existing_draft: true   # update the draft titled "myapp 1.2.3"
```

The run prints the draft it picked:

```text
   • reusing existing draft release 'myapp 1.2.3' (id=123456)
```

## Prerelease detection

When `prerelease: auto` (default), anodizer detects prereleases from the version string. Versions like `1.0.0-rc.1`, `1.0.0-beta.2` are automatically marked as prereleases.

## Extra files

Upload additional files that aren't part of the pipeline:

```yaml
release:
  extra_files:
    - "dist/completions/*"
    - "docs/man/*.1"
```

## Full example

```yaml
crates:
  - name: myapp
    release:
      github:
        owner: myorg
        name: myapp
      name_template: "{{ ProjectName }} {{ Version }}"
      header: |
        ## What's Changed
      prerelease: auto
      make_latest: auto
```
