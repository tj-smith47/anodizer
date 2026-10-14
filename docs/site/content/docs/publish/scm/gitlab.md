+++
title = "GitLab"
description = "Create releases and upload assets to GitLab"
weight = 1
template = "docs.html"
+++

Anodizer can create releases and upload assets to GitLab repositories.

## Minimal config

```yaml
release:
  gitlab:
    owner: mygroup
    name: myproject
```

`owner` and `name` are inferred from the `origin` git remote when omitted, so a block may name only a token override:

```yaml
release:
  gitlab:
    token: "{{ .Env.RELEASE_GITLAB_TOKEN }}"   # owner/name come from the origin remote
```

The remote has to be on GitLab's host: gitlab.com (`altssh.gitlab.com` counts), or the host `gitlab_urls.api` names for a self-hosted instance. A remote on another host fills nothing and the block is refused naming that host. A block with an empty `owner` whose `name` is a numeric project id (`name: "12345"`) or a full path (`name: "group/sub/widget"`) has no namespace to infer and is left as written; a bare `name` (`name: "widget"`) has its `owner` filled from the remote.

## GitLab-specific config

The `release` config is shared across all SCM providers. See the [GitHub releases](/docs/publish/github/) page for the full list of shared fields. The GitLab-specific fields are:

### GitLab URLs (`gitlab_urls`)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `api` | string | `https://gitlab.com/api/v4/` | GitLab API base URL |
| `download` | string | `https://gitlab.com` | GitLab download base URL |
| `skip_tls_verify` | bool | `false` | Skip TLS certificate verification |
| `use_package_registry` | bool | `false` | Upload to the Package Registry; when `false`, files go to project uploads |
| `use_job_token` | bool | `false` | Authenticate with `CI_JOB_TOKEN`; when `false`, a personal token is used |

## Environment variables

| Variable | Description |
|----------|-------------|
| `GITLAB_TOKEN` | Personal access token or project token |
| `CI_JOB_TOKEN` | CI job token (when `use_job_token: true`) |

## Upload methods

GitLab supports two upload methods:

- **Project Uploads** (default) — uploads via `POST /projects/{id}/uploads` (multipart), then creates release links
- **Package Registry** — uploads via `PUT /projects/{id}/packages/generic/{package}/{version}/{filename}`

```yaml
gitlab_urls:
  use_package_registry: true
```

## Release modes

Control how release notes are handled when a release already exists:

| Mode | Behavior |
|------|----------|
| `keep-existing` | Keep existing release body unchanged |
| `append` | Append new content after existing body |
| `prepend` | Prepend new content before existing body |
| `replace` | Replace existing body entirely |

```yaml
release:
  gitlab:
    owner: mygroup
    name: myproject
  mode: append
```

## Behavior

- GitLab does not support draft releases
- Existing release detection: checks by tag; creates new on 403/404, updates on 200
- Release links: each uploaded file gets a release link. Creating the link retries on a 5xx or a 429 per the top-level `retry:` policy, and a 429 waits the `Retry-After` or `RateLimit-Reset` GitLab sent (see [Retry](@/docs/general/retry.md))
- `replace_existing_artifacts`: on conflict (400/422), deletes the link of the same name and creates the new one in the same attempt; a 400 that no existing link explains is reported as the create failure. When the cleanup itself fails, the error prints once, the create failure first and the cleanup failure after it:

  ```text
  gitlab: create release link for 'myapp_1.2.3_linux_amd64.tar.gz' failed (HTTP 400): …; cleaning up the existing link failed: …
  ```

- Finding the link to replace reads every page of the release's links, 100 per page. The sweep stops with an error when a page repeats the one before it (a server ignoring the `page` parameter; pages are compared by their link ids when every link has one, else by their raw text) or when the retry deadline passes
- Server version probe: GitLab 17 renamed the link's `filepath` field to `direct_asset_path`. anodizer reads `CI_SERVER_VERSION` when it is set, else asks `GET /version` once per run (3 attempts, retrying a 5xx or a 429), and parallel uploads share the answer. A probe that gets no answer uploads that asset with `filepath` and the next asset asks again
- URL-encoded project paths (e.g., `group/subgroup/project` becomes `group%2Fsubgroup%2Fproject`)
- 300-second HTTP timeout
- Release preflight: before the build, `GET /projects/{id}` checks that the token has Developer access or higher (the higher of the project and group access; a response with no `permissions` passes). A 404, 401 or 403 fails the check whatever headers it carries, because GitLab throttles with a 429; a 429, a 5xx or an unreachable server is a warning, or a failure under [`preflight.strict`](@/docs/general/preflight.md). Retries print under the label `gitlab project probe`. A failed check stops the run unless `release.preflight.fail_on_error` is `false`; see [Release preflight](@/docs/publish/github.md#release-preflight)
- CI job token: a run that authenticates with a CI job token (`use_job_token`) skips the access check, because GitLab does not let a job token read `GET /projects/{id}`. The skip prints once per project at default output:

  ```text
     • skipped the GitLab access check for mygroup/myproject: a CI job token cannot read the project
  ```

## Self-hosted GitLab

```yaml
gitlab_urls:
  api: "https://gitlab.mycompany.com/api/v4/"
  download: "https://gitlab.mycompany.com"
```

## Full example

```yaml
release:
  gitlab:
    owner: mygroup
    name: myproject
  name_template: "{{ ProjectName }} {{ Version }}"
  header:
    from_file: RELEASE_HEADER.md
  mode: replace
  replace_existing_artifacts: true

gitlab_urls:
  use_package_registry: true
  use_job_token: true
```
