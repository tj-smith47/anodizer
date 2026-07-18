use super::*;

impl Harness {
    /// Match `artifact_name` against the harness allow-list. Compile-time
    /// entries win on collision.
    pub(crate) fn resolve_allow_reason(&self, artifact_name: &str) -> Option<String> {
        for entry in &self.allowlist.compile_time {
            if matches_artifact_pattern(&entry.artifact, artifact_name) {
                return Some(entry.reason.clone());
            }
        }
        for entry in &self.allowlist.runtime {
            if matches_artifact_pattern(&entry.artifact, artifact_name) {
                return Some(entry.reason.clone());
            }
        }
        None
    }

    /// Parse the run's `artifacts.json` manifest(s) into the set of produced
    /// member basenames. This is the authoritative "what did we produce"
    /// list, so any dist file whose basename appears here is a tracked
    /// primary regardless of its extension (covers `template_files` /
    /// `extra_files` / uploadable files). Reads the LAST run's manifest;
    /// the path set is identical across runs (only member digests drift).
    /// Best-effort: an absent or unparseable manifest yields an empty set
    /// (callers fall back to extension- and allow-list-based classification).
    pub(crate) fn produced_member_basenames(
        &self,
        per_run_hashes: &[BTreeMap<String, ArtifactInfo>],
    ) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let Some(run) = per_run_hashes.last() else {
            return out;
        };
        for (name, info) in run {
            if !anodizer_core::determinism::ArtifactsManifest.matches(name) {
                continue;
            }
            let Some(full) = info.full.as_deref() else {
                continue;
            };
            if let Ok(units) = anodizer_core::determinism::ArtifactsManifest.members_by_unit(full) {
                out.extend(units.into_values());
            }
        }
        out
    }

    /// Parse the run's `artifacts.json` manifest(s) into the set of basenames
    /// flagged as combined checksums files via the `combined = "true"` marker.
    /// This is the authoritative recognizer for the combined-checksums
    /// aggregate — it catches an operator-renamed file (`SHA512SUMS`) that the
    /// filename-suffix heuristic cannot. Best-effort: an absent / unparseable
    /// manifest yields an empty set (callers fall back to the suffix match).
    pub(crate) fn produced_combined_markers(
        &self,
        per_run_hashes: &[BTreeMap<String, ArtifactInfo>],
    ) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let Some(run) = per_run_hashes.last() else {
            return out;
        };
        for (name, info) in run {
            if !anodizer_core::determinism::ArtifactsManifest.matches(name) {
                continue;
            }
            let Some(full) = info.full.as_deref() else {
                continue;
            };
            if let Ok(markers) =
                anodizer_core::determinism::combined_checksum_members_from_manifest(full)
            {
                out.extend(markers);
            }
        }
        out
    }

    /// Resolve the [`AggregateKind`] for `name`, consulting the manifest's
    /// `combined = "true"` markers as well as the filename-suffix registry.
    /// The marker path lets an operator-renamed combined file (`SHA512SUMS`)
    /// be recognized as a [`CombinedChecksums`] aggregate.
    pub(crate) fn aggregate_kind_for_name(
        &self,
        name: &str,
        combined_markers: &BTreeSet<String>,
    ) -> Option<Box<dyn anodizer_core::determinism::AggregateKind>> {
        if let Some(kind) = anodizer_core::determinism::aggregate_kind_for(name) {
            return Some(kind);
        }
        if combined_markers.contains(basename(name)) {
            return Some(Box::new(anodizer_core::determinism::CombinedChecksums));
        }
        None
    }

    /// Classify a produced dist file. Order matters: a registered aggregate
    /// is recognized before the sidecar / primary checks so a combined
    /// `checksums.txt` is never mislabeled a plain checksum primary.
    pub(crate) fn classify(
        &self,
        name: &str,
        all_names: &BTreeSet<String>,
        manifest_members: &BTreeSet<String>,
        combined_markers: &BTreeSet<String>,
    ) -> Classification {
        if self
            .aggregate_kind_for_name(name, combined_markers)
            .is_some()
        {
            return Classification::Aggregate;
        }
        // Sidecar: a `.sha256` / `.sig` whose stripped stem names a primary.
        if let Some(stem) = strip_sidecar_suffix(name) {
            let stem_is_primary = self.is_primary(stem, manifest_members)
                || all_names
                    .iter()
                    .any(|n| n.as_str() == stem || basename(n) == basename(stem));
            if stem_is_primary {
                return Classification::Sidecar;
            }
        }
        if self.is_primary(name, manifest_members) {
            return Classification::Primary;
        }
        Classification::Unclassified
    }

    /// A *primary* artifact: a recognized build/stage output (known
    /// `infer_stage_from_path` attribution), an intrinsically
    /// non-deterministic allow-listed format, the explicitly-tracked
    /// `metadata.json`, or any file the run's manifest declares it produced.
    pub(crate) fn is_primary(&self, name: &str, manifest_members: &BTreeSet<String>) -> bool {
        let base = basename(name);
        infer_stage_from_path(name) != "unknown"
            || self.resolve_allow_reason(name).is_some()
            // `metadata.json` is a tracked primary (expected byte-stable);
            // its pass is explicit, not incidental.
            || base == anodizer_core::dist::METADATA_JSON
            || manifest_members.contains(base)
    }

    /// Apply the transitive-derivation rule to a drifting aggregate.
    ///
    /// Reconstructs each run's members from the aggregate's full bytes and
    /// computes the set of *differing* members (a unit absent from any run —
    /// added, removed, or value-changed). The aggregate is excused IFF every
    /// differing member is itself allow-listed; any unexcused member is a
    /// real regression. Fails closed when the bytes are missing / uncaptured
    /// / unparseable, or when bytes drifted yet no member unit changed
    /// (structural drift we cannot attribute).
    pub(crate) fn evaluate_aggregate(
        &self,
        kind: &dyn anodizer_core::determinism::AggregateKind,
        name: &str,
        per_run_hashes: &[BTreeMap<String, ArtifactInfo>],
        combined_markers: &BTreeSet<String>,
    ) -> AggregateVerdict {
        let mut visited: BTreeSet<String> = BTreeSet::new();
        visited.insert(name.to_string());
        self.evaluate_aggregate_inner(kind, name, per_run_hashes, combined_markers, &mut visited)
    }

    /// Recursive core of [`Self::evaluate_aggregate`]. `visited` tracks the
    /// aggregate names already on the evaluation stack so a nested aggregate
    /// that (pathologically) lists itself fails closed instead of recursing
    /// forever.
    pub(crate) fn evaluate_aggregate_inner(
        &self,
        kind: &dyn anodizer_core::determinism::AggregateKind,
        name: &str,
        per_run_hashes: &[BTreeMap<String, ArtifactInfo>],
        combined_markers: &BTreeSet<String>,
        visited: &mut BTreeSet<String>,
    ) -> AggregateVerdict {
        let mut maps: Vec<BTreeMap<String, String>> = Vec::with_capacity(per_run_hashes.len());
        for run in per_run_hashes {
            let Some(info) = run.get(name) else {
                return AggregateVerdict::FailClosed(format!(
                    "aggregate {name} missing from a run — cannot reconstruct members; \
                     treated as real drift"
                ));
            };
            let Some(full) = info.full.as_deref() else {
                return AggregateVerdict::FailClosed(format!(
                    "aggregate {name} full bytes not captured — cannot reconstruct members; \
                     treated as real drift"
                ));
            };
            match kind.members_by_unit(full) {
                Ok(m) => maps.push(m),
                Err(e) => {
                    return AggregateVerdict::FailClosed(format!(
                        "aggregate {name} failed to parse ({e:#}); treated as real drift"
                    ));
                }
            }
        }
        let n = maps.len();
        let mut all_keys: BTreeSet<&String> = BTreeSet::new();
        for m in &maps {
            all_keys.extend(m.keys());
        }
        let mut differing_members: BTreeSet<String> = BTreeSet::new();
        for key in all_keys {
            let present = maps.iter().filter(|m| m.contains_key(key)).count();
            if present < n
                && let Some(member) = maps.iter().find_map(|m| m.get(key))
            {
                differing_members.insert(member.clone());
            }
        }
        if differing_members.is_empty() {
            return AggregateVerdict::FailClosed(format!(
                "aggregate {name} bytes drifted but no member unit changed \
                 (structural / ordering drift); treated as real drift"
            ));
        }
        let mut unexcused: Vec<String> = Vec::new();
        for member in &differing_members {
            match self.member_excused(member, per_run_hashes, combined_markers, visited) {
                Ok(true) => {}
                Ok(false) => unexcused.push(member.clone()),
                Err(reason) => return AggregateVerdict::FailClosed(reason),
            }
        }
        if unexcused.is_empty() {
            AggregateVerdict::Excused(format!(
                "aggregate of derived rows: every differing member ({}) is allow-listed \
                 non-deterministic; each member is drift-checked independently",
                differing_members
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        } else {
            AggregateVerdict::Regression(unexcused)
        }
    }

    /// Whether a *differing* aggregate member is excused.
    ///
    /// A member is excused when it is directly allow-listed, OR when it is
    /// itself a (nested) aggregate whose own members all resolve as excused —
    /// the transitive rule applied recursively (`artifacts.json` ⊃
    /// `checksums.txt` ⊃ per-artifact rows). `Err` is fail-closed: the nested
    /// aggregate's bytes are missing / unparseable, or a membership cycle was
    /// hit — the caller treats it as real drift, never an excuse.
    pub(crate) fn member_excused(
        &self,
        member: &str,
        per_run_hashes: &[BTreeMap<String, ArtifactInfo>],
        combined_markers: &BTreeSet<String>,
        visited: &mut BTreeSet<String>,
    ) -> Result<bool, String> {
        if self.resolve_allow_reason(member).is_some() {
            return Ok(true);
        }
        let Some(kind) = self.aggregate_kind_for_name(member, combined_markers) else {
            return Ok(false);
        };
        // Resolve the basename `member` back to the actual artifact key so we
        // can fetch its full bytes (member came from a parent aggregate's
        // member map, where it is recorded as a bare basename).
        let agg_name = per_run_hashes
            .last()
            .and_then(|run| run.keys().find(|k| basename(k) == member).cloned());
        let Some(agg_name) = agg_name else {
            return Err(format!(
                "nested aggregate member {member} could not be located among produced \
                 artifacts — cannot verify its members; treated as real drift"
            ));
        };
        if !visited.insert(agg_name.clone()) {
            return Err(format!(
                "aggregate membership cycle detected at {agg_name}; treated as real drift"
            ));
        }
        let verdict = self.evaluate_aggregate_inner(
            kind.as_ref(),
            &agg_name,
            per_run_hashes,
            combined_markers,
            visited,
        );
        match verdict {
            AggregateVerdict::Excused(_) => Ok(true),
            AggregateVerdict::Regression(_) => Ok(false),
            AggregateVerdict::FailClosed(reason) => Err(reason),
        }
    }
}
