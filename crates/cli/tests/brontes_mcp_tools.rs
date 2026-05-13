//! Dogfood test: anodizer's CLI tree must produce a valid brontes tool list.
use anodizer_cli::Cli;
use brontes::Config;
use clap::CommandFactory;

#[test]
fn anodizer_cli_produces_valid_brontes_tool_list() {
    let cmd = Cli::command();
    let cfg = Config::default().tool_name_prefix("anodizer");
    let tools = brontes::generate_tools(&cmd, &cfg)
        .expect("anodizer CLI must produce a valid brontes tool list");

    assert!(!tools.is_empty(), "expected at least one tool");
    for tool in &tools {
        let name = tool.name.as_ref();
        assert!(
            name == "anodizer" || name.starts_with("anodizer_"),
            "tool name must start with anodizer prefix, got {name}"
        );
    }
}
