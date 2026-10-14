# Contributing to Anodizer

Thank you for your interest in contributing to Anodizer! This document provides guidelines and instructions for contributing.

## Getting Started

### Prerequisites

- Rust stable toolchain (install via [rustup](https://rustup.rs/))
- Git
- Optionally: `cargo-zigbuild`, `cross`, Docker, `nfpm` (for testing specific stages)

### Setup

```bash
# Clone the repository
git clone https://github.com/tj-smith47/anodizer.git
cd anodizer

# Build the project
cargo build

# Run tests
cargo test

# Run the CLI
cargo run -- --help
```

### Project Structure

```
anodizer/
  crates/
    cli/              # CLI entry point (clap-based)
    core/             # Core types: config, template, artifact, pipeline context
    stage-archive/    # Archive creation (tar.gz, tar.xz, tar.zst, zip, binary)
    stage-build/      # Cargo build orchestration
    stage-changelog/  # Changelog generation
    stage-checksum/   # Checksum computation
    stage-docker/     # Docker image building
    stage-nfpm/       # Linux package generation
    stage-publish/    # crates.io, Homebrew, Scoop publishing
    stage-release/    # GitHub release creation
    stage-sign/       # Artifact and Docker signing
  docs/               # Documentation
  tests/              # Integration and E2E tests
```

Each `stage-*` crate implements a single pipeline stage. The `core` crate provides shared types (`Config`, `TemplateVars`, `Artifact`, `PipelineContext`) that stages operate on.

## Development Workflow

### Branching

- `main` is the stable branch.
- Feature branches are named `feature/<description>` or `session-N/<description>`.
- Bug fix branches are named `fix/<description>`.

### Making Changes

1. Create a branch from `main`.
2. Make your changes with clear, focused commits.
3. Write or update tests for your changes.
4. Ensure all tests pass: `cargo test`.
5. Ensure code compiles without warnings: `cargo build`.
6. Open a pull request against `main`.

### Commit Messages

We follow [Conventional Commits](https://www.conventionalcommits.org/):

```
feat: add support for tar.zst archives
fix: handle empty changelog gracefully
docs: update configuration reference
test: add integration tests for checksum stage
refactor: simplify template preprocessing
chore: update dependencies
```

The commit type is used by the changelog generator to group entries.

### Code Style

- Follow standard Rust conventions and `rustfmt` formatting.
- Run `cargo fmt --check` before committing.
- Run `cargo clippy -- -D warnings` to catch common issues.
- Prefer explicit error handling with `anyhow::Context` over bare `unwrap()`.
- Use `eprintln!` with colored prefixes for user-facing messages (not `println!`).

### Testing

#### Unit Tests

Each crate has inline unit tests. Run them with:

```bash
cargo test
```

Or test a specific crate:

```bash
cargo test -p anodizer-core
cargo test -p anodizer-stage-archive
```

#### Integration Tests

Integration tests live in `crates/cli/tests/` and test multi-stage interactions:

```bash
cargo test -p anodizer-cli --test integration
```

#### E2E Tests

End-to-end tests that exercise the full CLI live in `tests/`:

```bash
cargo test --test e2e
```

### Adding a New Stage

1. Create a new crate: `cargo new crates/stage-mystage --lib`.
2. Add it to the workspace `Cargo.toml`.
3. Implement the `Stage` trait or follow the pattern from existing stages.
4. Wire it into the pipeline in `crates/cli/src/pipeline.rs`.
5. Add config structs to `crates/core/src/config.rs`.
6. Add tests (both unit and integration).
7. Document the new config fields via `schemars` doc comments on the new
   struct — the docsite reference at `docs/site/content/docs/reference/configuration.md`
   regenerates from those via `cargo xtask gen-docs`.

### Adding a New Config Field

1. Add the field to the appropriate struct under `crates/core/src/config/`.
2. Use `Option<T>` for optional fields with `#[serde(default)]`.
3. Add validation in `crates/cli/src/commands/check/config/` if needed.
4. Add a `///` doc comment describing the field — picked up by `schemars`
   and surfaced in the regenerated docsite reference and JSON schema.
5. Add a test that parses a YAML/TOML config with the new field.

### Adding a New Template Variable

1. Add the variable in the appropriate stage where `TemplateVars` is populated.
2. Document it in `docs/site/content/docs/general/templates.md`.
3. Add a test in `crates/core/src/template/tests.rs`.

### YAML Examples in the Docs

Every ```` ```yaml ```` block on a docs page, in `README.md` and in a crate's
`README.md` loads as a whole `.anodizer.yaml` through `anodizer check config`.
The test `every_yaml_block_in_the_docs_loads_as_a_config`
(`crates/cli/tests/docs_config_blocks.rs`) loads each one and fails naming the
page, the line of the fence and the error. It creates every crate `path:`,
build `dir:` and local include file the block names, so an example can name
several crates.

Write a crate-level example inside the `crates:` list, the way the page's
other examples do:

```yaml
crates:
  - name: myapp
    archives:
      - name_template: "{{ ProjectName }}-{{ Version }}-{{ Target }}"
```

Two kinds of block are not a whole config, and each is marked:

- A piece of a config shown on its own, where the full shape would bury the
  point (a lone `if:` or `name_template:` value), or a config shown being
  refused, carries the `fragment` tag after the language. Zola reads the
  language up to the comma, so the block is still highlighted as YAML:

  ````markdown
  ```yaml,fragment
  if: "{{ IsSnapshot }}"
  ```
  ````

- YAML that is some other file (a GitHub Actions workflow, a krew or winget
  manifest) opens with the line `# not an anodizer config`. GitHub and
  crates.io do not read the `fragment` tag, so use this line in a README too:

  ```yaml
  # not an anodizer config
  name: Release
  on:
    push:
      tags: ["v*"]
  ```

A marked block that loads as a config fails the test, so remove the mark
once the block is whole. Any other tag after the language fails it as well.

## Reporting Issues

When reporting issues, please include:

- Anodizer version (`anodizer --version`)
- Operating system and architecture
- Rust toolchain version (`rustc --version`)
- Your `.anodizer.yaml` config (redact sensitive values)
- The full error output
- Steps to reproduce the issue

## Pull Request Guidelines

- Keep PRs focused on a single concern.
- Include tests for new functionality.
- Update documentation for user-facing changes.
- Ensure CI passes before requesting review.
- Link related issues in the PR description.

## License

By contributing to Anodizer, you agree that your contributions will be licensed under the MIT License.
