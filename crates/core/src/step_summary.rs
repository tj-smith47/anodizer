//! The GitHub Actions job summary of what a run published.
//!
//! GitHub Actions names a markdown file in `GITHUB_STEP_SUMMARY`; whatever a
//! step appends there renders on the run's summary page. At pipeline end
//! anodizer appends one bullet per notable publish action the run completed —
//! a release published, a pull request opened, a package pushed, images
//! pushed, files uploaded — read off the run report and the stage outputs.
//! Outside GitHub Actions, and on a dry run or a snapshot, nothing is written.

use crate::context::Context;
use crate::log::StageLogger;
use crate::publish_evidence::{PublishEvidence, PublishEvidenceExtra};
use crate::publish_report::{PublishReport, PublisherOutcome, PublisherResult};
use std::collections::BTreeMap;

/// The variable GitHub Actions sets to the job summary file.
pub const ENV_VAR: &str = "GITHUB_STEP_SUMMARY";

/// Append `lines` as markdown bullets to the file `env(ENV_VAR)` names.
///
/// Nothing is written when the variable is unset or `lines` is empty. Each
/// line is masked through `log.redact` first. A write failure is a warning:
/// the summary is an observability extra, never a release gate.
pub fn append_lines(env: impl Fn(&str) -> Option<String>, lines: &[String], log: &StageLogger) {
    let Some(path) = env(ENV_VAR).filter(|p| !p.is_empty()) else {
        return;
    };
    if lines.is_empty() {
        return;
    }
    let body: String = lines
        .iter()
        .map(|line| format!("- {}\n", log.redact(line)))
        .collect();
    let written = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
        .and_then(|mut f| std::io::Write::write_all(&mut f, body.as_bytes()));
    if let Err(err) = written {
        log.warn(&format!("failed to write the job summary to {path}: {err}"));
    }
}

/// Write the run's job summary: nothing on a dry run or a snapshot, else
/// every line of [`summary_lines`] through [`append_lines`].
pub fn write(ctx: &Context, log: &StageLogger) {
    if ctx.is_dry_run() || ctx.is_snapshot() {
        return;
    }
    let lines = summary_lines(ctx.publish_report.as_ref(), ctx);
    append_lines(|name| ctx.env_var(name), &lines, log);
}

/// One line per thing the run published: the report's rows that stand for
/// published work, every pushed image, every custom publisher that ran.
pub fn summary_lines(report: Option<&PublishReport>, ctx: &Context) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(report) = report {
        for row in &report.results {
            if let Some(evidence) = published_evidence(row) {
                lines.extend(evidence_lines(evidence, ctx));
            }
        }
    }
    for image in ctx.artifacts.pushed_images() {
        lines.push(format!("Pushed image `{}`", image.reference));
    }
    for reference in ctx.artifacts.signed_images() {
        lines.push(format!("Signed image `{reference}`"));
    }
    for run in &ctx.stage_outputs.custom_publishers_run {
        lines.push(format!(
            "Ran custom publisher `{}` on {}",
            run.name,
            count(run.artifacts, "artifact")
        ));
    }
    lines
}

/// The evidence of a row whose outcome means the work it names is live.
///
/// A `Failed` row counts when its evidence records committed work: the part
/// it published before failing exists. A skipped row and a rolled-back row
/// published nothing that still stands.
fn published_evidence(row: &PublisherResult) -> Option<&PublishEvidence> {
    let evidence = row.evidence.as_ref()?;
    let published = match &row.outcome {
        PublisherOutcome::Succeeded
        | PublisherOutcome::PendingModeration
        | PublisherOutcome::PendingValidation
        | PublisherOutcome::RollbackFailed(_)
        | PublisherOutcome::RollbackSkippedNoScope
        | PublisherOutcome::PublishedNoRollback => true,
        PublisherOutcome::Failed(_) => evidence.records_published_work(),
        PublisherOutcome::Skipped(_) | PublisherOutcome::RolledBack => false,
    };
    published.then_some(evidence)
}

fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// Count `items` per key and render one `Uploaded N <noun>s to <key>` line per key.
fn uploaded_per<K: Ord + std::fmt::Display>(
    keys: impl IntoIterator<Item = K>,
    noun: &str,
) -> Vec<String> {
    let mut per: BTreeMap<K, usize> = BTreeMap::new();
    for key in keys {
        *per.entry(key).or_default() += 1;
    }
    per.into_iter()
        .map(|(key, n)| format!("Uploaded {} to {key}", count(n, noun)))
        .collect()
}

