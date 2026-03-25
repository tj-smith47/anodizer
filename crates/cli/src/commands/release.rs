use anyhow::Result;
use anodize_core::context::{Context, ContextOptions};
use crate::pipeline;

pub struct ReleaseOpts {
    pub crate_names: Vec<String>,
    pub all: bool,
    pub force: bool,
    pub snapshot: bool,
    pub dry_run: bool,
    pub clean: bool,
    pub skip: Vec<String>,
    pub token: Option<String>,
    pub verbose: bool,
    pub debug: bool,
}

pub fn run(opts: ReleaseOpts) -> Result<()> {
    let config = pipeline::load_config(&pipeline::find_config()?)?;

    if opts.clean {
        let dist = &config.dist;
        if dist.exists() {
            std::fs::remove_dir_all(dist)?;
        }
    }

    let ctx_opts = ContextOptions {
        snapshot: opts.snapshot,
        dry_run: opts.dry_run,
        verbose: opts.verbose,
        debug: opts.debug,
        skip_stages: opts.skip,
        selected_crates: opts.crate_names,
        token: opts.token,
    };
    let mut ctx = Context::new(config, ctx_opts);
    let p = pipeline::build_release_pipeline();
    p.run(&mut ctx)
}
