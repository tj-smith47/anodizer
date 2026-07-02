//! JSON boundary adapter between tera 2.0's engine types and anodizer's
//! `serde_json`-shaped filter/function bodies.
//!
//! The custom filters/functions implement a GoReleaser-parity contract whose
//! semantics are JSON-shaped (string/number/bool/null/array/object), so their
//! bodies stay on `serde_json::Value` permanently. tera 2.0 replaced its 1.x
//! `serde_json::Value` re-export with an opaque engine-native `tera::Value`,
//! so this module owns the one place where the two worlds convert:
//!
//! - [`to_json`] / [`from_json`] — value conversion via the serde round-trip
//!   (`tera::Value` implements `Serialize`; construction goes through
//!   `tera::Value::try_from_serializable`).
//! - [`kwargs_to_map`] — tera 2.0 `Kwargs` → the `HashMap<String, Value>`
//!   argument shape the 1.x-era bodies consume.
//! - [`JsonRegisterExt`] — `register_json_filter` / `register_json_function`
//!   wrappers that adapt a JSON-shaped closure to tera 2.0's
//!   `(value, Kwargs, &State)` / `(Kwargs, &State)` signatures.
//!
//! Rendered-output byte-identity across the 1.x → 2.0 engine swap is the
//! whole point: conversions must be lossless for every JSON shape (including
//! `u64 > i64::MAX` and whole-valued floats, which must stay floats so
//! `{{ 2.0 }}` keeps rendering `2.0`, not `2`).

use std::collections::HashMap;

use tera::{Kwargs, State, TeraResult};

/// Convert an engine value to its JSON equivalent via serde.
pub(super) fn to_json(v: &tera::Value) -> TeraResult<serde_json::Value> {
    serde_json::to_value(v).map_err(tera::Error::message)
}

/// Convert a JSON value to its engine equivalent via serde.
pub(super) fn from_json(v: &serde_json::Value) -> TeraResult<tera::Value> {
    tera::Value::try_from_serializable(v)
}

/// Convert tera 2.0 keyword arguments to the `HashMap<String, Value>` shape
/// the filter/function bodies consume (the 1.x function-argument shape).
pub(super) fn kwargs_to_map(kwargs: &Kwargs) -> TeraResult<HashMap<String, serde_json::Value>> {
    kwargs.deserialize()
}

