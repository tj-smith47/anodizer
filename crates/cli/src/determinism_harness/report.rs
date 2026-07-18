use super::*;

impl Harness {
    /// Aggregate per-run hashes into the final report.
    pub(crate) fn build_report(
        &self,
        per_run_hashes: Vec<BTreeMap<String, ArtifactInfo>>,
    ) -> DeterminismReport {
        // Union of artifact names across runs — an artifact missing from
        // one run is itself a form of drift, surfaced as the run's hash
        // becoming `<missing>`.
        let mut all_names: BTreeSet<String> = BTreeSet::new();
        for run in &per_run_hashes {
            for name in run.keys() {
                all_names.insert(name.clone());
            }
        }

        let mut artifacts: Vec<ArtifactRow> = Vec::new();
        let mut drift: Vec<DriftRow> = Vec::new();
        let mut drift_count: u32 = 0;

        // Authoritative produced-artifact set, parsed from the run's
        // `artifacts.json` manifest. Any dist file whose basename appears
        // here is a tracked primary — this covers template / extra /
        // uploadable files whose extension `infer_stage_from_path` cannot
        // classify (e.g. `install.sh`).
        let manifest_members = self.produced_member_basenames(&per_run_hashes);
        // Basenames the manifest flags as combined checksums files via the
        // `combined = "true"` marker — the authoritative aggregate signal,
        // independent of the operator's chosen filename (e.g. `SHA512SUMS`).
        let combined_markers = self.produced_combined_markers(&per_run_hashes);

        for name in &all_names {
            let mut hashes: Vec<String> = Vec::with_capacity(per_run_hashes.len());
            // Use the LAST run that produced the artifact as the source
            // of truth for path/size (matches "last writer wins"
            // semantics for the cosmetic fields).
            let mut last_info: Option<&ArtifactInfo> = None;
            for run in &per_run_hashes {
                match run.get(name) {
                    Some(info) => {
                        hashes.push(info.hash.clone());
                        last_info = Some(info);
                    }
                    None => hashes.push("<missing>".into()),
                }
            }

            let info = last_info.expect("artifact name came from union of run maps");
            let all_equal =
                hashes.iter().all(|h| h == &hashes[0]) && !hashes.iter().any(|h| h == "<missing>");

            // Byte-equality is the determinism verdict; classification only
            // excuses a DRIFTING aggregate (below). An unclassified file fails
            // only when its bytes drift — a stable one cannot mask member
            // drift: every member is independently hashed and surfaces its own
            // drift row regardless of any aggregate that contains it.
            let classification =
                self.classify(name, &all_names, &manifest_members, &combined_markers);
            if matches!(classification, Classification::Unclassified) {
                artifacts.push(ArtifactRow {
                    name: name.clone(),
                    path: info.relative_path.clone(),
                    size_bytes: info.size_bytes,
                    stage: info.stage.clone(),
                    deterministic: all_equal,
                    nondeterministic_reason: None,
                    hash: if all_equal {
                        Some(hashes[0].clone())
                    } else {
                        None
                    },
                    hashes: if all_equal { vec![] } else { hashes.clone() },
                });
                if !all_equal {
                    drift.push(DriftRow {
                        artifact: name.clone(),
                        hashes,
                        differing_bytes_summary: Some(
                            "unclassified produced file drifted across runs; if it is a \
                             combined checksums file, mark it combined=true so its members \
                             can be evaluated — otherwise it is a real regression"
                                .into(),
                        ),
                    });
                    drift_count += 1;
                }
                continue;
            }

            // Transitive-derivation rule: a drifting aggregate is excused IFF
            // every differing member is itself allow-listed. An unexcused
            // member is a real regression; an aggregate whose members cannot
            // be reconstructed fails closed (never excused).
            let mut aggregate_excuse: Option<String> = None;
            if !all_equal && matches!(classification, Classification::Aggregate) {
                let kind = self
                    .aggregate_kind_for_name(name, &combined_markers)
                    .expect("Aggregate classification ⇒ a registered kind matches");
                match self.evaluate_aggregate(
                    kind.as_ref(),
                    name,
                    &per_run_hashes,
                    &combined_markers,
                ) {
                    AggregateVerdict::Excused(reason) => aggregate_excuse = Some(reason),
                    AggregateVerdict::Regression(members) => {
                        artifacts.push(ArtifactRow {
                            name: name.clone(),
                            path: info.relative_path.clone(),
                            size_bytes: info.size_bytes,
                            stage: info.stage.clone(),
                            deterministic: false,
                            nondeterministic_reason: None,
                            hash: None,
                            hashes: hashes.clone(),
                        });
                        // One drift row per aggregate (keeps the report's
                        // `drift_count == drift.len()` invariant); the
                        // offending members are named in both the artifact
                        // field and the summary.
                        let joined = members.join(", ");
                        drift.push(DriftRow {
                            artifact: format!("{name} → {joined}"),
                            hashes,
                            differing_bytes_summary: Some(format!(
                                "aggregate member(s) [{joined}] drifted and are not allow-listed; \
                                 a gated artifact regressed (surfaced via the {name} aggregate)"
                            )),
                        });
                        drift_count += 1;
                        continue;
                    }
                    AggregateVerdict::FailClosed(reason) => {
                        artifacts.push(ArtifactRow {
                            name: name.clone(),
                            path: info.relative_path.clone(),
                            size_bytes: info.size_bytes,
                            stage: info.stage.clone(),
                            deterministic: false,
                            nondeterministic_reason: None,
                            hash: None,
                            hashes: hashes.clone(),
                        });
                        drift.push(DriftRow {
                            artifact: name.clone(),
                            hashes,
                            differing_bytes_summary: Some(reason),
                        });
                        drift_count += 1;
                        continue;
                    }
                }
            }

            // Sign-stage drift auto-allowlist: cosign sign-blob uses
            // ECDSA P-256 with a random nonce, so its signature bytes
            // can never be byte-identical across runs. Byte-equality is
            // not the right determinism signal for signatures —
            // verification (`cosign verify-blob` / `gpg --verify`) is.
            let signed_artifact_drift = !all_equal && info.stage == "sign";
            let allow_reason = aggregate_excuse
                .or_else(|| self.resolve_allow_reason(name))
                .or_else(|| {
                    if signed_artifact_drift {
                        Some(
                            "signed artifact: signature bytes vary by signer \
                             (cosign ECDSA random nonce); validate via \
                             `cosign verify-blob` / `gpg --verify`"
                                .into(),
                        )
                    } else {
                        None
                    }
                });

            if all_equal {
                artifacts.push(ArtifactRow {
                    name: name.clone(),
                    path: info.relative_path.clone(),
                    size_bytes: info.size_bytes,
                    stage: info.stage.clone(),
                    deterministic: true,
                    nondeterministic_reason: allow_reason.clone(),
                    hash: Some(hashes[0].clone()),
                    hashes: vec![],
                });
            } else {
                artifacts.push(ArtifactRow {
                    name: name.clone(),
                    path: info.relative_path.clone(),
                    size_bytes: info.size_bytes,
                    stage: info.stage.clone(),
                    deterministic: false,
                    nondeterministic_reason: allow_reason.clone(),
                    hash: None,
                    hashes: hashes.clone(),
                });
                // Drift row + drift_count are gated on allow-list status:
                // allow-listed artifacts surface their per-run hashes via
                // the drift row (so the audit trail is complete) but DO
                // NOT bump `drift_count`.
                if allow_reason.is_none() {
                    let summary = summarize_drift(name, &per_run_hashes);
                    drift.push(DriftRow {
                        artifact: name.clone(),
                        hashes,
                        differing_bytes_summary: summary,
                    });
                    drift_count += 1;
                }
            }
        }

        DeterminismReport {
            schema_version: CURRENT_SCHEMA_VERSION,
            anodize_version: env!("CARGO_PKG_VERSION").into(),
            commit: self.commit.clone(),
            commit_timestamp: self.sde,
            runs: self.runs,
            stages_under_test: self.stages.iter().map(|s| s.as_str().into()).collect(),
            allowlist: self.allowlist.clone(),
            artifacts,
            drift,
            drift_count,
        }
    }
}
