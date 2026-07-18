use super::*;

/// Validate the config schema version. Accepts version 1 (default) and 2.
/// Returns an error for unknown versions.
pub fn validate_version(config: &Config) -> Result<(), String> {
    match config.version {
        None | Some(1) | Some(2) => Ok(()),
        Some(v) => Err(format!(
            "unsupported config version: {}. Supported versions are 1 and 2.",
            v
        )),
    }
}

/// Validate `git.tag_sort` if present. Accepted values:
/// - `"-version:refname"` (default, lexicographic version sort)
/// - `"-version:creatordate"` (sort by tag creation date, newest first)
/// - `"semver"` (Rust-side strict SemVer 2.0.0 ordering, prereleases sort
///   below their release per spec section 11)
/// - `"smartsemver"` (same ordering as `semver`, but when the current version
///   is non-prerelease, prerelease tags are skipped when picking the previous
///   tag — avoids selecting `v0.2.0-beta.3` as the predecessor of `v0.2.0`)
///
/// Returns an error for unrecognized values.
pub fn validate_tag_sort(config: &Config) -> Result<(), String> {
    if let Some(ref git) = config.git
        && let Some(ref sort) = git.tag_sort
    {
        match sort.as_str() {
            "-version:refname" | "-version:creatordate" | "semver" | "smartsemver" => {}
            other => {
                return Err(format!(
                    "unsupported git.tag_sort value: \"{}\". \
                     Accepted values: \"-version:refname\", \"-version:creatordate\", \
                     \"semver\", \"smartsemver\".",
                    other
                ));
            }
        }
    }
    Ok(())
}

/// Validate `partial.by` up front so a stale value is rejected at config-load
/// time regardless of which target-resolution path runs.
///
/// `partial.by` is read in two unrelated places: the host-detection branch of
/// [`crate::partial::resolve_partial_target`] (which already rejects unknown
/// values) and the split-matrix generator (which treats anything that is not
/// `"os"` as `"target"`). Those two readers disagree on an out-of-set value
/// like the pre-rename `"goos"`: one errors, the other silently mis-groups the
/// matrix. Centralising the check means a typo fails loudly once, before
/// either reader can diverge.
pub fn validate_partial(config: &Config) -> Result<(), String> {
    if let Some(ref partial) = config.partial
        && let Some(ref by) = partial.by
    {
        match by.as_str() {
            "os" | "target" => {}
            other => {
                return Err(format!(
                    "unsupported partial.by value: \"{}\". \
                     Accepted values: \"os\", \"target\".",
                    other
                ));
            }
        }
    }
    Ok(())
}

/// Known OS values accepted by `archives[].format_overrides[].os`.
/// The Go runtime's `runtime.GOOS` values the archive pipe
/// recognises; anything outside this set is almost always a typo
/// (e.g. a Rust target triple slice like `pc-windows-msvc`).
const KNOWN_OS: &[&str] = &[
    "aix",
    "android",
    "darwin",
    "dragonfly",
    "freebsd",
    "illumos",
    "ios",
    "js",
    "linux",
    "netbsd",
    "openbsd",
    "plan9",
    "solaris",
    "wasip1",
    "windows",
];

/// Validate that each crate's `release:` block configures at most one SCM
/// backend. A multiple-releases error, which
/// errors at `Default()` time. Anodizer dispatches on `ctx.token_type` at
/// runtime so a silently-ignored extra backend is easy to miss.
pub fn validate_release_backends(config: &Config) -> Result<(), String> {
    let check = |crate_name: &str, release: &ReleaseConfig| -> Result<(), String> {
        let mut set = Vec::new();
        if release.github.is_some() {
            set.push("github");
        }
        if release.gitlab.is_some() {
            set.push("gitlab");
        }
        if release.gitea.is_some() {
            set.push("gitea");
        }
        if set.len() > 1 {
            return Err(format!(
                "crate {}: release config sets multiple mutually-exclusive SCM \
                 backends ({}). Pick one.",
                crate_name,
                set.join(" + ")
            ));
        }
        Ok(())
    };
    for krate in &config.crates {
        if let Some(ref release) = krate.release {
            check(&krate.name, release)?;
        }
    }
    if let Some(ws_list) = config.workspaces.as_ref() {
        for ws in ws_list {
            for krate in &ws.crates {
                if let Some(ref release) = krate.release {
                    check(&krate.name, release)?;
                }
            }
        }
    }
    Ok(())
}

