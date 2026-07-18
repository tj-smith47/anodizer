use super::*;

// ---------------------------------------------------------------------------
// Per-crate publish visitor
// ---------------------------------------------------------------------------

/// Identifies which of the three publish-config axes a visited block came from.
///
/// The config-validation walkers each format their own location string from
/// this identity, so different walkers can keep their distinct location wording
/// (`crate '{name}'` vs `crates[{name}].publish.homebrew_cask`) while sharing a
/// single iteration order: crates, then workspaces, then defaults.
pub(crate) enum PublishAxis<'a> {
    /// A top-level `crates[].publish` block, carrying the crate name.
    Crate { name: &'a str },
    /// A `workspaces[].crates[].publish` block, carrying the workspace and
    /// crate names.
    Workspace {
        workspace: &'a str,
        crate_name: &'a str,
    },
    /// The `defaults.publish` block.
    Defaults,
}

impl PublishAxis<'_> {
    /// Location string in the bare publish-block wording shared by the
    /// submitter-required and legacy-Homebrew-Formula warnings:
    /// `crate '{name}'`, `workspaces[{ws}].crates[{krate}]`, or
    /// `defaults.publish`.
    pub(crate) fn location(&self) -> String {
        match self {
            PublishAxis::Crate { name } => format!("crate '{name}'"),
            PublishAxis::Workspace {
                workspace,
                crate_name,
            } => format!("workspaces[{workspace}].crates[{crate_name}]"),
            PublishAxis::Defaults => "defaults.publish".to_string(),
        }
    }

    /// Location string in the cask-block wording used by the legacy
    /// Homebrew-Cask singular fold: `crates[{name}].publish.homebrew_cask`,
    /// `workspaces[{ws}].crates[{krate}].publish.homebrew_cask`, or
    /// `defaults.publish.homebrew_cask`.
    pub(crate) fn homebrew_cask_location(&self) -> String {
        match self {
            PublishAxis::Crate { name } => {
                format!("crates[{name}].publish.homebrew_cask")
            }
            PublishAxis::Workspace {
                workspace,
                crate_name,
            } => format!("workspaces[{workspace}].crates[{crate_name}].publish.homebrew_cask"),
            PublishAxis::Defaults => "defaults.publish.homebrew_cask".to_string(),
        }
    }

    /// Location string in the winget-block wording:
    /// `crates[{name}].publish.winget`,
    /// `workspaces[{ws}].crates[{krate}].publish.winget`, or
    /// `defaults.publish.winget`.
    pub(crate) fn winget_location(&self) -> String {
        match self {
            PublishAxis::Crate { name } => format!("crates[{name}].publish.winget"),
            PublishAxis::Workspace {
                workspace,
                crate_name,
            } => format!("workspaces[{workspace}].crates[{crate_name}].publish.winget"),
            PublishAxis::Defaults => "defaults.publish.winget".to_string(),
        }
    }
}

/// Shared, immutable view over the publisher sub-configs that appear on both
/// [`PublishConfig`] (the `crates[].publish` axis) and [`PublishDefaults`] (the
/// `defaults.publish` axis). The two underlying structs are distinct types, so
/// this enum erases the difference for read-only walkers.
pub(crate) enum PublishRef<'a> {
    /// A per-crate `publish:` block.
    Crate(&'a PublishConfig),
    /// The `defaults.publish:` block.
    Defaults(&'a PublishDefaults),
}

impl PublishRef<'_> {
    pub(crate) fn homebrew(&self) -> Option<&HomebrewConfig> {
        match self {
            PublishRef::Crate(p) => p.homebrew.as_ref(),
            PublishRef::Defaults(p) => p.homebrew.as_ref(),
        }
    }

    pub(crate) fn chocolatey(&self) -> Option<&ChocolateyConfig> {
        match self {
            PublishRef::Crate(p) => p.chocolatey.as_ref(),
            PublishRef::Defaults(p) => p.chocolatey.as_ref(),
        }
    }

    pub(crate) fn winget(&self) -> Option<&WingetConfig> {
        match self {
            PublishRef::Crate(p) => p.winget.as_ref(),
            PublishRef::Defaults(p) => p.winget.as_ref(),
        }
    }

    pub(crate) fn aur_source(&self) -> Option<&AurSourceConfig> {
        match self {
            PublishRef::Crate(p) => p.aur_source.as_ref(),
            PublishRef::Defaults(p) => p.aur_source.as_ref(),
        }
    }

    pub(crate) fn homebrew_cask(&self) -> Option<&HomebrewCaskConfig> {
        match self {
            PublishRef::Crate(p) => p.homebrew_cask.as_ref(),
            PublishRef::Defaults(p) => p.homebrew_cask.as_ref(),
        }
    }
}

