use super::*;

// ---------------------------------------------------------------------------
// SnapcraftConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SnapcraftConfig {
    /// Unique identifier for this snapcraft config.
    pub id: Option<String>,
    /// Build IDs to include. Empty means all builds.
    pub ids: Option<Vec<String>>,
    /// Snap package name in the store.
    pub name: Option<String>,
    /// Canonical application title (user-facing in store).
    pub title: Option<String>,
    /// Single-line elevator pitch (max 79 characters).
    pub summary: Option<String>,
    /// Extended description (user-facing in store).
    pub description: Option<String>,
    /// Path to icon image file.
    pub icon: Option<String>,
    /// Runtime base snap: core, core18, core20, core22, core24, bare.
    pub base: Option<String>,
    /// Release stability level: stable, devel.
    pub grade: Option<String>,
    /// License identifier (SPDX format).
    pub license: Option<String>,
    /// Whether to publish to the snapcraft store.
    pub publish: Option<bool>,
    /// Distribution channels: edge, beta, candidate, stable.
    pub channel_templates: Option<Vec<String>>,
    /// Security confinement level: strict, devmode, classic.
    pub confinement: Option<String>,
    /// Top-level snap plug definitions (structured map).
    /// Keys are plug names, values are either `null` (simple plug) or an object
    /// with `interface` and optional attributes (e.g. `{ interface: "content", target: "$SNAP/shared" }`).
    /// GoReleaser uses `map[string]any` for this field.
    pub plugs: Option<HashMap<String, serde_json::Value>>,
    // No top-level `slots:` — Snapcraft itself has no top-level slots
    // concept; use `apps.<name>.slots` for per-app slots.
    /// Required snapd features/versions.
    pub assumes: Option<Vec<String>>,
    /// Application configurations defining daemons, commands, env vars.
    pub apps: Option<HashMap<String, SnapcraftApp>>,
    /// Directory mappings for sandbox accessibility.
    pub layouts: Option<HashMap<String, SnapcraftLayout>>,
    /// Additional static files to bundle (string shorthand or structured form).
    pub extra_files: Option<Vec<SnapcraftExtraFileSpec>>,
    /// Extra files whose contents are rendered through the template engine before bundling.
    /// Unlike `extra_files` which copy as-is, template variables like `{{ .Tag }}` are expanded.
    /// GoReleaser Pro feature.
    pub templated_extra_files: Option<Vec<TemplatedExtraFile>>,
    /// Template for the output snap filename.
    pub name_template: Option<String>,
    /// Skip this snapcraft config. Accepts bool or template string
    /// (e.g. `"{{ if .IsSnapshot }}true{{ endif }}"` for conditional skip).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// Remove source archives from artifacts, keeping only snap.
    pub replace: Option<bool>,
    /// Output timestamp for reproducible builds.
    pub mod_timestamp: Option<String>,
    /// Snap hooks — maps hook name to arbitrary hook config.
    pub hooks: Option<HashMap<String, serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct SnapcraftApp {
    /// Command to run (relative to snap root).
    pub command: Option<String>,
    /// Daemon type: simple, forking, oneshot, notify, dbus.
    pub daemon: Option<String>,
    /// How to stop the daemon: sigterm, sigkill, etc.
    #[serde(alias = "stop-mode")]
    pub stop_mode: Option<String>,
    /// Interface plugs the app needs.
    pub plugs: Option<Vec<String>>,
    /// Environment variables for the app (supports string, integer, and boolean values).
    pub environment: Option<HashMap<String, serde_json::Value>>,
    /// Additional arguments passed to the command.
    pub args: Option<String>,
    /// Restart condition: on-failure, always, on-success, on-abnormal, on-abort, on-watchdog, never.
    #[serde(alias = "restart-condition")]
    pub restart_condition: Option<String>,
    /// Snap adapter type: "none" or "full" (default: "full").
    pub adapter: Option<String>,
    /// Services that must start before this app.
    pub after: Option<Vec<String>>,
    /// Alternative names for the command.
    pub aliases: Option<Vec<String>>,
    /// Desktop file for autostart.
    pub autostart: Option<String>,
    /// Services that must start after this app.
    pub before: Option<Vec<String>>,
    /// D-Bus well-known bus name.
    #[serde(alias = "bus-name")]
    pub bus_name: Option<String>,
    /// Wrapper commands run before the main command.
    #[serde(alias = "command-chain")]
    pub command_chain: Option<Vec<String>>,
    /// AppStream metadata common ID.
    #[serde(alias = "common-id")]
    pub common_id: Option<String>,
    /// Path to bash completion script relative to snap.
    pub completer: Option<String>,
    /// Path to .desktop file relative to snap.
    pub desktop: Option<String>,
    /// Snap extensions to apply.
    pub extensions: Option<Vec<String>>,
    /// Installation mode: "enable" or "disable".
    #[serde(alias = "install-mode")]
    pub install_mode: Option<String>,
    /// Arbitrary YAML passed through to snap.yaml.
    pub passthrough: Option<HashMap<String, serde_json::Value>>,
    /// Command to run after daemon stops.
    #[serde(alias = "post-stop-command")]
    pub post_stop_command: Option<String>,
    /// Refresh behavior: "endure" or "restart".
    #[serde(alias = "refresh-mode")]
    pub refresh_mode: Option<String>,
    /// Command to reload daemon config.
    #[serde(alias = "reload-command")]
    pub reload_command: Option<String>,
    /// Delay between restarts (duration string).
    #[serde(alias = "restart-delay")]
    pub restart_delay: Option<String>,
    /// Interface slots this app provides.
    pub slots: Option<Vec<String>>,
    /// Socket definitions map.
    pub sockets: Option<HashMap<String, serde_json::Value>>,
    /// Start timeout duration string.
    #[serde(alias = "start-timeout")]
    pub start_timeout: Option<String>,
    /// Command to gracefully stop the daemon.
    #[serde(alias = "stop-command")]
    pub stop_command: Option<String>,
    /// Stop timeout duration string.
    #[serde(alias = "stop-timeout")]
    pub stop_timeout: Option<String>,
    /// Timer definition (systemd timer syntax).
    pub timer: Option<String>,
    /// Watchdog timeout duration string.
    #[serde(alias = "watchdog-timeout")]
    pub watchdog_timeout: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct SnapcraftLayout {
    /// Bind-mount a directory to the snap's layout.
    pub bind: Option<String>,
    /// Bind-mount a single file to the snap's layout.
    pub bind_file: Option<String>,
    /// Symlink a path to a location in the snap.
    pub symlink: Option<String>,
    /// Layout entry type.
    #[serde(rename = "type")]
    pub type_: Option<String>,
}

/// Specifies an extra file for snapcraft. Can be a simple source path string or
/// a structured object with source, destination, and mode fields (matching
/// GoReleaser's SnapcraftExtraFiles).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum SnapcraftExtraFileSpec {
    /// Simple source path string.
    Source(String),
    /// Structured form with source, destination, and mode.
    Detailed {
        source: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        destination: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        mode: Option<u32>,
    },
}

impl SnapcraftExtraFileSpec {
    /// Return the source path for this spec.
    pub fn source(&self) -> &str {
        match self {
            SnapcraftExtraFileSpec::Source(s) => s,
            SnapcraftExtraFileSpec::Detailed { source, .. } => source,
        }
    }

    /// Return the optional destination path.
    pub fn destination(&self) -> Option<&str> {
        match self {
            SnapcraftExtraFileSpec::Source(_) => None,
            SnapcraftExtraFileSpec::Detailed { destination, .. } => destination.as_deref(),
        }
    }

    /// Return the optional file mode.
    pub fn mode(&self) -> Option<u32> {
        match self {
            SnapcraftExtraFileSpec::Source(_) => None,
            SnapcraftExtraFileSpec::Detailed { mode, .. } => *mode,
        }
    }
}