/// Validate that `release.on_failure` is set only at the root.
///
/// The failure policy is one process-wide decision per run, resolved
/// from the top-level `release:` block alone. Crate-level `release:`
/// blocks share the `ReleaseConfig` struct, so the field parses there
/// — but it would never be read; rejecting the misplacement at config
/// load keeps a policy choice from being silently ignored.
pub fn validate_on_failure_root_only(config: &Config) -> Result<(), String> {
    // Deliberately raw (not `crate_universe()`): validation must flag every
    // entry as written, including a workspace entry the dedup would shadow —
    // a policy violation on a shadowed crate is still a config mistake.
    let mut offenders: Vec<&str> = config
        .crates
        .iter()
        .chain(
            config
                .workspaces
                .iter()
                .flatten()
                .flat_map(|ws| ws.crates.iter()),
        )
        .filter(|c| c.release.as_ref().is_some_and(|r| r.on_failure.is_some()))
        .map(|c| c.name.as_str())
        .collect();
    offenders.sort_unstable();
    offenders.dedup();
    if offenders.is_empty() {
        return Ok(());
    }
    Err(format!(
        "release.on_failure is a root-level policy and cannot be set per crate \
         (set on: {}). Move it to the top-level `release:` block.",
        offenders.join(", ")
    ))
}

/// Marker prefix for the axis-mismatch validation error class. Existing
/// validators in this module return `Result<(), String>` rather than a
/// typed enum, so we expose this constant (instead of a `ConfigError`
/// variant) for callers that want to recognise the error class
/// programmatically.
///
/// The prefix is emitted at the start of every error returned by
/// [`validate_defaults_axis`] (formatted as `"DefaultsAxisMismatch: …"`),
/// so callers can match with `err.starts_with(ERR_DEFAULTS_AXIS_MISMATCH)`
/// or `err.contains(ERR_DEFAULTS_AXIS_MISMATCH)` without depending on the
/// exact human-readable wording.
///
/// ```ignore
/// match validate_defaults_axis(&config) {
///     Err(e) if e.starts_with(ERR_DEFAULTS_AXIS_MISMATCH) => {
///         // handle the axis-mismatch error class
///     }
///     other => other?,
/// }
/// ```
///
/// Future error-type unification can rename to
/// `ConfigError::DefaultsAxisMismatch` without changing call-sites that
/// match on this prefix.
pub const ERR_DEFAULTS_AXIS_MISMATCH: &str = "DefaultsAxisMismatch";