/// The lines one row's evidence stands for, one per published thing.
fn evidence_lines(evidence: &PublishEvidence, ctx: &Context) -> Vec<String> {
    use PublishEvidenceExtra as E;
    match &evidence.extra {
        E::Homebrew(x) => x
            .homebrew_targets
            .iter()
            .map(|t| tap_line("homebrew", &t.target, &t.repo_url, t.pr_url.as_deref()))
            .collect(),
        E::Scoop(x) => x
            .scoop_targets
            .iter()
            .map(|t| {
                tap_line(
                    "scoop manifest",
                    &t.target,
                    &t.repo_url,
                    t.pr_url.as_deref(),
                )
            })
            .collect(),
        E::Nix(x) => x
            .nix_targets
            .iter()
            .map(|t| tap_line("nix package", &t.target, &t.repo_url, t.pr_url.as_deref()))
            .collect(),
        E::Winget(x) => x
            .winget_targets
            .iter()
            .map(|t| {
                format!(
                    "Opened pull request to `{}/{}` (winget package `{}` {}) from `{}:{}`{}",
                    t.upstream_owner,
                    t.upstream_repo,
                    t.package_id,
                    t.version,
                    t.fork_owner,
                    t.branch,
                    url_tail(t.pr_url.as_deref())
                )
            })
            .collect(),
        E::Chocolatey(x) => x
            .chocolatey_targets
            .iter()
            .map(|t| format!("Pushed `{}` {} to Chocolatey", t.package_id, t.version))
            .collect(),
        E::Krew(x) => x
            .krew_targets
            .iter()
            .map(|t| {
                format!(
                    "Opened pull request to `{}/{}` (krew plugin `{}`) from `{}:{}`{}",
                    t.upstream_owner,
                    t.upstream_repo,
                    t.target,
                    t.fork_owner,
                    t.branch,
                    url_tail(t.pr_url.as_deref())
                )
            })
            .collect(),
        E::Aur(x) => x
            .aur_our_targets
            .iter()
            .map(|t| format!("Pushed AUR package `{}` to `{}`", t.target, t.git_url))
            .collect(),
        E::AurSource(x) => x
            .aur_source_targets
            .iter()
            .map(|t| {
                format!(
                    "Pushed AUR source package `{}` ({}) to `{}`",
                    t.package, t.tag, t.git_url
                )
            })
            .collect(),
        E::Mcp(x) => x
            .mcp_targets
            .iter()
            .map(|t| {
                format!(
                    "Published `{}` {} to the MCP registry `{}`",
                    t.server_name, t.version, t.registry_url
                )
            })
            .collect(),
        E::Dockerhub(x) => x
            .dockerhub_targets
            .iter()
            .map(|t| {
                format!(
                    "Updated Docker Hub description of `{}/{}`",
                    t.namespace, t.name
                )
            })
            .collect(),
        E::Cargo(x) => x
            .cargo_yank_targets
            .iter()
            .map(|t| {
                let registry = t.registry.as_deref().unwrap_or("crates.io");
                format!("Published `{}` {} to {registry}", t.name, t.version)
            })
            .collect(),
        E::Artifactory(x) => uploaded_per(
            x.artifactory_targets
                .iter()
                .map(|t| format!("`{}`", t.entry)),
            "file",
        ),
        E::Cloudsmith(x) => uploaded_per(
            x.cloudsmith_targets
                .iter()
                .map(|t| format!("Cloudsmith `{}/{}`", t.org, t.repo)),
            "package",
        ),
        E::Blob(x) => uploaded_per(
            x.blob_targets
                .iter()
                .map(|t| format!("`{}://{}`", t.provider, t.bucket)),
            "object",
        ),
        E::Snapcraft(x) => x
            .snapcraft_targets
            .iter()
            .filter(|t| t.revision.is_some())
            .map(|t| {
                let arch = t
                    .arch
                    .as_deref()
                    .map(|a| format!(" ({a})"))
                    .unwrap_or_default();
                let held = if t.held_for_review {
                    ", held for manual review"
                } else {
                    ""
                };
                format!(
                    "Pushed snap `{}`{arch} to the Snap Store{held}",
                    t.package_name
                )
            })
            .collect(),
        E::GithubRelease(x) => github_release_lines(x, ctx),
        E::Npm(x) => x
            .npm_targets
            .iter()
            .map(|t| {
                format!(
                    "Published `{}@{}` to npm ({})",
                    t.package, t.version, t.registry
                )
            })
            .collect(),
        E::GemFury(x) => x
            .gemfury_targets
            .iter()
            .map(|t| format!("Pushed `{}` to GemFury `{}`", t.package, t.account))
            .collect(),
        E::Pypi(x) => uploaded_per(
            x.pypi_files
                .iter()
                .filter(|f| !f.skipped_existing)
                .map(|f| f.repository.clone()),
            "file",
        ),
        E::Schemastore(x) => x
            .schemastore_targets
            .iter()
            .map(|t| {
                format!(
                    "Opened pull request to `{}/{}` (SchemaStore registration) from `{}:{}`{}",
                    t.upstream_owner,
                    t.upstream_repo,
                    t.fork_owner,
                    t.branch,
                    url_tail(t.pr_url.as_deref())
                )
            })
            .collect(),
        E::HomebrewCore(x) => x
            .homebrew_core_targets
            .iter()
            .map(|t| {
                let repo = format!("{}/{}", t.upstream_owner, t.upstream_repo);
                match (&t.pr_url, t.direct_commit) {
                    (Some(url), _) => format!(
                        "Opened pull request to `{repo}` (homebrew formula `{}` {}): {url}",
                        t.formula, t.version
                    ),
                    (None, true) => format!(
                        "Updated homebrew formula `{}` to {} in `{repo}`",
                        t.formula, t.version
                    ),
                    (None, false) => format!(
                        "Opened pull request to `{repo}` (homebrew formula `{}` {}) from `{}:{}`",
                        t.formula, t.version, t.head_owner, t.branch
                    ),
                }
            })
            .collect(),
        E::Empty => {
            if !evidence.records_published_work() {
                return Vec::new();
            }
            let what = match &evidence.primary_ref {
                Some(r) => format!("`{r}`"),
                None => count(evidence.artifact_paths.len(), "artifact"),
            };
            vec![format!("Published {what} via `{}`", evidence.publisher)]
        }
    }
}

