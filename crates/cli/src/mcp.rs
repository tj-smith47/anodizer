//! The MCP tool surface anodizer exposes through brontes.
//!
//! Lives outside `main.rs` because the annotation, grouping and task-mode
//! tables are per-command data rather than control flow.

use brontes::{Config, TaskMode, ToolAnnotations};

/// Commands that read the world and write nothing, so an agent may call them
/// without a confirmation prompt.
const READ_ONLY: &[&str] = &[
    "check config",
    "check version-files",
    "healthcheck",
    "jsonschema",
    "resolve-tag",
    "targets",
    "tools",
    "vocabulary",
];

/// Read-only commands that additionally reach the network — registries,
/// endpoints, the docker daemon.
const READ_ONLY_NETWORKED: &[&str] = &["preflight"];

/// Commands that write locally (dist/, CHANGELOG.md, Cargo.toml, git tags)
/// but publish nothing. Re-running any of them converges rather than
/// stacking, so each is idempotent.
const LOCAL_WRITES: &[&str] = &["build", "changelog", "check determinism", "init", "tag"];

/// Commands that publish to registries, package managers or chat. Every one
/// is externally visible the moment it succeeds.
const PUBLISHING: &[&str] = &[
    "announce", "continue", "notify", "promote", "publish", "release",
];

/// Commands that routinely run for minutes. Handed back as task handles so a
/// client that speaks the tasks extension can poll, stream progress and
/// cancel instead of holding a request open for the whole pipeline.
const LONG_RUNNING: &[&str] = &[
    "build",
    "check determinism",
    "continue",
    "promote",
    "publish",
    "release",
];

/// Build the brontes configuration for anodizer's `mcp` subtree.
///
/// The tool list is anodizer's own clap tree minus the two commands that only
/// make sense at a shell prompt (`completion`, `man`), with hints attached so
/// a client can tell a config validation apart from a publish.
pub fn config() -> Config {
    let mut cfg = Config::default().tool_name_prefix("anodizer");

    // The bare root prints help and exits. Annotated on its own rather than
    // through READ_ONLY because group membership covers a path's descendants,
    // and the root's descendants are the entire tree.
    cfg = cfg.annotation(
        "anodizer",
        ToolAnnotations {
            read_only_hint: Some(true),
            open_world_hint: Some(false),
            ..Default::default()
        },
    );

    for path in READ_ONLY {
        cfg = cfg.annotation(
            *path,
            ToolAnnotations {
                read_only_hint: Some(true),
                open_world_hint: Some(false),
                ..Default::default()
            },
        );
    }
    for path in READ_ONLY_NETWORKED {
        cfg = cfg.annotation(
            *path,
            ToolAnnotations {
                read_only_hint: Some(true),
                open_world_hint: Some(true),
                ..Default::default()
            },
        );
    }
    for path in LOCAL_WRITES {
        cfg = cfg.annotation(
            *path,
            ToolAnnotations {
                read_only_hint: Some(false),
                destructive_hint: Some(false),
                idempotent_hint: Some(true),
                open_world_hint: Some(false),
                ..Default::default()
            },
        );
    }
    for path in PUBLISHING {
        cfg = cfg.annotation(
            *path,
            ToolAnnotations {
                read_only_hint: Some(false),
                destructive_hint: Some(false),
                idempotent_hint: Some(true),
                open_world_hint: Some(true),
                ..Default::default()
            },
        );
    }

    // The one command that removes published state: it unwinds publishers,
    // deletes tags and rewrites history.
    cfg = cfg.annotation(
        "tag rollback",
        ToolAnnotations {
            read_only_hint: Some(false),
            destructive_hint: Some(true),
            idempotent_hint: Some(false),
            open_world_hint: Some(true),
            ..Default::default()
        },
    );
    // Version bumps stack: calling it twice moves the version twice.
    cfg = cfg.annotation(
        "bump",
        ToolAnnotations {
            read_only_hint: Some(false),
            destructive_hint: Some(false),
            idempotent_hint: Some(false),
            open_world_hint: Some(false),
            ..Default::default()
        },
    );

    for path in LONG_RUNNING {
        cfg = cfg.task_mode_for(*path, TaskMode::Detached);
    }

    // `preflight`'s clap `about` is a paragraph aimed at a human reading
    // `--help`; the tool list wants the one-line contract.
    cfg = cfg
        .description(
            "preflight",
            "Verify the environment can run the configured release — required tools, \
             env vars and secrets (presence only), endpoint reachability, docker, key \
             material — and report every failure in one pass. Also prints the \
             per-publisher reconcile table. Runs automatically at the start of `release`.",
        )
        .description(
            "release",
            "Run the full release pipeline. Re-running the identical command converges \
             on already-published state instead of double-publishing, so a re-run is \
             how a failed release is recovered.",
        );

    // Shell completions and man pages are shell-prompt artifacts; as tools
    // they only add tokens to every tools/list.
    cfg = cfg.hide_command("completion").hide_command("man");

    cfg.group(
        "inspect",
        READ_ONLY.iter().chain(READ_ONLY_NETWORKED).copied(),
    )
    .group_description(
        "inspect",
        "Read-only: validate config, resolve tags, list targets and tools, probe the environment",
    )
    .group("author", ["init", "changelog", "bump", "tag"])
    .group_description(
        "author",
        "Local edits: scaffold config, refresh the changelog, bump versions, cut tags",
    )
    .group(
        "ship",
        PUBLISHING.iter().copied().chain(std::iter::once("build")),
    )
    .group_description(
        "ship",
        "Build artifacts and publish them to registries, package managers and chat",
    )
}
