//! The base [`tera::Tera`] instance every render clones from, plus the builtin
//! filter and function registrations that populate it.
//!
//! Each submodule owns one family of builtins — the helper machinery it needs
//! together with both the filter (`{{ x | f }}`) and function (`{{ f(s=x) }}`)
//! form of the same builtin, so changing a builtin means changing one file.
//! Registration order across families carries no meaning: `register_filter` and
//! `register_function` are map inserts and no name is registered twice, so the
//! families commute.

use serde_json::Value;
use std::borrow::Cow;
use std::sync::LazyLock;

mod collection;
mod datetime;
mod env_file;
mod hash;
mod path;
mod printf;
mod text;
mod version;

pub(super) use datetime::translate_go_time_format;
pub(super) use text::register_ruby_escape;
pub use text::ruby_escape_str;

/// Convert a JSON template `Value` to a string for comparison purposes.
/// Numbers, bools, and strings are all stringified; null → "".
/// Returns `Cow::Borrowed` for strings (avoiding a clone), `Cow::Owned` otherwise.
fn value_to_string(v: &Value) -> Cow<'_, str> {
    match v {
        Value::String(s) => Cow::Borrowed(s.as_str()),
        Value::Number(n) => Cow::Owned(n.to_string()),
        Value::Bool(b) => Cow::Owned(b.to_string()),
        Value::Null => Cow::Borrowed(""),
        // Arrays and objects: fall back to JSON representation
        other => Cow::Owned(other.to_string()),
    }
}

/// Base Tera instance with custom filters pre-registered.
/// Cloned per render() call (cheap — no templates to clone).
pub(super) static BASE_TERA: LazyLock<tera::Tera> = LazyLock::new(|| {
    let mut tera = tera::Tera::default();
    text::register_ruby_escape(&mut tera);
    text::register(&mut tera);
    env_file::register(&mut tera);
    version::register(&mut tera);
    hash::register(&mut tera);
    datetime::register(&mut tera);
    path::register(&mut tera);
    collection::register(&mut tera);
    printf::register(&mut tera);

    tera
});
