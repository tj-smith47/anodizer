//! Pure plan-builder for `anodize bump`.
//!
//! Walks the workspace, resolves each member's current version, computes the
//! next version from `BumpOpts`, and emits one `PlanRow` per crate. No IO,
//! no filesystem writes; `run()` handles both.

use anyhow::{Context, Result, bail};
use semver::Version;
use serde::Serialize;
use std::path::{Path, PathBuf};

use super::BumpOpts;
use super::cargo_edit;
use super::inference;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BumpLevel {
    Major,
    Minor,
    Patch,
    Explicit,
    Release,
    Skip,
}

impl BumpLevel {
    fn label(self) -> &'static str {
        match self {
            BumpLevel::Major => "major",
            BumpLevel::Minor => "minor",
            BumpLevel::Patch => "patch",
            BumpLevel::Explicit => "exact",
            BumpLevel::Release => "release",
            BumpLevel::Skip => "skip",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanRow {
    #[serde(rename = "crate")]
    pub crate_name: String,
    pub current: String,
    pub next: String,
    #[serde(serialize_with = "ser_level_label")]
    pub level: BumpLevel,
    pub reason: String,
    /// Paths of `Cargo.toml` files that will be edited for this crate.
    #[serde(skip)]
    pub edited_files: Vec<PathBuf>,
    /// Workspace-relative path of the crate's manifest (for git/diff reporting).
    #[serde(skip)]
    pub manifest: PathBuf,
    /// Whether this crate inherits `version.workspace = true`.
    #[serde(skip)]
    pub inherits_workspace_version: bool,
}

fn ser_level_label<S: serde::Serializer>(lv: &BumpLevel, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(lv.label())
}

/// Parsed (but not dispatched) representation of the positional argument.
#[derive(Debug, Clone)]
enum Positional {
    Infer,
    Level(BumpLevel),
    Explicit(Version),
    Release,
}

fn parse_positional(arg: &Option<String>) -> Result<Positional> {
    let Some(raw) = arg.as_deref() else {
        return Ok(Positional::Infer);
    };
    match raw {
        "patch" => Ok(Positional::Level(BumpLevel::Patch)),
        "minor" => Ok(Positional::Level(BumpLevel::Minor)),
        "major" => Ok(Positional::Level(BumpLevel::Major)),
        "release" => Ok(Positional::Release),
        other => {
            let v = Version::parse(other).with_context(|| {
                format!(
                    "unrecognized bump argument '{}': expected patch|minor|major|release or a semver version",
                    other
                )
            })?;
            Ok(Positional::Explicit(v))
        }
    }
}

/// Walk the workspace and build the plan.
pub fn build_plan(workspace_root: &Path, opts: &BumpOpts) -> Result<Vec<PlanRow>> {
    let ws = cargo_edit::load_workspace(workspace_root)?;
    let positional = parse_positional(&opts.level_or_version)?;
    // Optional `.anodize.yaml` lookup so per-crate `tag_template` overrides
    // bump's `<crate-name>-v` fallback. Without this, crates whose tag is
    // bare `v{{ Version }}` (e.g. cfgd's primary crate) match no tag and
    // inference scans all-of-history instead of the just-released range.
    let anodize_cfg: Option<anodize_core::config::Config> = {
        let cfg_path = match opts.config_override.as_deref() {
            Some(p) => p.to_path_buf(),
            None => workspace_root.join(".anodize.yaml"),
        };
        if cfg_path.is_file() {
            crate::pipeline::load_config(&cfg_path).ok()
        } else {
            None
        }
    };

    // Filter set of crates to consider.
    let mut targets: Vec<&cargo_edit::MemberInfo> = Vec::new();
    if !opts.package.is_empty() {
        for name in &opts.package {
            let Some(m) = ws.members.iter().find(|m| &m.name == name) else {
                bail!("crate '{}' not found in workspace", name);
            };
            targets.push(m);
        }
    } else if opts.workspace {
        for m in &ws.members {
            if m.publish_false || opts.exclude.iter().any(|e| e == &m.name) {
                continue;
            }
            targets.push(m);
        }
    } else {
        // Single-crate workspace? Default to the only publishable member.
        let pubs: Vec<&cargo_edit::MemberInfo> =
            ws.members.iter().filter(|m| !m.publish_false).collect();
        if pubs.len() == 1 {
            targets.push(pubs[0]);
        } else {
            bail!("multi-crate workspace: specify `-p <name>` (repeatable) or `--workspace`");
        }
    }

    let mut rows: Vec<PlanRow> = Vec::new();
    for m in targets {
        let current_ver_str = resolve_member_version(m, &ws)?;
        let current = Version::parse(&current_ver_str).with_context(|| {
            format!(
                "crate '{}' has un-parseable version '{}'",
                m.name, current_ver_str
            )
        })?;

        let (level, next, reason) = match &positional {
            Positional::Explicit(v) => {
                let r = format!("explicit version {}", v);
                (
                    BumpLevel::Explicit,
                    with_pre(v.clone(), opts.pre.as_deref()),
                    r,
                )
            }
            Positional::Release => {
                let mut v = current.clone();
                v.pre = semver::Prerelease::EMPTY;
                let r = "strip prerelease".to_string();
                (BumpLevel::Release, v, r)
            }
            Positional::Level(lv) => {
                let next = apply_level(&current, *lv, opts.pre.as_deref());
                let r = format!("explicit {}", lv.label());
                (*lv, next, r)
            }
            Positional::Infer => {
                let tag_prefix = anodize_cfg
                    .as_ref()
                    .and_then(|cfg| find_crate_in_config(cfg, &m.name))
                    .and_then(|c| anodize_core::git::extract_tag_prefix(&c.tag_template));
                let inferred =
                    inference::infer_for_crate(workspace_root, m, tag_prefix.as_deref())?;
                match inferred.level {
                    BumpLevel::Skip => (BumpLevel::Skip, current.clone(), inferred.reason),
                    other => (
                        other,
                        apply_level(&current, other, opts.pre.as_deref()),
                        inferred.reason,
                    ),
                }
            }
        };

        let next_str = if level == BumpLevel::Skip {
            "—".to_string()
        } else {
            next.to_string()
        };

        rows.push(PlanRow {
            crate_name: m.name.clone(),
            current: current.to_string(),
            next: next_str,
            level,
            reason,
            edited_files: Vec::new(),
            manifest: m.manifest_path.clone(),
            inherits_workspace_version: m.inherits_workspace_version,
        });
    }

    // Populate edited_files with the manifest of each non-skip crate.
    // Propagation + workspace-inheritance handling is layered in later phases;
    // this keeps Phase 1 self-contained.
    for row in rows.iter_mut() {
        if row.level == BumpLevel::Skip {
            continue;
        }
        if row.inherits_workspace_version {
            // Root Cargo.toml is edited via [workspace.package].
            row.edited_files.push(workspace_root.join("Cargo.toml"));
        } else {
            row.edited_files.push(row.manifest.clone());
        }
    }

    Ok(rows)
}

/// Find a crate's config across both top-level `crates:` and any
/// `workspaces[*].crates`. cfgd-style monorepos put their crates under
/// `workspaces:` rather than at the root.
fn find_crate_in_config<'a>(
    cfg: &'a anodize_core::config::Config,
    name: &str,
) -> Option<&'a anodize_core::config::CrateConfig> {
    if let Some(c) = cfg.crates.iter().find(|c| c.name == name) {
        return Some(c);
    }
    cfg.workspaces
        .as_ref()?
        .iter()
        .flat_map(|w| w.crates.iter())
        .find(|c| c.name == name)
}