/// Validate that `defaults.crates:` and `defaults.workspaces:` match the
/// top-level axis.
///
/// Rules:
/// - `defaults.crates:` is set → top-level `crates:` MUST be present.
/// - `defaults.workspaces:` is set → top-level `workspaces:` MUST be present.
/// - Both `defaults.crates` and `defaults.workspaces` set simultaneously → error
///   (mutually exclusive).
/// - Wrong-axis (e.g. `defaults.crates:` while top-level uses `workspaces:`) → error.
pub fn validate_defaults_axis(config: &Config) -> Result<(), String> {
    let Some(ref defaults) = config.defaults else {
        return Ok(());
    };
    let has_crate_block = defaults.crates.is_some();
    let has_workspace_block = defaults.workspaces.is_some();

    if has_crate_block && has_workspace_block {
        return Err(format!(
            "{ERR_DEFAULTS_AXIS_MISMATCH}: defaults.crates and defaults.workspaces are \
             mutually exclusive — pick the axis that matches the top-level config \
             (`crates:` or `workspaces:`)",
        ));
    }

    let top_uses_workspaces = config.workspaces.as_ref().is_some_and(|w| !w.is_empty());
    let top_uses_crates = !config.crates.is_empty();

    if has_crate_block && !top_uses_crates {
        return Err(format!(
            "{ERR_DEFAULTS_AXIS_MISMATCH}: defaults.crates is set but top-level `crates:` \
             is {}; move defaults under `defaults.workspaces:` or remove the block",
            if top_uses_workspaces {
                "absent (top-level uses `workspaces:`)"
            } else {
                "absent"
            },
        ));
    }
    if has_workspace_block && !top_uses_workspaces {
        return Err(format!(
            "{ERR_DEFAULTS_AXIS_MISMATCH}: defaults.workspaces is set but top-level \
             `workspaces:` is {}; move defaults under `defaults.crates:` or remove the block",
            if top_uses_crates {
                "absent (top-level uses `crates:`)"
            } else {
                "absent"
            },
        ));
    }

    Ok(())
}

/// Validate `archives[].format_overrides[].os` values reject unknown OSes.
/// Silently no-op-ing unknown overrides has burned users typing
/// Rust triples like `apple` or `pc-windows-msvc`.
///
/// Walks every `archives[]` location in the config:
/// - `crates[].archives:`
/// - `workspaces[].crates[].archives:`
/// - `defaults.archives:` (an unknown `os` here would otherwise pass silently
///   and propagate to every inheriting crate at merge time).
pub fn validate_format_overrides(config: &Config) -> Result<(), String> {
    let check = |location: &str, archives: &[ArchiveConfig]| -> Result<(), String> {
        for (idx, archive) in archives.iter().enumerate() {
            let Some(ref overrides) = archive.format_overrides else {
                continue;
            };
            for over in overrides {
                if !KNOWN_OS.contains(&over.os.as_str()) {
                    let archive_id = archive.id.as_deref().unwrap_or("default");
                    return Err(format!(
                        "{}: archives[{}] (id={}): format_overrides.os=\"{}\" is not a recognised OS. \
                         Accepted values: {}.",
                        location,
                        idx,
                        archive_id,
                        over.os,
                        KNOWN_OS.join(", ")
                    ));
                }
            }
        }
        Ok(())
    };
    for krate in &config.crates {
        if let ArchivesConfig::Configs(ref list) = krate.archives {
            check(&format!("crate {}", krate.name), list)?;
        }
    }
    if let Some(ws_list) = config.workspaces.as_ref() {
        for ws in ws_list {
            for krate in &ws.crates {
                if let ArchivesConfig::Configs(ref list) = krate.archives {
                    check(&format!("crate {}", krate.name), list)?;
                }
            }
        }
    }
    if let Some(ref defaults) = config.defaults
        && let Some(ref archive) = defaults.archives
    {
        // defaults.archives is a single ArchiveConfig (not a list); wrap it
        // into a one-element slice so the same checker walks it.
        check("defaults.archives", std::slice::from_ref(archive))?;
    }
    Ok(())
}

/// Validate that no [`HomebrewCaskConfig`] sets both `url_template` AND
/// `url.template` simultaneously — they are mutually exclusive shorthands
/// for the same URL field and combining them is ambiguous.
///
/// Inspects every occurrence of `HomebrewCaskConfig` in the config:
/// - `homebrew_casks:` (top-level array)
/// - `crates[].publish.homebrew_cask:`
/// - `workspaces[].crates[].publish.homebrew_cask:`
/// - `defaults.publish.homebrew_cask:`
pub fn validate_homebrew_cask_url_template(config: &Config) -> Result<(), String> {
    let check = |location: &str, cask: &HomebrewCaskConfig| -> Result<(), String> {
        let has_url_template = cask.url_template.is_some();
        let has_url_dot_template = cask.url.as_ref().is_some_and(|u| u.template.is_some());
        if has_url_template && has_url_dot_template {
            return Err(format!(
                "{location}: homebrew_cask sets both `url_template` and `url.template`. \
                 These are mutually exclusive — use one or the other."
            ));
        }
        Ok(())
    };

    // Top-level homebrew_casks list (not nested under publish:) — not a
    // publish axis, so it is scanned separately from the visitor.
    if let Some(ref casks) = config.homebrew_casks {
        for (i, cask) in casks.iter().enumerate() {
            check(&format!("homebrew_casks[{i}]"), cask)?;
        }
    }

    try_for_each_crate_publish(config, |axis, publish| {
        if let Some(cask) = publish.homebrew_cask() {
            check(&axis.homebrew_cask_location(), cask)?;
        }
        Ok(())
    })
}

