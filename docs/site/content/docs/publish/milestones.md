+++
title = "Milestones"
description = "Automatically close milestones after a release"
weight = 88
template = "docs.html"
+++

Anodizer can automatically close milestones on GitHub, GitLab, or Gitea after a release completes.

## Classification

Manager — closes a milestone via the upstream forge API after a successful release. Required: false (no-op when `close: false`).

## Minimal config

```yaml
milestones:
  - close: true
```

## Full config reference

```yaml
milestones:
  - close: false                       # optional; only acts when true
    name_template: "{{ .Tag }}"        # optional; milestone name to match (template)
    repo:                              # optional; override the repository
      owner: ""
      name: ""
    fail_on_error: false               # optional; make milestone errors fatal
```

## Authentication

Re-uses the release publisher's credentials. The same token used to create the GitHub/GitLab/Gitea release closes the milestone — no separate config field is needed.

## Common gotchas

- The milestone name template is rendered and matched against open milestones. A name mismatch silently no-ops.
- Repository is auto-detected from the first crate's release config (GitHub/GitLab/Gitea).
- Errors are logged as warnings by default; set `fail_on_error: true` to make them fatal.

### Provider-specific behavior

| Provider | How milestones are found | How they are closed |
|----------|--------------------------|---------------------|
| GitHub | Paginated listing (100/page), title match | PATCH `state: "closed"` |
| GitLab | API filter by title | PUT `state_event: "close"` |
| Gitea | API filter by name | PATCH `state: "closed"` |

## Custom milestone name

Match a milestone with a name different from the tag:

```yaml
milestones:
  - close: true
    name_template: "v{{ .Version }}"
```

## Full example

```yaml
milestones:
  - close: true
    name_template: "{{ .Tag }}"
    fail_on_error: true
```
