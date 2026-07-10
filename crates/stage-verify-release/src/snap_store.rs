//! Snap Store channel-map probe for the snapcraft landing check.
//!
//! `snapcraft upload` returning OK proves the store ACCEPTED the binary — not
//! that consumers can install it: a manual-review hold parks the revision
//! outside every channel until a human approves it, and a decline arrives
//! only by email. The store's public info endpoint
//! (`GET /v2/snaps/info/<name>`, anonymous, `Snap-Device-Series: 16`) reports
//! what is actually live per channel, so probing it for the released version
//! is the honest post-publish verdict.

use anodizer_core::log::StageLogger;
use anodizer_core::retry::{RetryLog, RetryPolicy, SuccessClass, http_status, retry_http_blocking};
use anyhow::{Context as _, Result};

/// Public Snap Store info API base.
const SNAP_INFO_BASE: &str = "https://api.snapcraft.io";

/// Probe timeout per request (the payload is a small JSON document).
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Whether `version` of `snap` is live in the store's channel map — in the
/// specific `channel` when one was released to, otherwise in any channel.
/// `Ok(false)` covers both "snap unknown" (404) and "version absent from the
/// channel map"; `Err` means the store could not be consulted, which the
/// caller must report as unverifiable rather than as a pass.
pub fn snap_version_in_channel_map(
    snap: &str,
    version: &str,
    channel: Option<&str>,
    policy: &RetryPolicy,
    log: &StageLogger,
) -> Result<bool> {
    // Test-harness base override, mirroring the cargo landing probe's env
    // gating: integration tests drive the real binary across a process
    // boundary, so an env-routed base pointing at a local responder is the
    // only hermetic option there. Honored ONLY under ANODIZE_TEST_HARNESS=1.
    let base = match std::env::var("ANODIZER_TEST_SNAP_INFO_BASE") {
        Ok(b) if std::env::var("ANODIZE_TEST_HARNESS").as_deref() == Ok("1") => b,
        _ => SNAP_INFO_BASE.to_string(),
    };
    snap_version_in_channel_map_at(&base, snap, version, channel, policy, log)
}

/// [`snap_version_in_channel_map`] against an explicit API base (unit tests
/// point this at a scripted responder).
fn snap_version_in_channel_map_at(
    base: &str,
    snap: &str,
    version: &str,
    channel: Option<&str>,
    policy: &RetryPolicy,
    log: &StageLogger,
) -> Result<bool> {
    let url = format!("{}/v2/snaps/info/{snap}", base.trim_end_matches('/'));
    let client = anodizer_core::http::blocking_client(PROBE_TIMEOUT)
        .context("build HTTP client for snap store probe")?;
    let label = format!("verify-release: query snap store info for '{snap}'");
    match retry_http_blocking(
        RetryLog::new(&label, log),
        policy,
        SuccessClass::Strict,
        // The Snap-Device-Series header is mandatory on this endpoint; 16 is
        // the only series the store has ever defined.
        |_| client.get(&url).header("Snap-Device-Series", "16").send(),
        |status, body| format!("snap store info returned {status} for '{snap}': {body}"),
    ) {
        Ok((_status, body)) => {
            let info: serde_json::Value = serde_json::from_str(&body)
                .with_context(|| format!("parse snap store info for '{snap}'"))?;
            Ok(channel_map_contains(&info, version, channel))
        }
        Err(err) if http_status(&err) == 404 => Ok(false),
        Err(err) => Err(err),
    }
}

/// Whether the info document's `channel-map` carries `version` — in the
/// requested `channel` when given, in any channel otherwise.
///
/// A channel spec is accepted in every spelling the store and snapcraft use:
/// the full channel `name` (`latest/stable`), the bare `risk` (`stable`), or
/// `track/risk` assembled from the entry's own fields — so a config
/// `channels: [stable]` matches the store's `latest/stable` entry.
fn channel_map_contains(info: &serde_json::Value, version: &str, channel: Option<&str>) -> bool {
    let Some(entries) = info.get("channel-map").and_then(|v| v.as_array()) else {
        return false;
    };
    entries.iter().any(|entry| {
        if entry.get("version").and_then(|v| v.as_str()) != Some(version) {
            return false;
        }
        let Some(want) = channel else {
            return true;
        };
        let ch = entry.get("channel");
        let field = |key: &str| {
            ch.and_then(|c| c.get(key))
                .and_then(|v| v.as_str())
                .unwrap_or("")
        };
        let (name, track, risk) = (field("name"), field("track"), field("risk"));
        want == name || want == risk || want == format!("{track}/{risk}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn info(entries: serde_json::Value) -> serde_json::Value {
        json!({ "channel-map": entries })
    }

    fn entry(track: &str, risk: &str, version: &str) -> serde_json::Value {
        json!({
            "channel": { "name": format!("{track}/{risk}"), "track": track, "risk": risk },
            "version": version,
        })
    }

    #[test]
    fn version_in_any_channel_matches_without_channel_filter() {
        let doc = info(json!([entry("latest", "edge", "1.2.3")]));
        assert!(channel_map_contains(&doc, "1.2.3", None));
        assert!(!channel_map_contains(&doc, "9.9.9", None));
    }

    #[test]
    fn bare_risk_spec_matches_the_latest_track_entry() {
        let doc = info(json!([entry("latest", "stable", "1.2.3")]));
        assert!(channel_map_contains(&doc, "1.2.3", Some("stable")));
        assert!(channel_map_contains(&doc, "1.2.3", Some("latest/stable")));
        assert!(!channel_map_contains(&doc, "1.2.3", Some("candidate")));
    }

    #[test]
    fn track_qualified_spec_matches_only_its_track() {
        let doc = info(json!([
            entry("latest", "stable", "2.0.0"),
            entry("1.0", "stable", "1.0.9"),
        ]));
        assert!(channel_map_contains(&doc, "1.0.9", Some("1.0/stable")));
        assert!(!channel_map_contains(&doc, "2.0.0", Some("1.0/stable")));
    }

    #[test]
    fn missing_or_malformed_channel_map_is_not_visible() {
        assert!(!channel_map_contains(&json!({}), "1.2.3", None));
        assert!(!channel_map_contains(
            &json!({ "channel-map": "nope" }),
            "1.2.3",
            None
        ));
    }
}
