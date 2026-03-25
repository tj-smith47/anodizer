use anodize_core::context::Context;
use anodize_core::stage::Stage;
use anyhow::Result;

pub struct DockerStage;

impl Stage for DockerStage {
    fn name(&self) -> &str { "docker" }
    fn run(&self, _ctx: &mut Context) -> Result<()> { todo!() }
}
