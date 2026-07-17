//! Shared derivation of which released assets are signature or keyless-cert
//! artifacts.
//!
//! Signature bytes (GPG/cosign) are non-reproducible by construction — a
//! resign of byte-identical input yields different output (embedded
//! timestamp/nonce) — and a keyless cosign certificate (`signs[].certificate`)
//! is equally per-invocation (Fulcio mints a fresh short-lived cert every
//! sign). Any consumer that compares an asset's bytes across two points in
//! time (the determinism harness's drift allow-list, release verification's
//! published-vs-local digest check) needs to know which asset names are
//! signatures/certificates so it can exempt them from that comparison. Both
//! consumers must derive the same suffix set from the same config, or they
//! silently drift apart (a suffix that's allow-listed for determinism but
//! not exempted from digest verification, or vice versa) — this module is
//! the single source of truth for that derivation.

use std::collections::BTreeSet;

use crate::config::{Config, SignConfig};

/// Extract the literal filename suffix a `signature:` template appends
/// after the artifact reference — the text following the final `}}`
/// template expansion (e.g. `{{ .Artifact }}.cosign.bundle` →
/// `.cosign.bundle`, `{{ .Artifact }}.sig` → `.sig`).
///
/// Returns `None` when there is no usable dotted extension to anchor a
/// `*.<ext>` match on (empty tail, a bare `.`, or a template that signs
/// in place without adding an extension). The guard is load-bearing: a
/// tail of `""` would yield a bare `*` (matching every artifact) and a
/// tail of `"."` would yield `*.` (matching any name ending in a dot) —
/// both would silently over-match. Require at least one extension
/// character after the leading dot.
///
/// This also (correctly) returns `None` when the final path segment is
/// itself an expansion — e.g. `{{ .Artifact }}.{{ .Format }}` or
/// `sigs/{{ .ArtifactName }}`. There the text after the last `}}` is empty
/// (or has no leading-dot literal), so no static suffix exists to anchor a
/// match on. Such templates can't be reduced to a `*.<ext>` glob; the
/// derived suffix set omits them. Release verification only consults this
/// suffix set for assets with no locally-registered artifact (its primary
/// classification is [`crate::artifact::ArtifactKind::Signature`] /
/// `Certificate`, which is exact regardless of template shape); the
/// determinism harness has no equivalent kind signal and relies on the
/// suffix set alone, so a dynamic-tail template stays unclassified there.
pub fn signature_template_suffix(template: &str) -> Option<String> {
    let tail = match template.rfind("}}") {
        Some(idx) => &template[idx + 2..],
        None => template,
    };
    let tail = tail.trim();
    if tail.len() < 2 || !tail.starts_with('.') {
        return None;
    }
    Some(tail.to_string())
}

/// Collect the distinct signature- and keyless-certificate-asset suffixes
/// configured across top-level and per-workspace `signs:` / `binary_signs:`.
///
/// `certificate:` (cosign keyless mode) has no default template — it's only
/// present when the user configures a cert output path — so it contributes
/// a suffix only when set.
///
/// Pure: no I/O, safe to call from both the determinism harness and
/// release verification without a cwd-dependent config load.
pub fn signature_asset_suffixes(cfg: &Config) -> BTreeSet<String> {
    let mut suffixes = BTreeSet::new();
    let mut collect = |entries: &[SignConfig], default_tmpl: &str| {
        for s in entries {
            if let Some(suffix) =
                signature_template_suffix(s.resolved_signature_template(default_tmpl))
            {
                suffixes.insert(suffix);
            }
            if let Some(suffix) = s.certificate.as_deref().and_then(signature_template_suffix) {
                suffixes.insert(suffix);
            }
        }
    };
    collect(&cfg.signs, SignConfig::DEFAULT_SIGNATURE_TEMPLATE);
    collect(
        &cfg.binary_signs,
        SignConfig::DEFAULT_BINARY_SIGNATURE_TEMPLATE,
    );
    for w in cfg.workspaces.iter().flatten() {
        collect(&w.signs, SignConfig::DEFAULT_SIGNATURE_TEMPLATE);
        collect(
            &w.binary_signs,
            SignConfig::DEFAULT_BINARY_SIGNATURE_TEMPLATE,
        );
    }
    suffixes
}

/// Whether `asset_name` is a signature artifact, given the suffix set from
/// [`signature_asset_suffixes`].
pub fn is_signature_asset(asset_name: &str, suffixes: &BTreeSet<String>) -> bool {
    suffixes.iter().any(|suffix| asset_name.ends_with(suffix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_template_suffix_extracts_literal_tail_after_last_expansion() {
        assert_eq!(
            signature_template_suffix("{{ .Artifact }}.cosign.bundle").as_deref(),
            Some(".cosign.bundle")
        );
        assert_eq!(
            signature_template_suffix("{{ .Artifact }}.sig").as_deref(),
            Some(".sig")
        );
        assert_eq!(
            signature_template_suffix("{{ .Artifact }}.asc").as_deref(),
            Some(".asc")
        );
    }

    #[test]
    fn signature_template_suffix_rejects_unanchorable_templates() {
        assert_eq!(signature_template_suffix("{{ .Artifact }}"), None);
        assert_eq!(signature_template_suffix("{{ .Artifact }}   "), None);
        assert_eq!(signature_template_suffix("{{ .Artifact }}."), None);
        assert_eq!(signature_template_suffix("{{ .Artifact }}sig"), None);
        assert_eq!(
            signature_template_suffix("{{ .Artifact }}.{{ .Format }}"),
            None
        );
    }

    #[test]
    fn signature_asset_suffixes_include_keyless_certificate_template() {
        let cfg = Config {
            signs: vec![SignConfig {
                certificate: Some("{{ .Artifact }}.pem".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let suffixes = signature_asset_suffixes(&cfg);
        assert!(
            suffixes.contains(".pem"),
            "a configured certificate: template must contribute its suffix: {suffixes:?}"
        );
    }

    #[test]
    fn is_signature_asset_matches_any_configured_suffix() {
        let mut suffixes = BTreeSet::new();
        suffixes.insert(".sig".to_string());
        suffixes.insert(".cosign.bundle".to_string());

        assert!(is_signature_asset("app.tar.gz.sig", &suffixes));
        assert!(is_signature_asset("app.tar.gz.cosign.bundle", &suffixes));
        assert!(!is_signature_asset("app.tar.gz", &suffixes));
    }
}
