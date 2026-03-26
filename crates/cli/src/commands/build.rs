use anyhow::Result;
use std::path::PathBuf;

#[allow(dead_code)] // Fields consumed when build command is fully implemented
pub struct BuildOpts {
    pub crate_names: Vec<String>,
    pub config_override: Option<PathBuf>,
    pub parallelism: usize,
    pub single_target: Option<String>,
}

pub fn run(opts: BuildOpts) -> Result<()> {
    let _ = &opts;
    eprintln!("build command not yet implemented");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_opts_defaults() {
        let opts = BuildOpts {
            crate_names: vec![],
            config_override: None,
            parallelism: 4,
            single_target: None,
        };
        assert_eq!(opts.parallelism, 4);
        assert!(opts.single_target.is_none());
    }

    #[test]
    fn test_build_opts_with_single_target() {
        let opts = BuildOpts {
            crate_names: vec!["myapp".to_string()],
            config_override: None,
            parallelism: 2,
            single_target: Some("x86_64-unknown-linux-gnu".to_string()),
        };
        assert_eq!(
            opts.single_target.as_deref(),
            Some("x86_64-unknown-linux-gnu")
        );
    }

    #[test]
    fn test_build_run_succeeds() {
        let opts = BuildOpts {
            crate_names: vec![],
            config_override: None,
            parallelism: 1,
            single_target: None,
        };
        let result = run(opts);
        assert!(result.is_ok());
    }
}
