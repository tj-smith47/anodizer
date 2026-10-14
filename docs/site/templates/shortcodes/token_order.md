The push to {{ target }} uses the first token that is set:

1. `repository.token`
2. `{{ publisher_vars | join(sep="`, then `") }}`
3. the run's GitHub token (`--token`, or the `ANODIZER_GITHUB_TOKEN` / `GITHUB_TOKEN` / `GH_TOKEN` value read at startup, or, when all three are unset, the GitHub token file: `env_files.github_token`, else `~/.config/anodizer/github_token`, else `~/.config/goreleaser/github_token`); a GitLab or Gitea token is never sent to GitHub
4. `ANODIZER_GITHUB_TOKEN`, `GITHUB_TOKEN`, `GH_TOKEN`

The pull request is opened with the first token that is set:

1. `repository.pull_request.token`
2. `repository.token`
3. `{{ publisher_vars | join(sep="`, then `") }}`
4. the run's GitHub token (`--token`, or the `ANODIZER_GITHUB_TOKEN` / `GITHUB_TOKEN` / `GH_TOKEN` value read at startup, or, when all three are unset, the GitHub token file: `env_files.github_token`, else `~/.config/anodizer/github_token`, else `~/.config/goreleaser/github_token`); a GitLab or Gitea token is never sent to GitHub
5. `ANODIZER_GITHUB_TOKEN`, `GITHUB_TOKEN`, `GH_TOKEN`

{% if gh_login is defined and not gh_login %}With none of these set, the publish fails.{% else %}With none of these set, the `gh` CLI's own login opens the pull request; with neither a token nor `gh`, the publish fails.{% endif %}