/// Allowed `winget.upgrade_behavior` values, mirroring the winget installer
/// manifest schema (1.12.0) `UpgradeBehavior` enum. A value outside this set
/// renders an installer manifest the winget validator rejects at PR time —
/// catch it at config-validate instead.
pub const WINGET_UPGRADE_BEHAVIORS: [&str; 3] = ["install", "uninstallPrevious", "deny"];

/// Validate that every configured `winget.upgrade_behavior` is one of the
/// winget-recognized values ([`WINGET_UPGRADE_BEHAVIORS`]). Walks the per-crate,
/// per-workspace, and `defaults.publish` axes.
pub fn validate_winget_upgrade_behavior(config: &Config) -> Result<(), String> {
    let check = |location: &str, winget: &WingetConfig| -> Result<(), String> {
        if let Some(ref behavior) = winget.upgrade_behavior
            && !WINGET_UPGRADE_BEHAVIORS.contains(&behavior.as_str())
        {
            return Err(format!(
                "{location}: upgrade_behavior `{behavior}` is not a valid winget value. \
                 Use one of: {}.",
                WINGET_UPGRADE_BEHAVIORS.join(", ")
            ));
        }
        Ok(())
    };

    try_for_each_crate_publish(config, |axis, publish| {
        if let Some(winget) = publish.winget() {
            check(&axis.winget_location(), winget)?;
        }
        Ok(())
    })
}

/// Validate that every `winget.dependencies[].architectures` entry names a
/// recognized WinGet architecture ([`WINGET_ARCHITECTURES`]). Walks the
/// per-crate, per-workspace, and `defaults.publish` axes.
///
/// The per-installer dependency emitter matches a scope value against each
/// installer's WinGet architecture by exact, case-sensitive equality. A value
/// outside the canonical set ([`WINGET_ARCHITECTURES`]: `x64`, `arm64`, `x86`)
/// therefore matches
/// no installer, so the dependency would silently disappear from the generated
/// manifest. Reject it at config-validate instead of shipping a manifest that
/// quietly omits a declared dependency. An empty list (or absent
/// `architectures`) means "all installers" and is valid.
pub fn validate_winget_dependency_architectures(config: &Config) -> Result<(), String> {
    let check = |location: &str, winget: &WingetConfig| -> Result<(), String> {
        let Some(ref deps) = winget.dependencies else {
            return Ok(());
        };
        for (i, dep) in deps.iter().enumerate() {
            let Some(ref scopes) = dep.architectures else {
                continue;
            };
            for scope in scopes {
                if !WINGET_ARCHITECTURES.contains(&scope.as_str()) {
                    return Err(format!(
                        "{location}: dependencies[{i}].architectures contains `{scope}`, \
                         which is not a valid winget architecture. Use one of: {} \
                         (or leave architectures empty/unset to apply the dependency \
                         to every installer).",
                        WINGET_ARCHITECTURES.join(", ")
                    ));
                }
            }
        }
        Ok(())
    };

    try_for_each_crate_publish(config, |axis, publish| {
        if let Some(winget) = publish.winget() {
            check(&axis.winget_location(), winget)?;
        }
        Ok(())
    })
}

