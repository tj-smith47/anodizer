use anodize_core::context::Context;
use anodize_core::stage::Stage;
use anyhow::Result;

pub struct ChecksumStage;

impl Stage for ChecksumStage {
    fn name(&self) -> &str { "checksum" }
    fn run(&self, _ctx: &mut Context) -> Result<()> { todo!() }
}
