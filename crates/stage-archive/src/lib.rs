use anodize_core::context::Context;
use anodize_core::stage::Stage;
use anyhow::Result;

pub struct ArchiveStage;

impl Stage for ArchiveStage {
    fn name(&self) -> &str { "archive" }
    fn run(&self, _ctx: &mut Context) -> Result<()> { todo!() }
}