/// Validate that `archives[].id` and `universal_binaries[].id` are unique
/// within their respective lists.
///
/// The id-uniqueness validation for archives and universal binaries.
/// Two archive
/// configs with the same `id` silently both set the same `id` metadata key
/// on artifacts, breaking publishers that filter `ids: [<id>]`. Anodizer's
/// build/sign stages already enforce id uniqueness; archive and
/// universal_binary were missed.
///
/// Walks every occurrence of `archives[]` and `universal_binaries[]`:
/// - `crates[].archives:` / `crates[].universal_binaries:`
/// - `workspaces[].crates[].archives:` / `.universal_binaries:`
/// - `defaults.archives:` is a single `ArchiveConfig`, so uniqueness within
///   itself is vacuously true; not walked here.
///
pub fn validate_id_uniqueness(config: &Config) -> Result<(), String> {
    fn check_unique(
        location: &str,
        kind: &str,
        ids: impl IntoIterator<Item = (usize, Option<String>)>,
    ) -> Result<(), String> {
        let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for (idx, maybe_id) in ids {
            // Empty is stored as "default" for archives via Default-time
            // assignment. Anodizer applies `default_archive_id` at deserialize
            // time, so the option is normally `Some("default")`. A truly empty
            // / None id here means the user explicitly cleared it; we still
            // dedupe across `None` so two None-id'd entries collide just like
            // two "default"-id'd entries would.
            let key = maybe_id.unwrap_or_else(|| "<unset>".to_string());
            if let Some(prev_idx) = seen.insert(key.clone(), idx) {
                return Err(format!(
                    "{location}: {kind} id \"{key}\" is used by both entry {prev_idx} and entry {idx} — \
                     ids must be unique within a {kind} list."
                ));
            }
        }
        Ok(())
    }

    let check_archives = |location: &str, archives: &[ArchiveConfig]| -> Result<(), String> {
        check_unique(
            location,
            "archives",
            archives.iter().enumerate().map(|(i, a)| (i, a.id.clone())),
        )
    };
    let check_unibins = |location: &str, ubs: &[UniversalBinaryConfig]| -> Result<(), String> {
        check_unique(
            location,
            "universal_binaries",
            ubs.iter().enumerate().map(|(i, u)| (i, u.id.clone())),
        )
    };

    for krate in &config.crates {
        if let ArchivesConfig::Configs(ref list) = krate.archives {
            check_archives(&format!("crates[{}].archives", krate.name), list)?;
        }
        if let Some(ref ubs) = krate.universal_binaries {
            check_unibins(&format!("crates[{}].universal_binaries", krate.name), ubs)?;
        }
    }
    if let Some(ws_list) = config.workspaces.as_ref() {
        for ws in ws_list {
            for krate in &ws.crates {
                if let ArchivesConfig::Configs(ref list) = krate.archives {
                    check_archives(
                        &format!("workspaces[{}].crates[{}].archives", ws.name, krate.name),
                        list,
                    )?;
                }
                if let Some(ref ubs) = krate.universal_binaries {
                    check_unibins(
                        &format!(
                            "workspaces[{}].crates[{}].universal_binaries",
                            ws.name, krate.name
                        ),
                        ubs,
                    )?;
                }
            }
        }
    }
    Ok(())
}

