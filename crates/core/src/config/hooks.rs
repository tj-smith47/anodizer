use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

// ---------------------------------------------------------------------------
// HooksConfig
// ---------------------------------------------------------------------------

/// Top-level lifecycle hooks for `before` and `after` blocks.
/// Each block carries a list of hook commands that run around the
/// entire pipeline (not individual stages).
///
/// The canonical key is `hooks:` for both `before:` and `after:` to
/// match GoReleaser Pro (`hooks.md`). The `post:` spelling is accepted
/// as a serde alias on `hooks` for back-compat with the previous
/// anodizer spelling; users with `after: { post: [...] }` keep working
/// and a deprecation warning is logged when both spellings appear in
/// the same block (see [`HooksConfig::merge_hook_aliases`]).
#[derive(Debug, Clone, PartialEq, Default, JsonSchema)]
pub struct HooksConfig {
    /// Commands to run when the block fires. The wire format accepts
    /// either `hooks:` (canonical, GoReleaser-aligned) or the legacy
    /// `post:` spelling; both fold into this field at parse time.
    pub hooks: Option<Vec<HookEntry>>,
    /// Legacy alias for `hooks:` (anodizer pre-v0.4). Always `None`
    /// after parsing — `merge_hook_aliases` collapses it into `hooks`.
    /// Present on the struct only because `Deserialize` writes through
    /// it before the fold step.
    #[doc(hidden)]
    pub post: Option<Vec<HookEntry>>,
}

impl HooksConfig {
    /// Fold the deprecated `post:` spelling into `hooks:` so downstream
    /// readers consult one field. Emits a `tracing::warn!` when both
    /// spellings appear in the same block (the user almost certainly
    /// meant one or the other).
    fn merge_hook_aliases(&mut self) {
        let has_hooks = self.hooks.as_ref().is_some_and(|v| !v.is_empty());
        let has_post = self.post.as_ref().is_some_and(|v| !v.is_empty());
        if has_hooks && has_post {
            tracing::warn!(
                "DEPRECATION: top-level hooks block has both 'hooks:' and 'post:' \
                 — using 'hooks:' and ignoring 'post:'. The 'post:' spelling is \
                 deprecated; remove it from your config."
            );
            self.post = None;
        } else if has_post {
            tracing::warn!(
                "DEPRECATION: top-level 'after.post:' / 'before.post:' is renamed to \
                 'hooks:' for GoReleaser parity. The old spelling still works but \
                 will be removed in a future release; switch to 'hooks:'."
            );
            self.hooks = self.post.take();
        }
    }
}

impl Serialize for HooksConfig {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let count = self.hooks.is_some() as usize + self.post.is_some() as usize;
        let mut state = serializer.serialize_struct("HooksConfig", count)?;
        if let Some(ref h) = self.hooks {
            state.serialize_field("hooks", h)?;
        }
        if let Some(ref p) = self.post {
            state.serialize_field("post", p)?;
        }
        state.end()
    }
}

impl<'de> Deserialize<'de> for HooksConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Raw {
            hooks: Option<Vec<HookEntry>>,
            post: Option<Vec<HookEntry>>,
        }
        let raw = Raw::deserialize(deserializer)?;
        let mut out = HooksConfig {
            hooks: raw.hooks,
            post: raw.post,
        };
        out.merge_hook_aliases();
        Ok(out)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct StructuredHook {
    /// Command to run.
    ///
    /// The entire string is interpreted by `sh -c`, so shell metacharacters
    /// (`|`, `;`, `&&`, backticks, `$()`, redirects, globs) are honoured —
    /// any templated values folded into `cmd` become part of the shell
    /// command and are subject to word-splitting and metacharacter expansion.
    /// Keep templated user-config values out of `cmd` when possible, or quote
    /// them defensively (e.g. `'{{ .Env.FOO }}'`). Hooks already run with
    /// `env_clear()` plus an allow-list, so secrets in `$ENV` are not
    /// inherited unless explicitly listed in `env`.
    pub cmd: String,
    /// Working directory for the command (defaults to project root).
    pub dir: Option<String>,
    /// Environment variables for the command.
    #[serde(default)]
    pub env: Option<Vec<String>>,
    /// When true, capture and log stdout/stderr of the command.
    pub output: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum HookEntry {
    Simple(String),
    Structured(StructuredHook),
}

impl PartialEq<&str> for HookEntry {
    fn eq(&self, other: &&str) -> bool {
        match self {
            HookEntry::Simple(s) => s.as_str() == *other,
            HookEntry::Structured(h) => h.cmd.as_str() == *other,
        }
    }
}

impl<'de> Deserialize<'de> for HookEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        match &value {
            serde_json::Value::String(s) => Ok(HookEntry::Simple(s.clone())),
            serde_json::Value::Object(_) => {
                let hook: StructuredHook =
                    serde_json::from_value(value).map_err(serde::de::Error::custom)?;
                Ok(HookEntry::Structured(hook))
            }
            _ => Err(serde::de::Error::custom(
                "hook entry must be a string or an object with cmd/dir/env/output",
            )),
        }
    }
}
