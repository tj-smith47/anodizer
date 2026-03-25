use anodize_core::context::Context;
use anodize_core::stage::Stage;
use anyhow::Result;

pub struct NfpmStage;

impl Stage for NfpmStage {
    fn name(&self) -> &str { "nfpm" }
    fn run(&self, _ctx: &mut Context) -> Result<()> { todo!() }
}