/// Validate `builds[]` entries that opt into `builder: prebuilt`.
///
/// `builder: prebuilt` skips `cargo build` and imports a binary the
/// operator staged elsewhere. The validation rules below follow the
/// `prebuilt` builder contract (`/customization/builds/builders/prebuilt.md`):
///
/// 1. `prebuilt:` block MUST be set and `prebuilt.path` MUST be non-empty.
/// 2. `targets:` MUST be explicit on the build entry — no `defaults.targets`
///    fallback. Without this rule the build matrix has no rows.
/// 3. Cargo-only knobs are rejected as mutually exclusive: `cross_tool`,
///    `features`, `no_default_features`, `command`. The crate-level
///    `cross:` strategy is also rejected when any build on the crate is
///    prebuilt (the strategy has no meaning when nothing is being
///    compiled).
/// 4. `builder: cargo` (the default) with a `prebuilt:` block set warns —
///    the block has no effect and likely indicates a forgotten
///    `builder: prebuilt`.
pub fn validate_builds(config: &Config) -> Result<(), String> {
    let check_crate = |location: &str, krate: &CrateConfig| -> Result<(), String> {
        let Some(ref builds) = krate.builds else {
            return Ok(());
        };
        let crate_is_prebuilt = builds
            .iter()
            .any(|b| matches!(b.builder, Some(BuilderKind::Prebuilt)));
        if crate_is_prebuilt && krate.cross.is_some() {
            return Err(format!(
                "{location}: crate-level `cross:` strategy is set but at least one \
                 build uses `builder: prebuilt`; remove `cross:` (prebuilt imports a \
                 binary instead of compiling) or change the build's builder to `cargo`."
            ));
        }
        for (idx, build) in builds.iter().enumerate() {
            match build.builder {
                Some(BuilderKind::Prebuilt) => {
                    let path = build.prebuilt.as_ref().map(|p| p.path.trim()).unwrap_or("");
                    if path.is_empty() {
                        return Err(format!(
                            "{location}.builds[{idx}]: `builder: prebuilt` requires a non-empty \
                             `prebuilt.path` template. Example: \
                             `prebuilt: {{ path: \"output/mybin_{{{{ .Target }}}}\" }}`"
                        ));
                    }
                    let targets_explicit = build.targets.as_ref().is_some_and(|t| !t.is_empty());
                    if !targets_explicit {
                        return Err(format!(
                            "{location}.builds[{idx}] has `builder: prebuilt` but no explicit \
                             `targets:` — the prebuilt builder requires per-build target triples \
                             (no `defaults.targets:` fallback). Add `targets: [<triple>, ...]`."
                        ));
                    }
                    if build.cross_tool.as_ref().is_some_and(|s| !s.is_empty()) {
                        return Err(format!(
                            "{location}.builds[{idx}]: `cross_tool` is set with \
                             `builder: prebuilt` — the two are mutually exclusive. \
                             `cross_tool` controls how cargo cross-compiles; `prebuilt` \
                             imports an already-built binary. Drop `cross_tool` or use \
                             `builder: cargo`."
                        ));
                    }
                    if build.command.as_ref().is_some_and(|s| !s.is_empty()) {
                        return Err(format!(
                            "{location}.builds[{idx}]: `command:` override is set with \
                             `builder: prebuilt` — the override selects the cargo \
                             subcommand, which is not invoked under the prebuilt \
                             builder. Drop `command:` or use `builder: cargo`."
                        ));
                    }
                    if build.features.as_ref().is_some_and(|f| !f.is_empty()) {
                        return Err(format!(
                            "{location}.builds[{idx}]: `features:` is set with \
                             `builder: prebuilt` — Cargo features are evaluated at \
                             compile time, which the prebuilt builder skips. \
                             Drop `features:` or use `builder: cargo`."
                        ));
                    }
                    if build.no_default_features.is_some() {
                        return Err(format!(
                            "{location}.builds[{idx}]: `no_default_features:` is set with \
                             `builder: prebuilt` — Cargo feature flags are evaluated at \
                             compile time, which the prebuilt builder skips. \
                             Drop the flag or use `builder: cargo`."
                        ));
                    }
                }
                Some(BuilderKind::Cargo) | None => {
                    if build.prebuilt.is_some() {
                        tracing::warn!(
                            "{location}: build[{idx}] has a `prebuilt:` block but `builder:` \
                             is not `prebuilt`; the block is ignored. Set `builder: prebuilt` \
                             or remove the block."
                        );
                    }
                }
            }
        }
        Ok(())
    };

    for krate in &config.crates {
        check_crate(&format!("crates[{}]", krate.name), krate)?;
    }
    if let Some(ws_list) = config.workspaces.as_ref() {
        for ws in ws_list {
            for krate in &ws.crates {
                check_crate(
                    &format!("workspaces[{}].crates[{}]", ws.name, krate.name),
                    krate,
                )?;
            }
        }
    }
    Ok(())
}

