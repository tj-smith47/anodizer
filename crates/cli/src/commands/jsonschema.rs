use anyhow::Result;

/// Output JSON Schema for the `.anodizer.yaml` config file.
pub fn run() -> Result<()> {
    let schema = anodizer_core::config::config_schema();
    let json = serde_json::to_string_pretty(&schema)?;
    println!("{json}");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_jsonschema_generates_valid_json() {
        let schema = anodizer_core::config::config_schema();
        let json = serde_json::to_string_pretty(&schema);
        assert!(json.is_ok(), "schema should serialize to JSON");
        let json_str = json.unwrap();
        assert!(
            json_str.contains("\"type\""),
            "schema JSON should contain type definitions"
        );
        assert!(
            json_str.contains("project_name"),
            "schema should reference project_name field"
        );
    }

    #[test]
    fn test_jsonschema_contains_new_fields() {
        let schema = anodizer_core::config::config_schema();
        let json = serde_json::to_string_pretty(&schema).unwrap();
        assert!(
            json.contains("env_files"),
            "schema should contain env_files field"
        );
        assert!(
            json.contains("version"),
            "schema should contain version field"
        );
    }
}
