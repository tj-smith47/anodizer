# Security Policy

## Supported Versions

| Version | Supported          |
|---------|--------------------|
| 0.x.x   | :white_check_mark: |

## Reporting a Vulnerability

**Do not open a public issue for security vulnerabilities.**

Please report security issues privately via
[GitHub Security Advisories](https://github.com/tj-smith47/anodizer/security/advisories/new).
Include:

- Description of the vulnerability
- Steps to reproduce
- Impact assessment
- Any suggested fix (optional)

### Response Timeline

- **48 hours** - Acknowledgment of receipt
- **7 days** - Initial assessment and severity rating
- **30-90 days** - Resolution, depending on complexity

## Security Considerations

Anodizer is release-automation tooling. A single `anodizer release` run can build
binaries, sign artifacts, push to many registries, and announce to many channels.
The interesting threat surface follows from that scope:

- **Token handling** - Anodizer reads forge tokens (`GITHUB_TOKEN`,
  `ANODIZER_GITHUB_TOKEN`, GitLab, Gitea), `CARGO_REGISTRY_TOKEN`, Homebrew tap
  deploy keys, container registry credentials, and announcer webhook secrets
  from environment variables at runtime. Anodizer does not persist these
  tokens to disk. Anything you put in `.anodizer.yaml` is committed - keep
  secrets out of the config.
- **Signing key custody** - Cosign and GPG signing for binaries, archives,
  checksums, Docker images, and SBOMs is supported. Keys are referenced by
  path or pulled from the signer's own keychain - anodizer invokes the signer
  binary rather than loading raw key material itself.
- **Registry publishing** - A release run can push to crates.io, Homebrew
  taps, Scoop buckets, Chocolatey, Winget, AUR, Krew, npm, Snapcraft, Flatpak,
  Docker Hub, GHCR, Artifactory, Cloudsmith, MCP registry, S3/GCS/Azure blob
  storage, and custom publisher commands. A misconfigured config could push to
  the wrong destination or to a destination you do not own. Review your
  `publish` blocks before tagging.
- **Hooks execution** - `before` / `after` build hooks, custom publisher
  commands, and `template_files` execute arbitrary shell with the privileges
  of the invoking user (typically your CI runner). Treat them like CI scripts:
  pin tool versions, avoid `curl | sh`, and review them in PR.
- **Tera template execution** - Config files include Tera templates that can
  read environment variables (`{{ .Env.FOO }}`) and emit text into release
  artifacts (release notes, changelog, package manifests). Templates with
  attacker-controlled input could leak env values or inject content into
  published metadata.
- **Announce webhooks** - Discord/Slack/Telegram/Teams/Mattermost/email/generic
  webhook announcers send templated release messages. Compromised webhook URLs
  let third parties post on your behalf; treat them as secrets.

## Best Practices

- Pass forge and registry tokens via env vars in your CI secret store. Never
  commit tokens into `.anodizer.yaml` or any file under version control.
- Pin signing identities per release. Cosign keyless (OIDC) signing in CI is
  recommended; if you use a long-lived key, store it in your CI secret store
  and audit access.
- Run `anodizer check` to validate your config and `anodizer release --dry-run`
  (or `--snapshot`) before tagging. Both run the full pipeline without
  publishing or pushing.
- Review hook scripts (`builds[].hooks.pre`, `hooks.post`,
  `publishers[].cmd`, etc.) and Tera templates the same way you'd review CI
  workflows. Anything they print or upload becomes part of your release.
- Keep anodizer updated to the latest release.
- Use `permissions:` blocks in GitHub Actions to scope `GITHUB_TOKEN` to the
  minimum required (`contents: write` for releases, `packages: write` only
  when pushing to GHCR, etc.).
- Treat announcer webhooks as secrets. Rotate them if a CI run logs them by
  accident.