fn resolve_member_version(
    m: &cargo_edit::MemberInfo,
    ws: &cargo_edit::WorkspaceInfo,
) -> Result<String> {
    if m.inherits_workspace_version {
        ws.workspace_package_version.clone().context(
            "crate inherits version.workspace = true but root [workspace.package].version is unset",
        )
    } else {
        m.own_version.clone().with_context(|| {
            format!(
                "crate '{}' has no [package].version and does not inherit from workspace",
                m.name
            )
        })
    }
}

fn apply_level(cur: &Version, level: BumpLevel, pre: Option<&str>) -> Version {
    let mut next = cur.clone();
    next.build = semver::BuildMetadata::EMPTY;
    next.pre = semver::Prerelease::EMPTY;
    match level {
        BumpLevel::Major => {
            next.major += 1;
            next.minor = 0;
            next.patch = 0;
        }
        BumpLevel::Minor => {
            next.minor += 1;
            next.patch = 0;
        }
        BumpLevel::Patch => {
            next.patch += 1;
        }
        BumpLevel::Explicit | BumpLevel::Release | BumpLevel::Skip => {}
    }
    with_pre(next, pre)
}

fn with_pre(mut v: Version, pre: Option<&str>) -> Version {
    if let Some(ident) = pre {
        v.pre = semver::Prerelease::new(ident).unwrap_or(semver::Prerelease::EMPTY);
    }
    v
}

