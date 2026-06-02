use std::collections::BTreeMap;

use serde::Serialize;

// The default snap name template
// — preserves Os, Arm/Mips/Amd64 variant suffixes so multi-arch builds don't
// collapse to the same filename.
pub(super) const DEFAULT_SNAP_NAME_TEMPLATE: &str = "{{ .ProjectName }}_{{ .Version }}_{{ .Os }}_{{ .Arch }}{{ with .Arm }}v{{ . }}{{ end }}{{ with .Mips }}_{{ . }}{{ end }}{{ if not (eq .Amd64 \"v1\") }}{{ .Amd64 }}{{ end }}";

// ---------------------------------------------------------------------------
// Serde-serializable snapcraft YAML model
// ---------------------------------------------------------------------------

pub(super) fn is_empty_vec<T>(v: &[T]) -> bool {
    v.is_empty()
}

#[derive(Serialize)]
pub(super) struct SnapcraftYaml {
    pub name: String,
    pub version: String,
    pub summary: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grade: Option<String>,
    pub confinement: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(skip_serializing_if = "is_empty_vec")]
    pub assumes: Vec<String>,
    #[serde(skip_serializing_if = "is_empty_vec")]
    pub architectures: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub apps: BTreeMap<String, SnapcraftYamlApp>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub plugs: BTreeMap<String, serde_json::Value>,
    #[serde(rename = "layout")]
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub layouts: BTreeMap<String, SnapcraftYamlLayout>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub hooks: BTreeMap<String, serde_json::Value>,
}

#[derive(Default, Serialize)]
pub(super) struct SnapcraftYamlApp {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daemon: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "stop-mode")]
    pub stop_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "restart-condition")]
    pub restart_condition: Option<String>,
    #[serde(skip_serializing_if = "is_empty_vec")]
    pub plugs: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub environment: BTreeMap<String, serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    #[serde(skip_serializing_if = "is_empty_vec")]
    pub after: Vec<String>,
    #[serde(skip_serializing_if = "is_empty_vec")]
    pub aliases: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub autostart: Option<String>,
    #[serde(skip_serializing_if = "is_empty_vec")]
    pub before: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "bus-name")]
    pub bus_name: Option<String>,
    #[serde(skip_serializing_if = "is_empty_vec", rename = "command-chain")]
    pub command_chain: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "common-id")]
    pub common_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desktop: Option<String>,
    #[serde(skip_serializing_if = "is_empty_vec")]
    pub extensions: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "install-mode")]
    pub install_mode: Option<String>,
    #[serde(flatten)]
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub passthrough: BTreeMap<String, serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "post-stop-command")]
    pub post_stop_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "refresh-mode")]
    pub refresh_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "reload-command")]
    pub reload_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "restart-delay")]
    pub restart_delay: Option<String>,
    #[serde(skip_serializing_if = "is_empty_vec")]
    pub slots: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub sockets: BTreeMap<String, serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "start-timeout")]
    pub start_timeout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "stop-command")]
    pub stop_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "stop-timeout")]
    pub stop_timeout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "watchdog-timeout")]
    pub watchdog_timeout: Option<String>,
}

#[derive(Serialize)]
pub(super) struct SnapcraftYamlLayout {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "bind-file")]
    pub bind_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symlink: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "type")]
    pub type_: Option<String>,
}
