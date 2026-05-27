+++
title = "AI-Enhanced Release Notes"
description = "Wrap the generated changelog through an LLM (Anthropic, OpenAI, or local Ollama) before it ships"
weight = 8
template = "docs.html"
+++

The `changelog.ai` block in your `.anodizer.yaml` hands the rendered
release notes to an LLM, lets it produce a polished summary, and then
uses the model's response as the release body that ships to GitHub,
GitLab, and Gitea.

## Quick start

```yaml
changelog:
  ai:
    use: anthropic
    model: claude-sonnet-4-6
    prompt: |
      Summarise these release notes for end users. Group dependency
      bumps into a single line. Do not use emojis.

      {{ ReleaseNotes }}
```

```bash
export ANTHROPIC_API_KEY=sk-ant-...
anodize release
```

The flow:

1. The native changelog generator produces the SCM-style body (or
   `--release-notes-tmpl` does, if set — see [Precedence](#precedence)).
2. The prompt is rendered through Tera with the full template context
   plus a one-shot `ReleaseNotes` variable bound to that body.
3. The configured provider is called once per crate.
4. The provider's response REPLACES the body for that crate.
5. The replaced bodies are wrapped with `changelog.header` /
   `changelog.footer` and written to `dist/CHANGELOG.md`.

## Providers

| `use:` value | Default model    | Auth env var        | Endpoint base                       |
|--------------|------------------|---------------------|-------------------------------------|
| `anthropic`  | `claude-sonnet-4-6` | `ANTHROPIC_API_KEY` | `https://api.anthropic.com`          |
| `openai`     | `gpt-4o-mini`    | `OPENAI_API_KEY`    | `https://api.openai.com`            |
| `ollama`     | `llama3.1`       | none                | `${OLLAMA_HOST:-http://localhost:11434}` |

Override the model per release with `model:`. Override the endpoint
base via `ANODIZER_ANTHROPIC_ENDPOINT`, `ANODIZER_OPENAI_ENDPOINT`,
or `ANODIZER_OLLAMA_ENDPOINT`. Use cases:

- Routing traffic through a corporate proxy that fronts the upstream API.
- Pointing at a regional / private mirror that re-exposes the same
  contract (Azure OpenAI gateway, internal Anthropic relay, etc.).
- Pointing at an OpenAI-API-compatible inference server (vLLM, LiteLLM)
  for the `openai` provider.
- Pointing `ollama` at a remote Ollama host on another machine.

The Ollama provider also honours the upstream-standard `OLLAMA_HOST`
when `ANODIZER_OLLAMA_ENDPOINT` is unset.

## Prompt sources

The `prompt:` field accepts three shapes — an inline string, a file
path, or a URL. File takes priority over URL when both are set.

### Inline

```yaml
changelog:
  ai:
    use: openai
    prompt: "Improve these notes: {{ ReleaseNotes }}"
```

### From file

```yaml
changelog:
  ai:
    use: anthropic
    prompt:
      from_file:
        path: .anodizer/release-prompt.md
```

### From URL (with env-expanded headers)

```yaml
changelog:
  ai:
    use: ollama
    prompt:
      from_url:
        url: https://prompts.example.com/release.md
        headers:
          X-Auth: "Bearer ${PROMPT_TOKEN}"
```

`${VAR}` and `$VAR` references in header values are expanded from the
process environment before the HTTP request is sent. Unset variables
expand to the empty string (matching shell semantics).

### Default prompt

If `prompt:` is omitted, anodizer uses an internal default that asks
the model to write a short intro, merge dependency bumps into a single
line, and omit emojis.

## Template context

The prompt is rendered through the same Tera engine as the rest of
your config — every variable you can use in `release.header` is
available here. The additional `ReleaseNotes` variable is scoped to
the prompt-rendering call only; it does NOT pollute the global
template namespace.

```yaml
changelog:
  ai:
    use: anthropic
    prompt: |
      Project: {{ .ProjectName }}
      Tag: {{ .Tag }}

      Polish these notes:

      {{ ReleaseNotes }}
```

## Interaction with `--release-notes` / `--release-notes-tmpl`

`--release-notes <path>` and `--release-notes-tmpl <path>` short-circuit
the entire changelog stage: the file (rendered through Tera, for the
`-tmpl` variant) becomes the release body verbatim, and no further
processing runs. **AI enhancement is bypassed when either flag is set.**

The rationale: an operator who hands anodizer a pre-written release
body has already decided what should ship. Quietly handing it to a
model for "improvement" would mutate the operator's text without an
opt-in.

If you want template-driven enhancement (project-aware sections, custom
intro paragraphs, etc.), bake the template logic into the AI prompt
itself rather than into a separate notes file:

```yaml
changelog:
  ai:
    use: anthropic
    prompt: |
      You are writing the release notes for {{ .ProjectName }} {{ .Tag }}.
      Start with a one-paragraph intro mentioning the project name.
      Then polish the changelog below.

      {{ ReleaseNotes }}
```

The prompt has the full template context, so you can drive the same
shape as a `--release-notes-tmpl` file from inside the AI flow.

## Interaction with `changelog.groups`

**Enabling AI disables changelog grouping** (parity with GoReleaser).
When `changelog.ai.use` is set, `changelog.groups` is ignored and the
AI provider receives a flat commit list, leaving the structural
decisions (sections, ordering, grouping by topic) to the model.

If you need both grouping AND AI polish, instruct the model to produce
its own group headings in the prompt:

```yaml
changelog:
  ai:
    use: anthropic
    prompt: |
      Group these commits under "### Features", "### Bug Fixes",
      and "### Dependencies updated" headings.

      {{ ReleaseNotes }}
  groups:
    - title: Features
      regexp: "^feat"
```

The `groups:` block above is parsed but ignored at render time when
`ai.use` is set. Leaving it in place lets you toggle AI off (clear
`ai.use`) and immediately get the grouped output back without
re-authoring the section list.

## Error policy

By default anodizer is **fail-closed**: any provider error
(transport, non-2xx status, JSON parse) aborts the release. This
matches GoReleaser's "any hook failure aborts" pattern — a silent
fall-back to the raw notes would ship the wrong body without the
operator noticing.

Opt in to degraded behaviour with `--allow-ai-failure`:

```bash
anodize release --allow-ai-failure
```

With the flag set, a provider error is logged as a warning and the
pre-AI release notes are kept verbatim. Use this when AI enhancement
is a "nice to have" rather than a hard requirement (e.g., local
Ollama in CI where transient unavailability shouldn't block a tag).

## Snapshot mode

AI enhancement is automatically skipped in `--snapshot` mode for cost
containment. Snapshot builds are typically rapid local iterations, and
billing a model per local test run adds up. To preview AI-enhanced
notes without publishing, run `anodize release --dry-run` (which
keeps the AI call active) instead.

## Secret hygiene

- API keys are read from environment variables and are never present
  on argv.
- `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, and any `*_KEY` /
  `*_TOKEN` / `*_SECRET` env var are automatically masked in stage
  log output by the built-in redactor.
- Inline `sk-` prefixed values are masked by the value-prefix
  redactor even when exported under a non-standard variable name.
- Header values fetched via `from_url` are env-expanded at request
  time so a `Bearer ${TOKEN}` literal in YAML never reaches the disk
  cache or log output as the plain token.

## Troubleshooting

**Provider returns 401**: The auth env var is unset or invalid. The
error message includes the status code but never the key value.

**Provider returns 503 / network error**: Fail-closed by default;
re-run with `--allow-ai-failure` to degrade gracefully if the model
is non-critical to your release flow.

**Unknown provider**: anodizer bails with a list of valid names
(`anthropic`, `openai`, `ollama`) at the start of the changelog
stage so you don't waste a build.

**Prompt fetch via `from_url` fails**: The error includes the URL and
HTTP status. Check the `headers:` map — unset env vars in `${VAR}`
references expand to the empty string, which may produce a malformed
`Authorization` header.