/// Shared, mutable view over the publisher sub-configs that appear on both
/// [`PublishConfig`] and [`PublishDefaults`]. The `_mut` companion to
/// [`PublishRef`], for walkers that fold or rewrite a publisher block in place.
pub(crate) enum PublishMut<'a> {
    /// A per-crate `publish:` block.
    Crate(&'a mut PublishConfig),
    /// The `defaults.publish:` block.
    Defaults(&'a mut PublishDefaults),
}

impl PublishMut<'_> {
    pub(crate) fn homebrew_cask_mut(&mut self) -> Option<&mut HomebrewCaskConfig> {
        match self {
            PublishMut::Crate(p) => p.homebrew_cask.as_mut(),
            PublishMut::Defaults(p) => p.homebrew_cask.as_mut(),
        }
    }
}

/// Visit every `publish:` block across all three config axes — `crates[]`,
/// `workspaces[].crates[]`, then `defaults` — in that fixed order, passing each
/// block's [`PublishAxis`] identity and a read-only [`PublishRef`] view to
/// `visit`. Axes with no `publish:` block are skipped.
pub(crate) fn for_each_crate_publish<F>(config: &Config, mut visit: F)
where
    F: FnMut(PublishAxis<'_>, PublishRef<'_>),
{
    for krate in &config.crates {
        if let Some(ref publish) = krate.publish {
            visit(
                PublishAxis::Crate { name: &krate.name },
                PublishRef::Crate(publish),
            );
        }
    }

    if let Some(ref workspaces) = config.workspaces {
        for ws in workspaces {
            for krate in &ws.crates {
                if let Some(ref publish) = krate.publish {
                    visit(
                        PublishAxis::Workspace {
                            workspace: &ws.name,
                            crate_name: &krate.name,
                        },
                        PublishRef::Crate(publish),
                    );
                }
            }
        }
    }

    if let Some(ref defaults) = config.defaults
        && let Some(ref publish) = defaults.publish
    {
        visit(PublishAxis::Defaults, PublishRef::Defaults(publish));
    }
}

/// Fallible companion to [`for_each_crate_publish`]: visits the same three axes
/// in the same fixed order, but short-circuits on the first `Err` the callback
/// returns, propagating it to the caller. For validators that early-exit on the
/// first offending block.
pub(crate) fn try_for_each_crate_publish<F, E>(config: &Config, mut visit: F) -> Result<(), E>
where
    F: FnMut(PublishAxis<'_>, PublishRef<'_>) -> Result<(), E>,
{
    for krate in &config.crates {
        if let Some(ref publish) = krate.publish {
            visit(
                PublishAxis::Crate { name: &krate.name },
                PublishRef::Crate(publish),
            )?;
        }
    }

    if let Some(ref workspaces) = config.workspaces {
        for ws in workspaces {
            for krate in &ws.crates {
                if let Some(ref publish) = krate.publish {
                    visit(
                        PublishAxis::Workspace {
                            workspace: &ws.name,
                            crate_name: &krate.name,
                        },
                        PublishRef::Crate(publish),
                    )?;
                }
            }
        }
    }

    if let Some(ref defaults) = config.defaults
        && let Some(ref publish) = defaults.publish
    {
        visit(PublishAxis::Defaults, PublishRef::Defaults(publish))?;
    }

    Ok(())
}

/// Mutable companion to [`for_each_crate_publish`]: visits the same three axes
/// in the same fixed order, passing a [`PublishMut`] view so the callback can
/// rewrite the publisher block in place.
pub(crate) fn for_each_crate_publish_mut<F>(config: &mut Config, mut visit: F)
where
    F: FnMut(PublishAxis<'_>, PublishMut<'_>),
{
    for krate in &mut config.crates {
        if let Some(ref mut publish) = krate.publish {
            visit(
                PublishAxis::Crate { name: &krate.name },
                PublishMut::Crate(publish),
            );
        }
    }

    if let Some(ref mut workspaces) = config.workspaces {
        for ws in workspaces {
            for krate in &mut ws.crates {
                if let Some(ref mut publish) = krate.publish {
                    visit(
                        PublishAxis::Workspace {
                            workspace: &ws.name,
                            crate_name: &krate.name,
                        },
                        PublishMut::Crate(publish),
                    );
                }
            }
        }
    }

    if let Some(ref mut defaults) = config.defaults
        && let Some(ref mut publish) = defaults.publish
    {
        visit(PublishAxis::Defaults, PublishMut::Defaults(publish));
    }
}