/// `: <url>` when a pull request URL was recorded, nothing otherwise.
fn url_tail(pr_url: Option<&str>) -> String {
    pr_url.map(|u| format!(": {u}")).unwrap_or_default()
}

/// The line for a tap-style push (homebrew, scoop, nix). A push that opened
/// a pull request names the repository the request was opened against,
/// read off the URL, and the URL; a direct push names the repository pushed
/// to.
fn tap_line(kind: &str, target: &str, repo_url: &str, pr_url: Option<&str>) -> String {
    match pr_url {
        Some(url) => format!(
            "Opened pull request to `{}` ({kind} `{target}`): {url}",
            pull_request_repo(url).unwrap_or(repo_url)
        ),
        None => format!("Updated {kind} `{target}` in `{repo_url}`"),
    }
}

/// `owner/repo` of a `https://<host>/owner/repo/pull/N` URL. A URL whose
/// repository sits under a path prefix (a GitHub Enterprise Server under
/// `/github/`) names no slug and the caller falls back to the configured
/// repository.
fn pull_request_repo(url: &str) -> Option<&str> {
    let (repo, _) = url.rsplit_once("/pull/")?;
    let (_, slug) = repo.strip_prefix("https://")?.split_once('/')?;
    (slug.matches('/').count() == 1).then_some(slug)
}

