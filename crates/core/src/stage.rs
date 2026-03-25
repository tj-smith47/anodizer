use crate::context::Context;
use anyhow::Result;

pub trait Stage {
    fn name(&self) -> &str;
    fn run(&self, ctx: &mut Context) -> Result<()>;
}
