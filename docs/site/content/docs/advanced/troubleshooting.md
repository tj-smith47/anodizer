+++
title = "Troubleshooting"
description = "Diagnose and fix common anodizer issues"
weight = 10
template = "docs.html"
+++

# Troubleshooting

When something goes wrong during a release, anodizer provides several flags to help you understand what happened and why.

## Verbosity Flags

### `--quiet` / `-q`

Suppress all non-error output. Useful in CI pipelines where you only want to see failures:

```bash
anodizer release --quiet
```

Only error messages will be printed to stderr. Stdout remains clean for machine-parseable output.

### `--verbose`

Show detailed output from all stages, including:

- External command stdout/stderr (not just on failure)
- Template variable context before rendering
- Environment variables passed to build commands
- File paths being created, copied, or archived

```bash
anodizer release --verbose
```

### `--debug`

Maximum detail. Includes everything from `--verbose`, plus:

- Full HTTP request/response details for GitHub API calls
- The resolved configuration after includes and overrides are merged
- Artifact registry contents at each pipeline stage boundary

```bash
anodizer release --debug
```

## Common Issues

### Build failures

When an external command (cargo, cross, zigbuild) fails, anodizer captures and displays the full stderr output. Look for compiler errors in the output.

If you only see an exit code, run with `--verbose` to see the full command output:

```bash
anodizer release --verbose 2>&1 | less
```

### Missing GitHub token

If you see an error about a missing `GITHUB_TOKEN`, either:

1. Set the environment variable: `export GITHUB_TOKEN=ghp_...`
2. Pass it via CLI: `anodizer release --token ghp_...`

### API errors

When a GitHub API call fails, anodizer displays the HTTP status code and response body. Common causes:

- **401 Unauthorized**: Token is invalid or expired
- **403 Forbidden**: Token lacks required permissions (needs `repo` scope)
- **404 Not Found**: Repository doesn't exist or token can't access it
- **422 Unprocessable Entity**: Tag already exists, or validation failed

Run with `--debug` to see the full HTTP request and response headers.

### Config validation errors

Run `anodizer check` to validate your configuration without running a release:

```bash
anodizer check --verbose
```

This catches issues like:
- Unknown fields (typos in config keys)
- Invalid values (wrong type, unsupported format)
- Circular `depends_on` references
- Missing required fields

### Timeout issues

The default pipeline timeout is `3h`, a backstop; per-stage bounds such as `announce.deadline` catch a hung stage much sooner. To change it:

```bash
anodizer release --timeout 90m
```

### Dirty-tree refusal on an unreadable git status

The release refuses a dirty working tree unless the run is `--snapshot`, nightly or `--dry-run`. A working tree whose status git cannot read counts as dirty, so a corrupt index or git's `safe.directory` refusal (a checkout owned by another user, common in containers) stops the release with an empty "Dirty files" list. `anodizer bump` refuses the same tree, and `--auto-snapshot` switches to snapshot mode on it. Run git yourself to see the cause:

```bash
git status --porcelain
# fatal: detected dubious ownership in repository at '/work'
git config --global --add safe.directory /work
```

### Background processes that outlive their command

When a command anodizer runs (a hook, a build or sign tool) starts a process of its own that keeps its stdout, stderr or stdin open (`tool &`, a daemon it launched), anodizer stops reading those pipes 5 seconds after the command itself exits. The command's own exit status and the output read until then are kept, a last line without a trailing newline included, and the step carries on. Under `--verbose` the cut-off is printed:

```text
stopped using the pipes of <label> 5s after it exited: a process it started still holds them
```

The background process is not killed by this, except under a step that has its own timeout, which ends the leftover process tree after a short grace. On Windows the stdin writer is the one exception: it can only check between 16 KiB writes, so a write the leftover process never reads keeps that pipe open until the process reads or exits. Redirect the background process's streams (`tool >/dev/null 2>&1 </dev/null &`) to keep its output out of the step.

### Retry lines and rate limits

Every retried HTTP call prints one line per failed attempt, with the wait before the next one:

```text
upload of 'app.tar.gz' attempt 1/10 failed (<cause>); retrying in 2m
```

A `429 Too Many Requests` that carries `Retry-After` or `RateLimit-Reset` waits the time the server asked for, so a long wait on that line is the server's request. When the run's retry deadline cannot hold that wait, the retries end at once. An error ending in `exhausted retry attempts` means every attempt was spent; a 4xx other than 429 fails on its first attempt and never says so. See [Retry](@/docs/general/retry.md).

### Dry-run mode

Always test with `--dry-run` before a real release:

```bash
anodizer release --dry-run --verbose
```

This runs the full pipeline without creating releases, pushing tags, or publishing packages. Combined with `--verbose`, you can see exactly what would happen.

## Help

- [GitHub Issues](https://github.com/tj-smith47/anodizer/issues) -- report bugs or request features
- `anodizer --help` -- full CLI reference
- `anodizer check` -- validate your configuration
