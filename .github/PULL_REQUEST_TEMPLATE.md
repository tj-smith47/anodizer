## Summary

<!-- Brief description of what this PR does -->

## Type of Change

- [ ] Bug fix
- [ ] New feature (new pipeline stage, publisher, announcer, archive format, etc.)
- [ ] Enhancement to existing feature
- [ ] GoReleaser parity gap closed
- [ ] Refactoring (no functional change)
- [ ] Documentation
- [ ] CI/CD

## Changes Made

-

## Checklist

### Code Quality
- [ ] No `unwrap()` or `expect()` on user-reachable paths in library code
- [ ] `thiserror` for library errors, `anyhow` only in `main.rs` / `cli/`
- [ ] Import grouping: std, external, internal (blank line separated)
- [ ] New config fields documented in the JSON schema and surfaced in `anodizer jsonschema`

### Testing
- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes
- [ ] `cargo fmt --check` passes
- [ ] Pipeline-stage unit tests added/updated for the affected `stage-*` crate
- [ ] Integration test added/updated under `crates/cli/tests/` if behavior is user-visible
- [ ] GoReleaser-parity test added/updated if this closes a parity gap

### Documentation
- [ ] Help text updated (if adding/changing commands or flags)
- [ ] README / docs site updated (if user-facing change)
- [ ] CHANGELOG.md updated (if user-facing change)

## Testing Done

<!-- How did you test this? `anodizer release --snapshot`, `--dry-run`, a real tagged release, etc. -->

## Related Issues

<!-- Link to related issues: Fixes #123, Relates to #456 -->
