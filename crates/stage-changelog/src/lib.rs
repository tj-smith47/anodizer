use anodize_core::context::Context;
use anodize_core::stage::Stage;
use anyhow::Result;

pub struct ChangelogStage;

impl Stage for ChangelogStage {
    fn name(&self) -> &str { "changelog" }
    fn run(&self, _ctx: &mut Context) -> Result<()> { todo!() }
}
