use anodizer_core::context::Context;
use anyhow::Result;

/// Log and optionally execute a provider send action, respecting dry-run mode.
pub(crate) fn dispatch(
    ctx: &Context,
    provider: &str,
    log_line: &str,
    send: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let log = ctx.logger("announce");
    // kv register: the provider name is the key (several providers share
    // one Announcing section, so the name is genuine information), the
    // announcement line is the value.
    let key_width = provider.chars().count();
    if ctx.is_dry_run() {
        log.kv(provider, &format!("(dry-run) {log_line}"), key_width);
    } else {
        log.kv(provider, log_line, key_width);
        send()?;
    }
    Ok(())
}