/// One line per release, joined to the stage's own record of what it wrote
/// for the URL, the asset count and the draft flag. A draft is not reported:
/// nobody can download from it until it is published by hand. Crates sharing
/// one tag share one release, so the line is per (owner, repo, tag).
fn github_release_lines(
    x: &crate::publish_evidence::GithubReleaseExtra,
    ctx: &Context,
) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    x.github_release_targets
        .iter()
        .filter(|t| seen.insert((&t.owner, &t.repo, &t.tag)))
        .filter_map(|t| {
            let written = ctx
                .stage_outputs
                .releases_written
                .iter()
                .find(|w| w.owner == t.owner && w.repo == t.repo && w.tag == t.tag);
            match written {
                Some(w) if w.draft => None,
                Some(w) => Some(format!(
                    "Published {} to `{}/{}` with {}: {}",
                    t.tag,
                    t.owner,
                    t.repo,
                    count(w.assets, "asset"),
                    w.url
                )),
                None => Some(format!("Published {} to `{}/{}`", t.tag, t.owner, t.repo)),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::{Artifact, ArtifactKind, PUSHED_META, PUSHED_VALUE};
    use crate::context::{CustomPublisherRun, WrittenRelease};
    use crate::publish_evidence::*;
    use crate::publish_report::{PublisherGroup, SkipReason};
    use crate::test_helpers::{TestContextBuilder, test_logger};
    use strum::EnumCount;

    fn evidence(extra: PublishEvidenceExtra) -> PublishEvidence {
        PublishEvidence {
            schema_version: 1,
            publisher: "p".into(),
            primary_ref: None,
            artifact_paths: Vec::new(),
            artifact_digests: BTreeMap::new(),
            nondeterministic: None,
            extra,
        }
    }

    fn row(name: &str, outcome: PublisherOutcome, extra: PublishEvidenceExtra) -> PublisherResult {
        PublisherResult {
            name: name.into(),
            group: PublisherGroup::Manager,
            required: false,
            outcome,
            evidence: Some(evidence(extra)),
            entry_skips: Vec::new(),
        }
    }

    fn blob(n: usize) -> PublishEvidenceExtra {
        PublishEvidenceExtra::Blob(BlobExtra {
            blob_targets: (0..n)
                .map(|i| BlobTargetSnapshot {
                    provider: "s3".into(),
                    bucket: "my-bucket".into(),
                    key: format!("k{i}"),
                    ..Default::default()
                })
                .collect(),
        })
    }

    fn release_target() -> GithubReleaseTargetSnapshot {
        GithubReleaseTargetSnapshot {
            crate_name: "app".into(),
            owner: "acme".into(),
            repo: "app".into(),
            tag: "v1.2.3".into(),
            release_id: Some(7),
        }
    }

    fn written(draft: bool) -> WrittenRelease {
        WrittenRelease {
            crate_name: "app".into(),
            owner: "acme".into(),
            repo: "app".into(),
            tag: "v1.2.3".into(),
            url: "https://github.com/acme/app/releases/tag/v1.2.3".into(),
            draft,
            assets: 12,
        }
    }

    /// One populated evidence per variant; the count is held to the enum's
    /// so a new variant has to be added here and has to render a line.
    fn one_of_each() -> Vec<PublishEvidence> {
        use PublishEvidenceExtra as E;
        let s = |v: &str| v.to_string();
        let mut all = vec![
            evidence(E::Homebrew(HomebrewExtra {
                homebrew_targets: vec![HomebrewTargetSnapshot {
                    target: s("app"),
                    repo_url: s("https://github.com/acme/homebrew-tap"),
                    ..Default::default()
                }],
            })),
            evidence(E::Scoop(ScoopExtra {
                scoop_targets: vec![ScoopTargetSnapshot {
                    target: s("app"),
                    repo_url: s("https://github.com/acme/scoop-bucket"),
                    ..Default::default()
                }],
            })),
            evidence(E::Nix(NixExtra {
                nix_targets: vec![NixTargetSnapshot {
                    target: s("app"),
                    repo_url: s("https://github.com/acme/nur"),
                    ..Default::default()
                }],
            })),
            evidence(E::Winget(WingetExtra {
                winget_targets: vec![WingetTargetSnapshot {
                    target: s("app"),
                    package_id: s("Acme.App"),
                    version: s("1.2.3"),
                    upstream_owner: s("microsoft"),
                    upstream_repo: s("winget-pkgs"),
                    fork_owner: s("acme"),
                    branch: s("acme-app-1.2.3"),
                    ..Default::default()
                }],
            })),
            evidence(E::Chocolatey(ChocolateyExtra {
                chocolatey_targets: vec![ChocolateyTargetSnapshot {
                    package_id: s("app"),
                    version: s("1.2.3"),
                    ..Default::default()
                }],
            })),
            evidence(E::Krew(KrewExtra {
                krew_targets: vec![KrewTargetSnapshot {
                    target: s("app"),
                    upstream_owner: s("kubernetes-sigs"),
                    upstream_repo: s("krew-index"),
                    fork_owner: s("acme"),
                    branch: s("app-1.2.3"),
                    ..Default::default()
                }],
            })),
            evidence(E::Aur(AurExtra {
                aur_our_targets: vec![AurTargetSnapshot {
                    target: s("app-bin"),
                    git_url: s("ssh://aur@aur.archlinux.org/app-bin.git"),
                    ..Default::default()
                }],
            })),
            evidence(E::AurSource(AurSourceExtra {
                aur_source_targets: vec![AurSourceTargetSnapshot {
                    target: s("app"),
                    package: s("app"),
                    tag: s("v1.2.3"),
                    git_url: s("ssh://aur@aur.archlinux.org/app.git"),
                }],
            })),
            evidence(E::Mcp(McpExtra {
                mcp_targets: vec![McpTargetSnapshot {
                    target: s("app"),
                    server_name: s("io.github.acme/app"),
                    registry_url: s("https://registry.modelcontextprotocol.io"),
                    version: s("1.2.3"),
                    ..Default::default()
                }],
            })),
            evidence(E::Dockerhub(DockerhubExtra {
                dockerhub_targets: vec![DockerhubTargetSnapshot {
                    namespace: s("acme"),
                    name: s("app"),
                    ..Default::default()
                }],
            })),
            evidence(E::Cargo(CargoExtra {
                cargo_yank_targets: vec![CargoYankTargetSnapshot {
                    name: s("app"),
                    version: s("1.2.3"),
                    ..Default::default()
                }],
            })),
            evidence(E::Artifactory(ArtifactoryExtra {
                artifactory_targets: vec![ArtifactoryTargetSnapshot {
                    entry: s("production"),
                    url: s("https://artifactory.example/repo/app.tar.gz"),
                }],
            })),
            evidence(E::Cloudsmith(CloudsmithExtra {
                cloudsmith_targets: vec![CloudsmithTargetSnapshot {
                    org: s("acme"),
                    repo: s("stable"),
                    filename: s("app.deb"),
                    slug: None,
                }],
            })),
            evidence(blob(1)),
            evidence(E::Snapcraft(SnapcraftExtra {
                snapcraft_targets: vec![SnapcraftTargetSnapshot {
                    crate_name: s("app"),
                    package_name: s("app"),
                    arch: Some(s("amd64")),
                    revision: Some(s("42")),
                    ..Default::default()
                }],
            })),
            evidence(E::GithubRelease(GithubReleaseExtra {
                github_release_targets: vec![release_target()],
            })),
            evidence(E::Npm(NpmExtra {
                npm_targets: vec![NpmTargetSnapshot {
                    package: s("@acme/app"),
                    version: s("1.2.3"),
                    registry: s("https://registry.npmjs.org"),
                    ..Default::default()
                }],
            })),
            evidence(E::GemFury(GemFuryExtra {
                gemfury_targets: vec![GemFuryTargetSnapshot {
                    account: s("acme"),
                    package: s("app_1.2.3_amd64.deb"),
                    ..Default::default()
                }],
            })),
            evidence(E::Pypi(PypiExtra {
                pypi_files: vec![PypiFileSnapshot {
                    filename: s("app-1.2.3.tar.gz"),
                    repository: s("https://upload.pypi.org/legacy/"),
                    ..Default::default()
                }],
            })),
            evidence(E::Schemastore(SchemastoreExtra {
                schemastore_targets: vec![SchemastoreTargetSnapshot {
                    upstream_owner: s("SchemaStore"),
                    upstream_repo: s("schemastore"),
                    fork_owner: s("acme"),
                    branch: s("app-schema"),
                    token_env_var: None,
                    pr_url: None,
                }],
            })),
            evidence(E::HomebrewCore(HomebrewCoreExtra {
                homebrew_core_targets: vec![HomebrewCoreTargetSnapshot {
                    formula: s("app"),
                    version: s("1.2.3"),
                    upstream_owner: s("Homebrew"),
                    upstream_repo: s("homebrew-core"),
                    pr_url: Some(s("https://github.com/Homebrew/homebrew-core/pull/1")),
                    ..Default::default()
                }],
            })),
        ];
        let mut empty = evidence(E::Empty);
        empty.primary_ref = Some(s("https://example.test/app"));
        all.push(empty);
        all
    }

    #[test]
    fn every_evidence_variant_renders_a_line() {
        let all = one_of_each();
        assert_eq!(
            all.len(),
            PublishEvidenceExtra::COUNT,
            "one fixture per PublishEvidenceExtra variant"
        );
        let ctx = TestContextBuilder::new().build();
        let mut discriminants = std::collections::HashSet::new();
        for e in &all {
            assert!(
                discriminants.insert(std::mem::discriminant(&e.extra)),
                "two fixtures of one variant: {:?}",
                e.extra
            );
            let lines = evidence_lines(e, &ctx);
            assert_eq!(
                lines.len(),
                1,
                "{:?} renders one line, got {lines:?}",
                e.extra
            );
            assert!(!lines[0].trim().is_empty());
        }
    }

    #[test]
    fn the_variable_unset_writes_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("summary.md");
        append_lines(|_| None, &["x".into()], test_logger());
        append_lines(|_| Some(String::new()), &["x".into()], test_logger());
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn bullets_are_appended_after_existing_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("summary.md");
        std::fs::write(&path, "## earlier step\n").unwrap();
        let env = |_: &str| Some(path.display().to_string());
        append_lines(env, &["one".into(), "two".into()], test_logger());
        append_lines(env, &[], test_logger());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "## earlier step\n- one\n- two\n"
        );
    }

    #[test]
    fn a_missing_directory_is_a_warning_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent").join("summary.md");
        let (log, capture) = StageLogger::with_capture("announce", crate::log::Verbosity::Normal);
        append_lines(|_| Some(path.display().to_string()), &["one".into()], &log);
        let warnings = capture.warn_messages();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].starts_with("failed to write the job summary to "));
    }

    fn report(rows: Vec<PublisherResult>) -> PublishReport {
        PublishReport {
            results: rows,
            ..Default::default()
        }
    }

    #[test]
    fn a_failed_row_lists_what_it_pushed_and_a_skipped_row_nothing() {
        let ctx = TestContextBuilder::new().build();
        let r = report(vec![
            row(
                "blob",
                PublisherOutcome::Failed("third object".into()),
                blob(2),
            ),
            row(
                "homebrew",
                PublisherOutcome::Skipped(SkipReason::AlreadyPublished),
                PublishEvidenceExtra::Homebrew(HomebrewExtra {
                    homebrew_targets: vec![HomebrewTargetSnapshot {
                        target: "app".into(),
                        repo_url: "https://github.com/acme/homebrew-tap".into(),
                        ..Default::default()
                    }],
                }),
            ),
            row(
                "npm",
                PublisherOutcome::Failed("nothing".into()),
                PublishEvidenceExtra::Npm(NpmExtra::default()),
            ),
            row(
                "scoop",
                PublisherOutcome::RolledBack,
                PublishEvidenceExtra::Scoop(ScoopExtra {
                    scoop_targets: vec![ScoopTargetSnapshot {
                        target: "app".into(),
                        ..Default::default()
                    }],
                }),
            ),
            row(
                "cargo",
                PublisherOutcome::PendingModeration,
                PublishEvidenceExtra::Cargo(CargoExtra {
                    cargo_yank_targets: vec![CargoYankTargetSnapshot {
                        name: "app".into(),
                        version: "1.2.3".into(),
                        ..Default::default()
                    }],
                }),
            ),
        ]);
        assert_eq!(
            summary_lines(Some(&r), &ctx),
            vec![
                "Uploaded 2 objects to `s3://my-bucket`",
                "Published `app` 1.2.3 to crates.io",
            ]
        );
    }

    #[test]
    fn a_draft_release_is_not_reported_and_a_published_one_names_assets_and_url() {
        let r = report(vec![row(
            "github-release",
            PublisherOutcome::Succeeded,
            PublishEvidenceExtra::GithubRelease(GithubReleaseExtra {
                github_release_targets: vec![release_target(), release_target()],
            }),
        )]);
        let mut ctx = TestContextBuilder::new().build();
        ctx.stage_outputs.releases_written.push(written(true));
        assert!(
            summary_lines(Some(&r), &ctx).is_empty(),
            "a draft is not published"
        );

        ctx.stage_outputs.releases_written.clear();
        ctx.stage_outputs.releases_written.push(written(false));
        assert_eq!(
            summary_lines(Some(&r), &ctx),
            vec![
                "Published v1.2.3 to `acme/app` with 12 assets: https://github.com/acme/app/releases/tag/v1.2.3"
            ],
            "two crates on one tag are one release"
        );
    }

    fn pushed_image(reference: &str, pushed: bool) -> Artifact {
        let mut metadata = std::collections::HashMap::new();
        if pushed {
            metadata.insert(PUSHED_META.to_string(), PUSHED_VALUE.to_string());
        }
        Artifact {
            kind: ArtifactKind::DockerImageV2,
            name: reference.into(),
            path: reference.into(),
            target: None,
            crate_name: "app".into(),
            metadata,
            size: None,
        }
    }

    #[test]
    fn a_tap_push_that_opened_a_pull_request_names_the_upstream_and_the_url() {
        let ctx = TestContextBuilder::new().build();
        let url = "https://github.com/acme/homebrew-tap/pull/12";
        let r = report(vec![
            row(
                "homebrew",
                PublisherOutcome::Succeeded,
                PublishEvidenceExtra::Homebrew(HomebrewExtra {
                    homebrew_targets: vec![HomebrewTargetSnapshot {
                        target: "app".into(),
                        repo_url: "https://github.com/me/homebrew-tap.git".into(),
                        pr_url: Some(url.into()),
                        ..Default::default()
                    }],
                }),
            ),
            row(
                "scoop",
                PublisherOutcome::Succeeded,
                PublishEvidenceExtra::Scoop(ScoopExtra {
                    scoop_targets: vec![ScoopTargetSnapshot {
                        target: "app".into(),
                        repo_url: "https://github.com/acme/scoop-bucket.git".into(),
                        ..Default::default()
                    }],
                }),
            ),
            row(
                "nix",
                PublisherOutcome::Succeeded,
                PublishEvidenceExtra::Nix(NixExtra {
                    nix_targets: vec![NixTargetSnapshot {
                        target: "app".into(),
                        repo_url: "https://github.com/me/nur.git".into(),
                        pr_url: Some("https://github.com/acme/nur/pull/3".into()),
                        ..Default::default()
                    }],
                }),
            ),
        ]);
        assert_eq!(
            summary_lines(Some(&r), &ctx),
            vec![
                format!("Opened pull request to `acme/homebrew-tap` (homebrew `app`): {url}"),
                "Updated scoop manifest `app` in `https://github.com/acme/scoop-bucket.git`".to_string(),
                "Opened pull request to `acme/nur` (nix package `app`): https://github.com/acme/nur/pull/3"
                    .to_string(),
            ]
        );
    }

    #[test]
    fn the_pull_request_repository_is_read_off_a_two_segment_url_only() {
        assert_eq!(
            pull_request_repo("https://github.com/acme/homebrew-tap/pull/7"),
            Some("acme/homebrew-tap")
        );
        assert_eq!(
            pull_request_repo("https://ghe.example.com/github/acme/tap/pull/7"),
            None,
            "a path prefix leaves the slug unreadable"
        );
        assert_eq!(pull_request_repo("http://github.com/acme/tap/pull/7"), None);
        assert_eq!(
            tap_line(
                "homebrew",
                "app",
                "https://ghe.example.com/github/acme/tap.git",
                Some("https://ghe.example.com/github/acme/tap/pull/7"),
            ),
            "Opened pull request to `https://ghe.example.com/github/acme/tap.git` (homebrew `app`): \
             https://ghe.example.com/github/acme/tap/pull/7",
            "the configured repository is the fallback"
        );
    }

    #[test]
    fn pull_request_publishers_append_the_url_when_one_was_recorded() {
        let ctx = TestContextBuilder::new().build();
        let s = |v: &str| v.to_string();
        let r = report(vec![
            row(
                "winget",
                PublisherOutcome::Succeeded,
                PublishEvidenceExtra::Winget(WingetExtra {
                    winget_targets: vec![WingetTargetSnapshot {
                        target: s("app"),
                        crate_name: s("app"),
                        package_id: s("Acme.App"),
                        version: s("1.2.3"),
                        upstream_owner: s("microsoft"),
                        upstream_repo: s("winget-pkgs"),
                        fork_owner: s("acme"),
                        branch: s("Acme.App-1.2.3"),
                        pr_url: Some(s("https://github.com/microsoft/winget-pkgs/pull/9")),
                    }],
                }),
            ),
            row(
                "krew",
                PublisherOutcome::Succeeded,
                PublishEvidenceExtra::Krew(KrewExtra {
                    krew_targets: vec![KrewTargetSnapshot {
                        target: s("app"),
                        upstream_owner: s("kubernetes-sigs"),
                        upstream_repo: s("krew-index"),
                        fork_owner: s("acme"),
                        branch: s("app-1.2.3"),
                        token_env_var: None,
                        pr_url: Some(s("https://github.com/kubernetes-sigs/krew-index/pull/5")),
                    }],
                }),
            ),
            row(
                "schemastore",
                PublisherOutcome::Succeeded,
                PublishEvidenceExtra::Schemastore(SchemastoreExtra {
                    schemastore_targets: vec![SchemastoreTargetSnapshot {
                        upstream_owner: s("SchemaStore"),
                        upstream_repo: s("schemastore"),
                        fork_owner: s("acme"),
                        branch: s("app-schema"),
                        token_env_var: None,
                        pr_url: None,
                    }],
                }),
            ),
        ]);
        assert_eq!(
            summary_lines(Some(&r), &ctx),
            vec![
                "Opened pull request to `microsoft/winget-pkgs` (winget package `Acme.App` 1.2.3) from `acme:Acme.App-1.2.3`: https://github.com/microsoft/winget-pkgs/pull/9",
                "Opened pull request to `kubernetes-sigs/krew-index` (krew plugin `app`) from `acme:app-1.2.3`: https://github.com/kubernetes-sigs/krew-index/pull/5",
                "Opened pull request to `SchemaStore/schemastore` (SchemaStore registration) from `acme:app-schema`",
            ]
        );
    }

    #[test]
    fn a_signed_image_is_listed_once_after_the_pushed_ones() {
        let mut ctx = TestContextBuilder::new().build();
        ctx.artifacts
            .add(pushed_image("ghcr.io/acme/app:1.2.3", true));
        ctx.artifacts
            .add(pushed_image("ghcr.io/acme/app:1.2.3-amd64", true));
        ctx.artifacts
            .add(pushed_image("ghcr.io/acme/app:1.2.3-amd64", true));
        ctx.artifacts
            .mark_image_signed(std::path::Path::new("ghcr.io/acme/app:1.2.3-amd64"));
        assert_eq!(
            summary_lines(None, &ctx),
            vec![
                "Pushed image `ghcr.io/acme/app:1.2.3`",
                "Pushed image `ghcr.io/acme/app:1.2.3-amd64`",
                "Signed image `ghcr.io/acme/app:1.2.3-amd64`",
            ]
        );
    }

    #[test]
    fn pushed_images_and_custom_publishers_are_listed() {
        let mut ctx = TestContextBuilder::new().build();
        ctx.artifacts
            .add(pushed_image("ghcr.io/acme/app:1.2.3", true));
        ctx.artifacts
            .add(pushed_image("ghcr.io/acme/app:local", false));
        ctx.stage_outputs
            .custom_publishers_run
            .push(CustomPublisherRun {
                name: "mirror".into(),
                artifacts: 1,
            });
        ctx.stage_outputs
            .custom_publishers_run
            .push(CustomPublisherRun {
                name: "notify".into(),
                artifacts: 3,
            });
        assert_eq!(
            summary_lines(None, &ctx),
            vec![
                "Pushed image `ghcr.io/acme/app:1.2.3`",
                "Ran custom publisher `mirror` on 1 artifact",
                "Ran custom publisher `notify` on 3 artifacts",
            ]
        );
    }

    #[test]
    fn a_dry_run_and_a_snapshot_write_nothing_and_a_release_writes_bullets() {
        for (dry_run, snapshot) in [(true, false), (false, true), (false, false)] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("summary.md");
            let mut ctx = TestContextBuilder::new()
                .dry_run(dry_run)
                .snapshot(snapshot)
                .env(ENV_VAR, path.display().to_string())
                .build();
            ctx.publish_report = Some(report(vec![row(
                "blob",
                PublisherOutcome::Succeeded,
                blob(1),
            )]));
            write(&ctx, test_logger());
            if dry_run || snapshot {
                assert!(!path.exists(), "dry_run={dry_run} snapshot={snapshot}");
            } else {
                assert_eq!(
                    std::fs::read_to_string(&path).unwrap(),
                    "- Uploaded 1 object to `s3://my-bucket`\n"
                );
            }
        }
    }

    #[test]
    fn a_secret_in_a_line_is_masked_before_it_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("summary.md");
        let log = StageLogger::new("announce", crate::log::Verbosity::Normal)
            .with_job_env([("UPLOAD_TOKEN".to_string(), "hunter2hunter2".to_string())]);
        append_lines(
            |_| Some(path.display().to_string()),
            &["Uploaded 1 file to `https://x/hunter2hunter2`".into()],
            &log,
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("hunter2hunter2"), "{text}");
        assert!(text.contains("$UPLOAD_TOKEN"), "{text}");
    }
}
