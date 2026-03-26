# Contributing to Anodize

Thank you for your interest in contributing to Anodize! This document provides guidelines and instructions for contributing.

## Getting Started

### Prerequisites

- Rust stable toolchain (install via [rustup](https://rustup.rs/))
- Git
- Optionally: `cargo-zigbuild`, `cross`, Docker, `nfpm` (for testing specific stages)

### Setup

```bash
# Clone the repository
git clone https://github.com/tj-smith47/anodize.git
cd anodize

# Build the project
cargo build

# Run tests
cargo test

# Run the CLI
cargo run -- --help
```

### Project Structure

```
anodize/
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
cargo test -p anodize-core
cargo test -p anodize-stage-archive
```

#### Integration Tests

Integration tests live in `crates/cli/tests/` and test multi-stage interactions:

```bash
cargo test -p anodize-cli --test integration
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
7. Document the new config fields in `docs/configuration.md`.

### Adding a New Config Field

1. Add the field to the appropriate struct in `crates/core/src/config.rs`.
2. Use `Option<T>` for optional fields with `#[serde(default)]`.
3. Add validation in `crates/cli/src/commands/check.rs` if needed.
4. Update the documentation in `docs/configuration.md`.
5. Add a test that parses a YAML/TOML config with the new field.

### Adding a New Template Variable

1. Add the variable in the appropriate stage where `TemplateVars` is populated.
2. Document it in `docs/templates.md`.
3. Add a test in `crates/core/src/template.rs`.

## Reporting Issues

When reporting issues, please include:

- Anodize version (`anodize --version`)
- Operating system and architecture
- Rust toolchain version (`rustc --version`)
- Your `.anodize.yaml` config (redact sensitive values)
- The full error output
- Steps to reproduce the issue

## Pull Request Guidelines

- Keep PRs focused on a single concern.
- Include tests for new functionality.
- Update documentation for user-facing changes.
- Ensure CI passes before requesting review.
- Link related issues in the PR description.

## License

By contributing to Anodize, you agree that your contributions will be licensed under the MIT License.
