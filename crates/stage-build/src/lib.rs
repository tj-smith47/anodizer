use anodize_core::context::Context;
use anodize_core::stage::Stage;
use anyhow::Result;

pub struct BuildStage;

impl Stage for BuildStage {
    fn name(&self) -> &str { "build" }
    fn run(&self, _ctx: &mut Context) -> Result<()> { todo!() }
}
