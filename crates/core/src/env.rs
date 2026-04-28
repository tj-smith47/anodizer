//! `KEY=VAL` env-list helpers.
//!
//! Stages and publishers accept `env: Vec<String>` lists where each entry is a
//! `KEY=VALUE` string. The helpers in this module parse, split, and render
//! those entries into `(key, value)` pairs preserving declaration order so
//! chained env applications (sign + sbom + notarize, hook before/after) see
//! entries in the order the user wrote them.
//!
//! Companion to [`crate::env_expand`], which handles `$VAR` / `${VAR}`
//! shell-style expansion inside individual values.

use anyhow::Context as _;

/// Split a `KEY=VAL` env entry into `(key, value)`.
///
/// Returns the original entry as the error message when the line is missing
/// `=` or has an empty key. Used by stage code that needs to apply env entries
/// to a child process (sign, sbom, notarize, publishers).
pub fn split_env_entry(entry: &str) -> Result<(&str, &str), String> {
    let (k, v) = entry
        .split_once('=')
        .ok_or_else(|| format!("env entry must be KEY=VALUE, got: {entry:?}"))?;
    let key = k.trim();
    if key.is_empty() {
        return Err(format!("env entry has empty key: {entry:?}"));
    }
    Ok((key, v))
}

/// Parse a list of `KEY=VAL` env entries into ordered `(key, value)` pairs.
///
/// Order is preserved so chained env applications (sign + sbom + notarize)
/// see entries in user-declared order.
pub fn parse_env_entries(entries: &[String]) -> anyhow::Result<Vec<(String, String)>> {
    entries
        .iter()
        .map(|e| {
            split_env_entry(e)
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .map_err(anyhow::Error::msg)
        })
        .collect()
}

/// Parse `KEY=VALUE` env entries and render each value through a template closure.
///
/// Combines [`parse_env_entries`] with per-value rendering in one pass so call
/// sites don't duplicate the parse → iterate → render loop.  The `render`
/// closure is called once per value; any error is propagated with a
/// descriptive context message so the caller can identify which key failed.
///
/// Preserves declaration order — important for chained-env semantics in stages
/// like sign, sbom, and notarize where later entries may reference env vars set
/// by earlier ones.
///
/// ```ignore
/// let rendered = render_env_entries(cfg.env.as_deref().unwrap_or(&[]), |v| ctx.render_template(v))?;
/// for (k, v) in rendered { cmd.env(k, v); }
/// ```
pub fn render_env_entries<F>(entries: &[String], render: F) -> anyhow::Result<Vec<(String, String)>>
where
    F: Fn(&str) -> anyhow::Result<String>,
{
    let parsed = parse_env_entries(entries)?;
    parsed
        .into_iter()
        .map(|(k, v)| {
            let rendered = render(&v).with_context(|| format!("render env value for '{k}'"))?;
            Ok((k, rendered))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_env_entry_basic() {
        assert_eq!(split_env_entry("KEY=value").unwrap(), ("KEY", "value"));
    }

    #[test]
    fn test_split_env_entry_split_on_first_equals() {
        assert_eq!(
            split_env_entry("FLAGS=--key=val --other=stuff").unwrap(),
            ("FLAGS", "--key=val --other=stuff")
        );
    }

    #[test]
    fn test_split_env_entry_no_equals_errors() {
        let err = split_env_entry("COSIGN_PASSWORD").unwrap_err();
        assert!(err.contains("must be KEY=VALUE"), "{err}");
    }

    #[test]
    fn test_split_env_entry_empty_key_errors() {
        let err = split_env_entry("=value").unwrap_err();
        assert!(err.contains("empty key"), "{err}");
    }

    #[test]
    fn test_parse_env_entries_preserves_order() {
        let input = vec![
            "FIRST=1".to_string(),
            "SECOND=2".to_string(),
            "THIRD=3".to_string(),
        ];
        let parsed = parse_env_entries(&input).unwrap();
        assert_eq!(
            parsed,
            vec![
                ("FIRST".to_string(), "1".to_string()),
                ("SECOND".to_string(), "2".to_string()),
                ("THIRD".to_string(), "3".to_string()),
            ]
        );
    }

    #[test]
    fn test_render_env_entries_propagates_render_errors() {
        let input = vec!["GOOD=ok".to_string(), "BAD=fail".to_string()];
        let err = render_env_entries(&input, |v| {
            if v == "fail" {
                Err(anyhow::anyhow!("render boom"))
            } else {
                Ok(v.to_string())
            }
        })
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("BAD"), "error should label key BAD: {msg}");
        assert!(
            msg.contains("render boom"),
            "error chain should include underlying cause: {msg}"
        );
    }

    #[test]
    fn test_render_env_entries_passes_through_when_render_is_identity() {
        let input = vec!["A=1".to_string(), "B=2".to_string()];
        let rendered = render_env_entries(&input, |v| Ok(v.to_string())).unwrap();
        assert_eq!(
            rendered,
            vec![("A".into(), "1".into()), ("B".into(), "2".into())]
        );
    }
}