/// Returns `true` if every build entry on every crate has
/// `builder: prebuilt`. Used by the determinism harness to short-circuit:
/// when no target compiles, there is nothing for the harness to rebuild
/// and compare across runs.
pub fn all_builds_prebuilt(config: &Config) -> bool {
    let crate_all_prebuilt = |krate: &CrateConfig| -> Option<bool> {
        let builds = krate.builds.as_ref()?;
        if builds.is_empty() {
            return None;
        }
        Some(
            builds
                .iter()
                .all(|b| matches!(b.builder, Some(BuilderKind::Prebuilt))),
        )
    };

    let mut saw_any = false;
    for krate in config.crate_universe() {
        match crate_all_prebuilt(krate) {
            Some(true) => saw_any = true,
            Some(false) => return false,
            None => {}
        }
    }
    saw_any
}

/// Validate the depth of `changelog.groups[].groups`.
///
/// Subgroups are capped at ONE level
/// (`/customization/publish/changelog.md`: "There can only be one level of
/// subgroups"). Anodizer's renderer can technically handle deeper nesting
/// (capped at 6 to match Markdown's heading limit), but accepting deeper
/// configs silently is a footgun: a config that works in anodizer but is
/// rejected here breaks parity for users migrating in.
///
/// Rejects any `changelog.groups[i].groups[j].groups[..]` configuration
/// with a clear error pointing at the offending parent group title.
pub fn validate_changelog_groups_depth(config: &Config) -> Result<(), String> {
    let check = |location: &str, cfg: &ChangelogConfig| -> Result<(), String> {
        let Some(ref groups) = cfg.groups else {
            return Ok(());
        };
        for g in groups {
            if let Some(ref subs) = g.groups {
                for sub in subs {
                    if sub.groups.as_ref().is_some_and(|s| !s.is_empty()) {
                        return Err(format!(
                            "{location}: changelog group '{}' > '{}' nests further \
                             subgroups; GoReleaser permits only one level of subgroups \
                             (see https://goreleaser.com/customization/changelog/). \
                             Flatten the inner groups into the parent or split into \
                             sibling top-level groups.",
                            g.title, sub.title
                        ));
                    }
                }
            }
        }
        Ok(())
    };
    if let Some(ref cfg) = config.changelog {
        check("changelog", cfg)?;
    }
    if let Some(ref ws_list) = config.workspaces {
        for ws in ws_list {
            if let Some(ref cfg) = ws.changelog {
                check(&format!("workspaces[{}].changelog", ws.name), cfg)?;
            }
        }
    }
    Ok(())
}

/// Validate `changelog.paths[]` syntax.
///
/// Path patterns are passed straight to `git log -- <path>` (or the
/// per-SCM equivalent). Two patterns are always wrong:
/// - Leading `/` — git pathspec treats this as anchored-to-CWD which is
///   almost never what the user wrote and produces empty changelogs.
/// - Empty string — silently matches everything; rejected so a typo
///   doesn't disable filtering.
///
/// Globs containing `**` are accepted (git accepts them) but the docs
/// note their semantics differ from gitignore; that's a docs concern,
/// not a hard error.
pub fn validate_changelog_paths(config: &Config) -> Result<(), String> {
    let check = |location: &str, cfg: &ChangelogConfig| -> Result<(), String> {
        let Some(ref paths) = cfg.paths else {
            return Ok(());
        };
        for (idx, p) in paths.iter().enumerate() {
            if p.is_empty() {
                return Err(format!(
                    "{location}: changelog.paths[{idx}] is empty; remove the entry \
                     or set a real path (empty string matches everything and \
                     disables filtering)"
                ));
            }
            if p.starts_with('/') {
                return Err(format!(
                    "{location}: changelog.paths[{idx}] = {:?} starts with '/'; \
                     git pathspec is repo-root-relative — write {:?} instead",
                    p,
                    p.trim_start_matches('/')
                ));
            }
        }
        Ok(())
    };
    if let Some(ref cfg) = config.changelog {
        check("changelog", cfg)?;
    }
    if let Some(ref ws_list) = config.workspaces {
        for ws in ws_list {
            if let Some(ref cfg) = ws.changelog {
                check(&format!("workspaces[{}].changelog", ws.name), cfg)?;
            }
        }
    }
    Ok(())
}