/// Registration wrappers adapting JSON-shaped closures to tera 2.0's
/// filter/function traits. Every custom registration goes through these so
/// there is exactly one engine-boundary conversion site.
pub(super) trait JsonRegisterExt {
    /// Register a filter whose body is `Fn(&Value, &args) -> Result<Value>`
    /// over `serde_json::Value` (the 1.x filter signature).
    fn register_json_filter<F>(&mut self, name: &'static str, f: F)
    where
        F: Fn(
                &serde_json::Value,
                &HashMap<String, serde_json::Value>,
            ) -> Result<serde_json::Value, tera::Error>
            + Send
            + Sync
            + 'static;

    /// Register a function whose body is `Fn(&args) -> Result<Value>` over
    /// `serde_json::Value` (the 1.x function signature).
    fn register_json_function<F>(&mut self, name: &'static str, f: F)
    where
        F: Fn(&HashMap<String, serde_json::Value>) -> Result<serde_json::Value, tera::Error>
            + Send
            + Sync
            + 'static;
}

impl JsonRegisterExt for tera::Tera {
    fn register_json_filter<F>(&mut self, name: &'static str, f: F)
    where
        F: Fn(
                &serde_json::Value,
                &HashMap<String, serde_json::Value>,
            ) -> Result<serde_json::Value, tera::Error>
            + Send
            + Sync
            + 'static,
    {
        self.register_filter(
            name,
            // Owned `Value` arg: `ArgFromValue` for `tera::Value` is identity,
            // sidestepping the higher-ranked borrow bound a `&Value` closure
            // would have to satisfy.
            move |value: tera::Value, kwargs: Kwargs, _: &State| -> TeraResult<tera::Value> {
                let json_value = to_json(&value)?;
                let args = kwargs_to_map(&kwargs)?;
                from_json(&f(&json_value, &args)?)
            },
        );
    }

    fn register_json_function<F>(&mut self, name: &'static str, f: F)
    where
        F: Fn(&HashMap<String, serde_json::Value>) -> Result<serde_json::Value, tera::Error>
            + Send
            + Sync
            + 'static,
    {
        self.register_function(
            name,
            move |kwargs: Kwargs, _: &State| -> TeraResult<tera::Value> {
                let args = kwargs_to_map(&kwargs)?;
                from_json(&f(&args)?)
            },
        );
    }
}

/// Restore tera 1.x raw string-literal semantics by doubling every backslash
/// inside string literals within `{{ … }}` / `{% … %}` blocks.
///
/// tera 1.x string literals were fully raw: the pest grammar closed a string
/// at the FIRST occurrence of its opening delimiter character, full stop —
/// 1.x had no escape syntax at all, so a backslash never protected the
/// character after it. `pattern="(\w+)"` reached the filter body as `(\w+)`;
/// `'it\'s'` closed right after `it\` (the `'` immediately following the
/// backslash IS the close, exactly like any other `'`). tera 2.0 processes
/// escape sequences instead and hard-errors on unknown ones (only
/// `\" \' \/ \\ \n \t \r` are accepted), so the same raw-authored template
/// fails to parse under 2.0 as-is. The scanner below reproduces 1.x's rule
/// verbatim — no escape awareness, because 1.x had none — and simply
/// doubles every backslash it passes through.
///
/// Doubling makes the fix work: a run of N backslashes always becomes 2N —
/// always EVEN, independent of whether N itself was even or odd. tera 2.0's
/// escape-aware lexer resolves an even run of backslashes as N/2 paired
/// escapes with nothing left over, so the character right after the run is
/// never itself consumed as an escape target — it lands on the delimiter as
/// an ordinary, unescaped byte, closing the string at EXACTLY the position
/// 1.x's first-occurrence rule chose on the original (undoubled) text. The
/// two engines agree on the boundary by construction, for every backslash
/// run length, without the scanner needing any concept of escaping.
///
/// All three 1.x literal delimiters (`"`, `'`, `` ` ``) are handled; text
/// outside blocks and comment blocks is left untouched.
///
/// 2.0 also added inline map literals (`{'a': 1}`), whose own `{`/`}` pair
/// can sit inside a `{{ … }}` expression. A depth-blind scan sees that map
/// literal's closing `}` immediately followed by another `}` (the block's
/// own terminator or a further-nested map) and mistakes it for `}}`,
/// closing the expression early — any backslash literal later in that same
/// block then never gets doubled. Tracking `{`/`}` depth while outside a
/// string literal (string contents don't count — a literal `{` inside a
/// string is just text) makes `}}` only end the block at depth 0.
///
/// Contract: for a template that was VALID under 1.x's raw grammar, the
/// shim yields byte-identical rendered output (see the parity-sweep test
/// below, across N=0..5 backslashes before a close, including the Windows
/// path shape `'C:\Users\'`). For a template that was already INVALID under
/// 1.x — `'it\'s'` is the canonical case: a human author's JS/Python-style
/// intent to escape the embedded quote is something 1.x's raw grammar never
/// honored, since 1.x closed that same string at the `'` right after the
/// backslash too, leaving a dangling `s'` neither engine can make sense of —
/// the shim does not attempt to recover authorial intent. The transformed
/// text may cascade into an unterminated string that swallows the rest of
/// the template, but that must always surface as a LOUD tera parse/render
/// error, never a silent, wrong render.
pub(super) fn double_string_literal_backslashes(template: &str) -> std::borrow::Cow<'_, str> {
    if !template.contains('\\') {
        return std::borrow::Cow::Borrowed(template);
    }

    #[derive(PartialEq)]
    enum Region {
        Text,
        Comment,
        Block,
    }

    let mut out = String::with_capacity(template.len() + 8);
    let mut region = Region::Text;
    // The active string delimiter inside a block, if any.
    let mut string_delim: Option<char> = None;
    // Depth of `{`/`}` map-literal nesting inside the current block, outside
    // any string literal. `}}` only closes the block at depth 0.
    let mut brace_depth: u32 = 0;
    let mut chars = template.chars().peekable();

    while let Some(c) = chars.next() {
        match region {
            Region::Text => {
                if c == '{' {
                    match chars.peek() {
                        Some('{') | Some('%') => {
                            // Consume the full 2-char open delimiter now.
                            // Leaving the second char (`{` or `%`) for the
                            // next iteration would feed it to the Block
                            // arm's generic brace-depth counter below —
                            // for `{{` that's a bare `{` indistinguishable
                            // from a user map-literal open, desyncing
                            // `brace_depth` to 1 when the block is actually
                            // at depth 0.
                            out.push(c);
                            if let Some(second) = chars.next() {
                                out.push(second);
                            }
                            region = Region::Block;
                            brace_depth = 0;
                            continue;
                        }
                        Some('#') => region = Region::Comment,
                        _ => {}
                    }
                }
                out.push(c);
            }
            Region::Comment => {
                if c == '#' && chars.peek() == Some(&'}') {
                    region = Region::Text;
                }
                out.push(c);
            }
            Region::Block => {
                match string_delim {
                    Some(delim) => {
                        if c == '\\' {
                            // The one rewrite this whole scan exists for.
                            // Every backslash byte doubles independently
                            // (not per-pair), so a run of N backslashes
                            // always becomes 2N — always even, which is
                            // also what keeps tera 2.0's own lexer from
                            // ever treating the char after the run as an
                            // escape target.
                            out.push('\\');
                        } else if c == delim {
                            // First-occurrence close, no escape awareness —
                            // exactly 1.x's raw grammar. A preceding
                            // backslash (however many) never protects this
                            // delimiter; see the function doc comment for
                            // why doubling still keeps the two engines'
                            // close positions in agreement.
                            string_delim = None;
                        }
                    }
                    None => match c {
                        '"' | '\'' | '`' => {
                            string_delim = Some(c);
                        }
                        '{' => brace_depth += 1,
                        // A nested map literal's close: consumed by the
                        // depth counter, never a block-close candidate.
                        '}' if brace_depth > 0 => brace_depth -= 1,
                        // Block close: `}}` or `%}` outside a string literal
                        // and outside any open map literal. Consume the
                        // full 2-char close delimiter now (same reasoning
                        // as the open side) so the region genuinely returns
                        // to Text — leaving the second `}`/`%` behind used
                        // to strand the scan in Block for the rest of the
                        // template, corrupting any later quoted text.
                        '}' | '%' if chars.peek() == Some(&'}') => {
                            out.push(c);
                            if let Some(second) = chars.next() {
                                out.push(second);
                            }
                            region = Region::Text;
                            continue;
                        }
                        _ => {}
                    },
                }
                out.push(c);
            }
        }
    }

    std::borrow::Cow::Owned(out)
}

/// Coerce a filter's piped value to `$ty`, returning the tera-1.x error text
/// on mismatch (tera 2.0 dropped the `try_get_value!` macro this replaces;
/// the message format is preserved so error output stays stable).
macro_rules! try_get_value {
    ($filter_name:expr, $var_name:expr, $ty:ty, $val:expr) => {{
        match serde_json::from_value::<$ty>($val.clone()) {
            Ok(s) => s,
            Err(_) => {
                if $var_name == "value" {
                    return Err(tera::Error::message(format!(
                        "Filter `{}` was called on an incorrect value: got `{}` but expected a {}",
                        $filter_name,
                        $val,
                        stringify!($ty)
                    )));
                } else {
                    return Err(tera::Error::message(format!(
                        "Filter `{}` received an incorrect type for arg `{}`: got `{}` but expected a {}",
                        $filter_name,
                        $var_name,
                        $val,
                        stringify!($ty)
                    )));
                }
            }
        }
    }};
}
pub(super) use try_get_value;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// JSON → engine → JSON must be the identity for every JSON shape.
    fn assert_round_trip(v: serde_json::Value) {
        let engine = from_json(&v).expect("from_json");
        let back = to_json(&engine).expect("to_json");
        assert_eq!(back, v, "round trip changed the value");
    }

    #[test]
    fn round_trip_i64() {
        assert_round_trip(json!(-42));
        assert_round_trip(json!(i64::MIN));
        assert_round_trip(json!(i64::MAX));
    }

    #[test]
    fn round_trip_u64_beyond_i64() {
        assert_round_trip(json!(u64::MAX));
        assert_round_trip(json!(i64::MAX as u64 + 1));
    }

    #[test]
    fn round_trip_f64_stays_float() {
        assert_round_trip(json!(1.5));
        assert_round_trip(json!(-0.25));
        let engine = from_json(&json!(1.5)).unwrap();
        assert!(engine.as_f64().is_some(), "float must stay a float");
    }

    #[test]
    fn round_trip_whole_f64_stays_float_and_renders_with_point() {
        // 2.0 must NOT collapse to the integer 2: templates rendering the
        // value must keep emitting "2.0" exactly as the 1.x engine did.
        let v = json!(2.0);
        assert_round_trip(v.clone());
        let engine = from_json(&v).unwrap();
        assert!(engine.as_f64().is_some(), "whole float must stay a float");
        assert_eq!(engine.to_string(), "2.0");
    }

    #[test]
    fn round_trip_bool_and_null() {
        assert_round_trip(json!(true));
        assert_round_trip(json!(false));
        assert_round_trip(json!(null));
    }

    #[test]
    fn round_trip_unicode_string() {
        assert_round_trip(json!("héllo wörld — 日本語 🚀"));
        assert_round_trip(json!(""));
    }

    #[test]
    fn round_trip_nested_structures() {
        assert_round_trip(json!({
            "arr": [1, "two", 3.5, null, true],
            "obj": { "inner": { "deep": ["a", { "b": 2 }] } },
        }));
    }

    #[test]
    fn round_trip_empty_containers() {
        assert_round_trip(json!([]));
        assert_round_trip(json!({}));
    }

    #[test]
    fn kwargs_to_map_extracts_all_pairs() {
        let kwargs = Kwargs::from([
            ("s", tera::Value::from("hello")),
            ("n", tera::Value::from(7)),
            ("flag", tera::Value::from(true)),
        ]);
        let map = kwargs_to_map(&kwargs).unwrap();
        assert_eq!(map.len(), 3);
        assert_eq!(map["s"], json!("hello"));
        assert_eq!(map["n"], json!(7));
        assert_eq!(map["flag"], json!(true));
    }

    #[test]
    fn kwargs_to_map_empty() {
        let map = kwargs_to_map(&Kwargs::default()).unwrap();
        assert!(map.is_empty());
    }

    #[test]
    fn kwargs_to_map_u64_beyond_i64_max() {
        // Mirrors round_trip_u64_beyond_i64: a kwarg value out of i64 range
        // must survive the Kwargs -> HashMap<String, Value> conversion too.
        let kwargs = Kwargs::from([("n", tera::Value::from(u64::MAX))]);
        let map = kwargs_to_map(&kwargs).unwrap();
        assert_eq!(map["n"], json!(u64::MAX));
    }

    #[test]
    fn kwargs_to_map_whole_f64_stays_float() {
        // Mirrors round_trip_whole_f64_stays_float_and_renders_with_point:
        // a whole-valued float kwarg must deserialize back as a float, not
        // collapse into an integer.
        let kwargs = Kwargs::from([("f", tera::Value::from(2.0_f64))]);
        let map = kwargs_to_map(&kwargs).unwrap();
        assert_eq!(map["f"], json!(2.0));
        assert!(
            map["f"].is_f64(),
            "whole float kwarg must stay a float, got: {:?}",
            map["f"]
        );
    }

    #[test]
    fn json_filter_and_function_render_end_to_end() {
        let mut tera = tera::Tera::default();
        tera.register_json_filter("echo_upper", |value, _| {
            Ok(serde_json::Value::String(
                value.as_str().unwrap_or_default().to_uppercase(),
            ))
        });
        tera.register_json_function("concat2", |args| {
            let a = args.get("a").and_then(|v| v.as_str()).unwrap_or_default();
            let b = args.get("b").and_then(|v| v.as_str()).unwrap_or_default();
            Ok(serde_json::Value::String(format!("{a}{b}")))
        });
        tera.add_raw_template("t", "{{ concat2(a=\"x\", b=name) | echo_upper }}")
            .unwrap();
        let mut ctx = tera::Context::new();
        ctx.insert("name", "yz");
        assert_eq!(tera.render("t", &ctx).unwrap(), "XYZ");
    }

    #[test]
    fn backslash_shim_doubles_inside_expression_literals() {
        let tpl = r#"{{ reReplaceAll(pattern="(\w+) (\w+)", replacement="$2 $1") }}"#;
        let out = double_string_literal_backslashes(tpl);
        assert_eq!(
            out,
            r#"{{ reReplaceAll(pattern="(\\w+) (\\w+)", replacement="$2 $1") }}"#
        );
    }

    #[test]
    fn backslash_shim_preserves_1x_raw_semantics_for_known_escapes() {
        // 1.x kept `\n` / `\\` / `\t` as literal two-char bytes (backslash
        // plus the letter) — it never interpreted them as a newline/tab.
        // Doubling makes 2.0's unescaping reproduce exactly those raw bytes.
        assert_eq!(
            double_string_literal_backslashes(r#"{% set x = "a\nb\\c\td" %}"#),
            r#"{% set x = "a\\nb\\\\c\\td" %}"#
        );
        let mut tera = tera::Tera::default();
        let tpl = double_string_literal_backslashes(r#"{{ "a\nb\\c\td" }}"#);
        tera.add_raw_template("t", tpl.as_ref()).unwrap();
        let out = tera.render("t", &tera::Context::new()).unwrap();
        assert_eq!(out, r"a\nb\\c\td", "\\t must stay two raw chars, not a tab");
    }

    #[test]
    fn backslash_shim_handles_single_quote_and_backtick_literals() {
        assert_eq!(
            double_string_literal_backslashes(r#"{{ f(a='\d', b=`\s`) }}"#),
            r#"{{ f(a='\\d', b=`\\s`) }}"#
        );
    }

    #[test]
    fn backslash_shim_leaves_text_outside_blocks_untouched() {
        let tpl = r#"C:\path\to\thing {{ Version }} more \raw text"#;
        assert_eq!(double_string_literal_backslashes(tpl), tpl);
    }

    #[test]
    fn backslash_shim_leaves_comments_untouched() {
        let tpl = r#"{# a "\w" comment #}{{ Version }}"#;
        assert_eq!(double_string_literal_backslashes(tpl), tpl);
    }

    #[test]
    fn backslash_shim_ignores_block_close_inside_literal() {
        let tpl = r#"{{ f(a="x}}\w") }} tail \q"#;
        assert_eq!(
            double_string_literal_backslashes(tpl),
            r#"{{ f(a="x}}\\w") }} tail \q"#
        );
    }

    #[test]
    fn backslash_shim_nested_map_literal_does_not_close_block_early() {
        // A nested map's inner-then-outer closing braces used to look
        // identical to the block's own `}}` to a depth-blind scan, ending
        // the block right there — the `p='\w+'` kwarg that follows would
        // never get its backslash doubled.
        let tpl = r#"{{ f(m={'a': {'b': 1}}, p='\w+') }}"#;
        assert_eq!(
            double_string_literal_backslashes(tpl),
            r#"{{ f(m={'a': {'b': 1}}, p='\\w+') }}"#
        );
    }

    #[test]
    fn backslash_shim_brace_in_string_literal_does_not_count_as_map_depth() {
        // A `{`/`}` pair that's just text inside a string literal (not an
        // actual map literal) must not perturb the depth counter used to
        // find the real block close.
        let tpl = r#"{{ f(s="{not a map}", p='\w+') }}"#;
        assert_eq!(
            double_string_literal_backslashes(tpl),
            r#"{{ f(s="{not a map}", p='\\w+') }}"#
        );
    }

    #[test]
    fn tera_rejects_adjacent_closing_map_braces_independent_of_shim() {
        // tera 2.0's lexer detects its own `}}` variable-end delimiter with
        // a context-free two-byte lookahead — its `State` enum tracks only
        // Template/Variable/Tag, nothing for map-literal nesting depth. A
        // nested map literal's directly-abutting inner+outer closing braces
        // are therefore indistinguishable from the block's own terminator
        // to tera ITSELF: this fails to parse via bare tera with no
        // anodizer preprocessing involved at all. anodizer's shim (see
        // double_string_literal_backslashes) tracks brace depth so ITS OWN
        // text scan stays honest about where the block really ends, but
        // that cannot make tera accept syntax tera's own lexer rejects —
        // template authors must separate abutting closes with a space
        // (`{'a': {'b': 1} }`, not `{'a': {'b': 1}}`).
        let mut tera = tera::Tera::default();
        let err = tera
            .add_raw_template("t", r#"{{ {"a": {"b": 1}} }}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("expected"), "got: {err}");
    }

    #[test]
    fn backslash_shim_spaced_nested_map_literal_rendered_end_to_end() {
        // The tera-valid spelling of a nested map (a space before EVERY
        // closing brace that would otherwise abut another one) parses and
        // renders correctly, and a backslash literal later in the same
        // block still gets doubled.
        let mut tera = tera::Tera::default();
        tera.register_json_function("probe", |args| {
            let has_map = args.get("m").is_some_and(|v| v.is_object());
            let p = args.get("p").and_then(|v| v.as_str()).unwrap_or_default();
            Ok(serde_json::Value::String(format!("{has_map}:{p}")))
        });
        let tpl = double_string_literal_backslashes(r#"{{ probe(m={'a': {'b': 1} }, p='\w+') }}"#);
        tera.add_raw_template("t", tpl.as_ref()).unwrap();
        let out = tera.render("t", &tera::Context::new()).unwrap();
        assert_eq!(out, r"true:\w+");
    }

    #[test]
    fn backslash_shim_no_backslash_is_borrowed_passthrough() {
        let tpl = "{{ Version }} plain";
        assert!(matches!(
            double_string_literal_backslashes(tpl),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    #[test]
    fn backslash_shim_rendered_end_to_end_matches_1x_bytes() {
        let mut tera = tera::Tera::default();
        let tpl = double_string_literal_backslashes(r#"{{ "a\nb" }}|{{ "regex: (\w+)" }}"#);
        tera.add_raw_template("t", tpl.as_ref()).unwrap();
        let out = tera.render("t", &tera::Context::new()).unwrap();
        // Literal backslash-n (two bytes), exactly as 1.x rendered it.
        assert_eq!(out, r"a\nb|regex: (\w+)");
    }

    #[test]
    fn backslash_shim_text_after_block_with_quoted_backslash_not_doubled() {
        // Regression for the brace-depth desync: the second `{` of `{{` was
        // being fed to the generic depth counter (starting it at 1 instead
        // of 0), so the closing `}}`'s first `}` merely decremented depth
        // instead of ending the block — the scan never returned to Text.
        // Trailing plain-text quotes were then misread as a Block string
        // literal and their backslashes got doubled.
        let tpl = "{{ Version }} say \"hi\\there\"";
        assert_eq!(double_string_literal_backslashes(tpl), tpl);
    }

    #[test]
    fn backslash_shim_text_with_quotes_between_two_blocks_untouched() {
        let tpl = r#"{{ A }} says "quo\test" and 'sing\lequote' {{ B }}"#;
        assert_eq!(double_string_literal_backslashes(tpl), tpl);
    }

    #[test]
    fn backslash_shim_map_literal_in_first_block_then_string_in_second() {
        let tpl = r#"{{ f(m={'a': 1}) }} between {{ g(p="\d+") }}"#;
        assert_eq!(
            double_string_literal_backslashes(tpl),
            r#"{{ f(m={'a': 1}) }} between {{ g(p="\\d+") }}"#
        );
    }

    #[test]
    fn json_filter_error_propagates() {
        let mut tera = tera::Tera::default();
        tera.register_json_filter("always_fail", |_, _| {
            Err(tera::Error::message("boom from body"))
        });
        tera.add_raw_template("t", "{{ 1 | always_fail }}").unwrap();
        let err = tera
            .render("t", &tera::Context::new())
            .unwrap_err()
            .to_string();
        assert!(err.contains("boom from body"), "got: {err}");
    }

    #[test]
    fn backslash_shim_windows_path_literal_round_trips_through_real_tera() {
        // The blind spot round 4's escape-parity design got wrong: a
        // Windows path literal has an odd trailing backslash right before
        // the close, which 1.x's raw grammar always treats as a plain,
        // unescaped close (it has no escape concept at all). The doubled
        // shim output must both match byte-for-byte AND render, through a
        // real tera::Tera, to the exact original raw path.
        let tpl = r"{{ 'C:\Users\' }} tail \x";
        let out = double_string_literal_backslashes(tpl);
        assert_eq!(out, r"{{ 'C:\\Users\\' }} tail \x");
        assert!(out.ends_with(r" tail \x"));

        let mut tera = tera::Tera::default();
        tera.add_raw_template("t", out.as_ref()).unwrap();
        let rendered = tera.render("t", &tera::Context::new()).unwrap();
        assert_eq!(rendered, r"C:\Users\ tail \x");
    }

    #[test]
    fn backslash_shim_parity_sweep_backslash_counts_0_to_5_round_trip_through_real_tera() {
        // 1.x's first-occurrence close does not care whether the backslash
        // run before the delimiter is odd or even — it always closes right
        // there. Doubling always produces an EVEN run (2N), so tera 2.0
        // always resolves it back to exactly N literal backslashes,
        // agreeing with 1.x's raw value for every N.
        for n in 0..=5 {
            let raw = "\\".repeat(n);
            let tpl = format!("{{{{ 'end{raw}' }}}} t \\x");
            let out = double_string_literal_backslashes(&tpl);

            let doubled = "\\".repeat(n * 2);
            let expected = format!("{{{{ 'end{doubled}' }}}} t \\x");
            assert_eq!(out, expected, "n={n}");
            assert!(out.ends_with(" t \\x"), "n={n}, got: {out}");

            let mut tera = tera::Tera::default();
            tera.add_raw_template("t", out.as_ref()).unwrap();
            let rendered = tera.render("t", &tera::Context::new()).unwrap();
            let expected_rendered = format!("end{raw} t \\x");
            assert_eq!(rendered, expected_rendered, "n={n}");
        }
    }

    #[test]
    fn backslash_shim_1x_invalid_literal_syntax_produces_loud_tera_error() {
        // Templates whose authors relied on JS/Python-style backslash
        // escaping of the delimiter itself were never valid under 1.x's
        // raw, non-escape-aware grammar either — 1.x closed these same
        // strings at the escaped quote, leaving a dangling remainder
        // neither engine can parse. The shim must not silently mis-render
        // these; it must fail loudly, exactly as 1.x would have.
        let invalid_templates = [
            r#"{{ 'it\'s' }} plain \raw {{ g(p="\d+") }}"#,
            r#"{{ 'abc\' more content ' }} tail \x"#,
        ];
        for tpl in invalid_templates {
            let shimmed = double_string_literal_backslashes(tpl);
            let mut tera = tera::Tera::default();
            let add_result = tera.add_raw_template("t", shimmed.as_ref());
            let is_loud_error = match add_result {
                Err(_) => true,
                Ok(()) => tera.render("t", &tera::Context::new()).is_err(),
            };
            assert!(
                is_loud_error,
                "expected a loud tera error for 1.x-invalid template {tpl:?}, shimmed to {shimmed:?}"
            );
        }
    }

    #[test]
    fn backslash_shim_double_backslash_immediately_before_close_leaves_trailing_text_untouched() {
        // Two backslashes right before the real closing quote: each
        // backslash still doubles independently (an even run stays even
        // after doubling either way), and the delimiter right after them
        // is the true, unescaped close.
        let tpl = r"{{ 'end\\' }} t \x";
        let out = double_string_literal_backslashes(tpl);
        assert_eq!(out, r"{{ 'end\\\\' }} t \x");
        assert!(out.ends_with(r" t \x"));
    }

    #[test]
    fn backslash_shim_unterminated_literal_ending_in_backslash_at_eof_does_not_panic() {
        let tpl = r#"before text {{ f(a="unterminated\"#;
        let out = double_string_literal_backslashes(tpl);
        assert!(
            out.starts_with("before text {{"),
            "text before the block must stay untouched, got: {out}"
        );
    }

    /// Reference span-finder for the property check below: a second,
    /// independently-written boundary scan that only needs to know "is this
    /// byte inside a `{{ … }}` / `{% … %}` block or not" — it doesn't double
    /// any backslash, so it can't hide a bug that the production function
    /// might share with a copy-pasted version of itself. Text (and comment)
    /// bytes are copied verbatim by the shim no matter what happens to
    /// strings inside blocks, so this walk finds the same boundaries as the
    /// real scan for every non-pathological template (the two 1.x-invalid
    /// inputs covered by their own dedicated error test are excluded here —
    /// an unterminated stray string swallowing the rest of the template
    /// isn't a "text span" in any meaningful sense).
    fn non_block_spans(template: &str) -> Vec<&str> {
        #[derive(PartialEq)]
        enum Region {
            Text,
            Block,
        }

        let mut spans = Vec::new();
        let mut region = Region::Text;
        let mut span_start = 0usize;
        let mut string_delim: Option<char> = None;
        let mut brace_depth: u32 = 0;
        let mut chars = template.char_indices().peekable();

        while let Some((i, c)) = chars.next() {
            match region {
                Region::Text => {
                    if c == '{' && matches!(chars.peek(), Some((_, '{')) | Some((_, '%'))) {
                        spans.push(&template[span_start..i]);
                        chars.next();
                        region = Region::Block;
                        brace_depth = 0;
                        string_delim = None;
                    }
                }
                Region::Block => match string_delim {
                    Some(delim) => {
                        if c == delim {
                            string_delim = None;
                        }
                    }
                    None => match c {
                        '"' | '\'' | '`' => string_delim = Some(c),
                        '{' => brace_depth += 1,
                        '}' if brace_depth > 0 => brace_depth -= 1,
                        '}' | '%' if matches!(chars.peek(), Some((_, '}'))) => {
                            let (j, _) = chars.next().expect("peeked Some above");
                            region = Region::Text;
                            span_start = j + 1;
                        }
                        _ => {}
                    },
                },
            }
        }
        if region == Region::Text {
            spans.push(&template[span_start..]);
        }
        spans
    }

    #[test]
    fn backslash_shim_text_outside_blocks_always_matches_across_adversarial_inputs() {
        // Every non-block byte must survive the shim byte-for-byte, across
        // the module's adversarial templates (the two 1.x-invalid inputs
        // get their own dedicated error-assertion test instead — see
        // `non_block_spans`'s doc comment for why).
        let templates = [
            r#"{{ reReplaceAll(pattern="(\w+) (\w+)", replacement="$2 $1") }}"#,
            r#"{% set x = "a\nb\\c" %}"#,
            r#"{{ f(a='\d', b=`\s`) }}"#,
            r"C:\path\to\thing {{ Version }} more \raw text",
            r#"{# a "\w" comment #}{{ Version }}"#,
            r#"{{ f(a="x}}\w") }} tail \q"#,
            r#"{{ f(m={'a': {'b': 1}}, p='\w+') }}"#,
            r#"{{ f(s="{not a map}", p='\w+') }}"#,
            "{{ Version }} plain",
            "{{ Version }} say \"hi\\there\"",
            r#"{{ A }} says "quo\test" and 'sing\lequote' {{ B }}"#,
            r#"{{ f(m={'a': 1}) }} between {{ g(p="\d+") }}"#,
            r"{{ 'end\\' }} t \x",
            r#"before text {{ f(a="unterminated\"#,
            // Odd-backslash Windows-path shapes — round 4's blind spot: a
            // trailing backslash sitting immediately adjacent to a block
            // delimiter (inside a string literal, or in plain text right
            // at the `{{`/`}}` boundary itself, no separating space).
            r"{{ 'C:\Users\' }} tail \x",
            r"C:\Users\{{ Version }}\tail",
            r"C:\Users\Name\{{ f(p='D:\Data\') }}\Trailing\",
        ];

        for tpl in templates {
            let shimmed = double_string_literal_backslashes(tpl);
            assert_eq!(
                non_block_spans(tpl),
                non_block_spans(shimmed.as_ref()),
                "text/comment spans diverged for template: {tpl:?}"
            );
        }
    }
}
