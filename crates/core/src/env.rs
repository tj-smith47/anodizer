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
/// Entries render in declaration order and each render is handed the pairs
/// rendered before it, so a later entry can read an earlier one through
/// `{{ .Env.KEY }}`. The closure overlays those pairs on whatever it renders
/// with ([`crate::context::Context::render_template_with_env`],
/// [`crate::template::TemplateVars::with_env_overlay`]).
///
/// ```ignore
/// let rendered = render_env_entries(cfg.env.as_deref().unwrap_or(&[]), |v, so_far| {
///     ctx.render_template_with_env(v, so_far)
/// })?;
/// for (k, v) in rendered { cmd.env(k, v); }
/// ```
pub fn render_env_entries<F>(
    entries: &[String],
    mut render: F,
) -> anyhow::Result<Vec<(String, String)>>
where
    F: FnMut(&str, &[(String, String)]) -> anyhow::Result<String>,
{
    let parsed = parse_env_entries(entries)?;
    let mut rendered = Vec::with_capacity(parsed.len());
    for (k, v) in parsed {
        let value = render(&v, &rendered).with_context(|| format!("render env value for '{k}'"))?;
        rendered.push((k, value));
    }
    Ok(rendered)
}

/// The entries of `entries` that parse and render, in declaration order.
///
/// For a dry run, which spawns nothing and renders a job's `env:` only to
/// mask the values its echo line may repeat: an entry that does not render
/// (an unset `{{ .Env.X }}`) has no value to repeat, so it is left out
/// instead of failing a run that would never have used it.
pub fn render_env_entries_that_render<F>(entries: &[String], mut render: F) -> Vec<(String, String)>
where
    F: FnMut(&str, &[(String, String)]) -> anyhow::Result<String>,
{
    let mut rendered: Vec<(String, String)> = Vec::new();
    for e in entries {
        let Ok((k, v)) = split_env_entry(e) else {
            continue;
        };
        if let Ok(value) = render(v, &rendered) {
            rendered.push((k.to_string(), value));
        }
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lenient_render_keeps_the_entries_that_render() {
        let entries = vec![
            "A=ok".to_string(),
            "B=fails".to_string(),
            "no-equals".to_string(),
            "C=ok".to_string(),
        ];
        let rendered = render_env_entries_that_render(&entries, |v, _| {
            if v == "fails" {
                anyhow::bail!("undefined variable")
            }
            Ok(v.to_uppercase())
        });
        assert_eq!(
            rendered,
            vec![
                ("A".to_string(), "OK".to_string()),
                ("C".to_string(), "OK".to_string())
            ]
        );
    }

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
        let err = render_env_entries(&input, |v, _| {
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
        let rendered = render_env_entries(&input, |v, _| Ok(v.to_string())).unwrap();
        assert_eq!(
            rendered,
            vec![("A".into(), "1".into()), ("B".into(), "2".into())]
        );
    }

    /// Each render sees the pairs rendered before it, on both the strict and
    /// the lenient helper, so `B={{ .Env.A }}` reads the `A` above it.
    #[test]
    fn a_later_entry_reads_the_entries_rendered_before_it() {
        let input = vec![
            "A=value".to_string(),
            "B={{ .Env.A }}-b".to_string(),
            "C={{ .Env.B }}-c".to_string(),
        ];
        let vars = crate::template::TemplateVars::new();
        let render = |v: &str, so_far: &[(String, String)]| {
            crate::template::render(v, &vars.with_env_overlay(so_far))
        };
        let expected = vec![
            ("A".to_string(), "value".to_string()),
            ("B".to_string(), "value-b".to_string()),
            ("C".to_string(), "value-b-c".to_string()),
        ];
        assert_eq!(render_env_entries(&input, render).unwrap(), expected);
        assert_eq!(render_env_entries_that_render(&input, render), expected);
    }
}
