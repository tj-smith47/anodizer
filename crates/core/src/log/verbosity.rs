//! Verbosity level derived from CLI flags.

/// Verbosity level, derived from CLI flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Verbosity {
    Quiet,
    #[default]
    Normal,
    Verbose,
    Debug,
}

impl Verbosity {
    /// Derive verbosity from CLI flag combination.
    /// `--quiet` overrides `--verbose`; `--debug` overrides everything.
    pub fn from_flags(quiet: bool, verbose: bool, debug: bool) -> Self {
        if debug {
            Verbosity::Debug
        } else if quiet {
            Verbosity::Quiet
        } else if verbose {
            Verbosity::Verbose
        } else {
            Verbosity::Normal
        }
    }
}