/// Validate every upload-destination `exclude:` glob across all config axes.
///
/// `exclude:` drops artifacts whose file name matches a glob (see
/// [`crate::artifact::passes_exclude_filter`]). An unparseable glob is treated
/// as non-matching at runtime so it never crashes a release — but a typo'd
/// glob that silently keeps an asset (or, worse, drops every asset) is a
/// foot-gun. Reject malformed globs here, at config-load, with a clear message
/// before they can take effect.
///
/// Covers every config position where `exclude:` is settable: per-crate
/// `release:` and `blobs:` (top-level crates AND `workspaces[].crates[]`), the
/// top-level `artifactories:`, `cloudsmiths:`, `gemfury:`, and `uploads:`
/// lists, and the top-level shared `release:` block.
pub fn validate_exclude_globs(config: &Config) -> Result<(), String> {
    fn check(location: &str, exclude: Option<&[String]>) -> Result<(), String> {
        let Some(globs) = exclude else {
            return Ok(());
        };
        for (idx, g) in globs.iter().enumerate() {
            if g.is_empty() {
                return Err(format!(
                    "{location}: exclude[{idx}] is empty; remove the entry or set a \
                     real glob (an empty pattern matches nothing and is a no-op)"
                ));
            }
            if let Err(e) = glob::Pattern::new(g) {
                return Err(format!(
                    "{location}: exclude[{idx}] = {g:?} is not a valid glob: {e}"
                ));
            }
        }
        Ok(())
    }

    let check_crate = |location: &str, krate: &CrateConfig| -> Result<(), String> {
        if let Some(ref release) = krate.release {
            check(&format!("{location}.release"), release.exclude.as_deref())?;
        }
        if let Some(ref blobs) = krate.blobs {
            for (i, b) in blobs.iter().enumerate() {
                check(&format!("{location}.blobs[{i}]"), b.exclude.as_deref())?;
            }
        }
        Ok(())
    };

    for krate in &config.crates {
        check_crate(&format!("crates[{}]", krate.name), krate)?;
    }
    if let Some(ref ws_list) = config.workspaces {
        for ws in ws_list {
            for krate in &ws.crates {
                check_crate(
                    &format!("workspaces[{}].crates[{}]", ws.name, krate.name),
                    krate,
                )?;
            }
        }
    }
    if let Some(ref list) = config.artifactories {
        for (i, a) in list.iter().enumerate() {
            check(&format!("artifactories[{i}]"), a.exclude.as_deref())?;
        }
    }
    if let Some(ref list) = config.cloudsmiths {
        for (i, c) in list.iter().enumerate() {
            check(&format!("cloudsmiths[{i}]"), c.exclude.as_deref())?;
        }
    }
    if let Some(ref list) = config.gemfury {
        for (i, g) in list.iter().enumerate() {
            check(&format!("gemfury[{i}]"), g.exclude.as_deref())?;
        }
    }
    if let Some(ref list) = config.uploads {
        for (i, u) in list.iter().enumerate() {
            check(&format!("uploads[{i}]"), u.exclude.as_deref())?;
        }
    }
    if let Some(ref release) = config.release {
        check("release", release.exclude.as_deref())?;
    }
    Ok(())
}