/// Render the plan as a plain-text table on stdout.
pub fn render_text_table(rows: &[PlanRow]) {
    // Column widths, minimums match the plan spec.
    let mut w_name = "Crate".len();
    let mut w_cur = "Current".len();
    let mut w_next = "Next".len();
    let mut w_level = "Level".len();
    for r in rows {
        w_name = w_name.max(r.crate_name.len());
        w_cur = w_cur.max(r.current.len());
        w_next = w_next.max(r.next.len());
        w_level = w_level.max(r.level.label().len());
    }
    println!(
        "{:<w_name$}  {:<w_cur$}  →  {:<w_next$}  {:<w_level$}  Reason",
        "Crate",
        "Current",
        "Next",
        "Level",
        w_name = w_name,
        w_cur = w_cur,
        w_next = w_next,
        w_level = w_level,
    );
    for r in rows {
        println!(
            "{:<w_name$}  {:<w_cur$}  →  {:<w_next$}  {:<w_level$}  {}",
            r.crate_name,
            r.current,
            r.next,
            r.level.label(),
            r.reason,
            w_name = w_name,
            w_cur = w_cur,
            w_next = w_next,
            w_level = w_level,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_positional_levels() {
        assert!(matches!(
            parse_positional(&None).unwrap(),
            Positional::Infer
        ));
        assert!(matches!(
            parse_positional(&Some("patch".to_string())).unwrap(),
            Positional::Level(BumpLevel::Patch)
        ));
        assert!(matches!(
            parse_positional(&Some("minor".to_string())).unwrap(),
            Positional::Level(BumpLevel::Minor)
        ));
        assert!(matches!(
            parse_positional(&Some("major".to_string())).unwrap(),
            Positional::Level(BumpLevel::Major)
        ));
        assert!(matches!(
            parse_positional(&Some("release".to_string())).unwrap(),
            Positional::Release
        ));
        match parse_positional(&Some("1.2.3".to_string())).unwrap() {
            Positional::Explicit(v) => {
                assert_eq!(v.major, 1);
                assert_eq!(v.minor, 2);
                assert_eq!(v.patch, 3);
            }
            _ => panic!("expected Explicit"),
        }
    }

    #[test]
    fn apply_level_semver_math() {
        let v = Version::parse("1.2.3").unwrap();
        assert_eq!(apply_level(&v, BumpLevel::Patch, None).to_string(), "1.2.4");
        assert_eq!(apply_level(&v, BumpLevel::Minor, None).to_string(), "1.3.0");
        assert_eq!(apply_level(&v, BumpLevel::Major, None).to_string(), "2.0.0");
    }

    #[test]
    fn apply_level_clears_prerelease_on_bump() {
        let v = Version::parse("1.2.3-rc.1").unwrap();
        assert_eq!(apply_level(&v, BumpLevel::Patch, None).to_string(), "1.2.4");
    }

    #[test]
    fn apply_level_with_pre_appends() {
        let v = Version::parse("1.2.3").unwrap();
        assert_eq!(
            apply_level(&v, BumpLevel::Minor, Some("rc.1")).to_string(),
            "1.3.0-rc.1"
        );
    }
}
