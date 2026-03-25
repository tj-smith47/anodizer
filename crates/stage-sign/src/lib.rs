use anodize_core::context::Context;
use anodize_core::stage::Stage;
use anyhow::Result;

pub struct SignStage;

impl Stage for SignStage {
    fn name(&self) -> &str { "sign" }
    fn run(&self, _ctx: &mut Context) -> Result<()> { todo!() }
}
