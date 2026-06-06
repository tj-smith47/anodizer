use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::ScmRepoConfig;

// ---------------------------------------------------------------------------
// MilestoneConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MilestoneConfig {
    /// Repository owner/name. Auto-detected from git remote if not set.
    pub repo: Option<ScmRepoConfig>,
    /// Close the milestone on release. Default: false.
    pub close: Option<bool>,
    /// Fail the pipeline if milestone close fails. Default: false.
    pub fail_on_error: Option<bool>,
    /// Milestone name template (default: "{{ Tag }}").
    pub name_template: Option<String>,
}

impl MilestoneConfig {
    /// Default milestone name template (`"{{Tag}}"`).
    /// Anodize uses Tera-style `{{ Tag }}`; the rendered value is
    /// identical for any tag the project produces.
    pub const DEFAULT_NAME_TEMPLATE: &'static str = "{{ Tag }}";

    /// Resolve the milestone name template, falling back to
    /// [`Self::DEFAULT_NAME_TEMPLATE`].
    pub fn resolved_name_template(&self) -> &str {
        self.name_template
            .as_deref()
            .unwrap_or(Self::DEFAULT_NAME_TEMPLATE)
    }

    /// Resolve `close`, falling back to `false` (don't close milestones
    /// on release by default).
    pub fn resolved_close(&self) -> bool {
        self.close.unwrap_or(false)
    }

    /// Resolve `fail_on_error`, falling back to `false` (milestone close
    /// errors are warnings by default; opt in to fail-the-build).
    pub fn resolved_fail_on_error(&self) -> bool {
        self.fail_on_error.unwrap_or(false)
    }
}
