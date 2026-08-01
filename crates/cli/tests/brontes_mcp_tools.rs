//! Dogfood test: anodizer's CLI tree must produce a valid brontes tool list
//! under the configuration the binary actually ships.
//!
//! `generate_tools` rejects a config path that names no walked command, so
//! this test also pins every command path in `anodizer_cli::mcp` against the
//! clap tree — rename a subcommand without updating the table and it fails.
use anodizer_cli::{Cli, mcp};
use clap::CommandFactory;

/// Tool names the shipped config generates, in list order.
fn tool_names() -> Vec<String> {
    brontes::generate_tools(&Cli::command(), &mcp::config())
        .expect("anodizer CLI must produce a valid brontes tool list")
        .iter()
        .map(|t| t.name.to_string())
        .collect()
}

/// The `(read_only, destructive, open_world)` hints attached to one tool.
fn hints(name: &str) -> (Option<bool>, Option<bool>, Option<bool>) {
    let tools = brontes::generate_tools(&Cli::command(), &mcp::config())
        .expect("anodizer CLI must produce a valid brontes tool list");
    let tool = tools
        .iter()
        .find(|t| t.name.as_ref() == name)
        .unwrap_or_else(|| {
            let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
            panic!("expected tool {name} in the generated list, got: {names:?}")
        });
    let ann = tool
        .annotations
        .as_ref()
        .unwrap_or_else(|| panic!("{name} must carry annotations"));
    (
        ann.read_only_hint,
        ann.destructive_hint,
        ann.open_world_hint,
    )
}

#[test]
fn anodizer_cli_produces_valid_brontes_tool_list() {
    let names = tool_names();
    assert!(!names.is_empty(), "expected at least one tool");
    for name in &names {
        assert!(
            name == "anodizer" || name.starts_with("anodizer_"),
            "tool name must start with anodizer prefix, got {name}"
        );
    }
}

#[test]
fn every_tool_carries_a_safety_hint() {
    // A client decides whether to prompt from `readOnlyHint`, so a command
    // added to the CLI without a row in one of the classification tables is a
    // tool the client has to guess about. Fail here instead.
    let tools = brontes::generate_tools(&Cli::command(), &mcp::config())
        .expect("anodizer CLI must produce a valid brontes tool list");

    let unclassified: Vec<&str> = tools
        .iter()
        .filter(|t| {
            t.annotations
                .as_ref()
                .and_then(|a| a.read_only_hint)
                .is_none()
        })
        .map(|t| t.name.as_ref())
        .collect();

    assert!(
        unclassified.is_empty(),
        "every tool needs a readOnlyHint; add these to a table in anodizer_cli::mcp: {unclassified:?}"
    );
}

#[test]
fn read_only_and_publishing_commands_carry_distinct_hints() {
    assert_eq!(
        hints("anodizer_check_config").0,
        Some(true),
        "validating config writes nothing"
    );

    let (read_only, _, open_world) = hints("anodizer_release");
    assert_eq!(read_only, Some(false));
    assert_eq!(
        open_world,
        Some(true),
        "the release pipeline reaches registries and package managers"
    );

    assert_eq!(
        hints("anodizer_tag_rollback").1,
        Some(true),
        "tag rollback is the one command that removes published state"
    );
}

#[test]
fn shell_prompt_commands_are_not_tools() {
    let names = tool_names();
    for hidden in ["anodizer_completion", "anodizer_man"] {
        assert!(
            !names.iter().any(|n| n == hidden),
            "{hidden} is a shell-prompt artifact and must stay out of the tool list, got: {names:?}"
        );
    }
}

#[test]
fn each_group_selects_a_coherent_subset() {
    let cfg = mcp::config();

    for group in ["inspect", "author", "ship"] {
        let tools = brontes::generate_tools(&Cli::command(), &cfg.clone().expose_group(group))
            .unwrap_or_else(|e| panic!("group {group} must resolve to a tool list: {e}"));
        assert!(
            !tools.is_empty(),
            "group {group} must select at least one tool"
        );
    }

    let inspect = brontes::generate_tools(&Cli::command(), &cfg.expose_group("inspect"))
        .expect("inspect group must resolve");
    let names: Vec<&str> = inspect.iter().map(|t| t.name.as_ref()).collect();
    assert!(
        !names.contains(&"anodizer_release"),
        "the read-only group must not expose the release pipeline, got: {names:?}"
    );
}
