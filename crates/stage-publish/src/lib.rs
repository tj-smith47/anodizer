use anodize_core::context::Context;
use anodize_core::stage::Stage;
use anyhow::Result;

pub struct PublishStage;

impl Stage for PublishStage {
    fn name(&self) -> &str { "publish" }
    fn run(&self, _ctx: &mut Context) -> Result<()> { todo!() }
}
