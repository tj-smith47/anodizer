use anodizer_core::context::Context;
use anyhow::Result;

/// Log and optionally execute a provider send action, respecting dry-run mode.
///
/// `key_width` is the shared pad width across every provider firing in
/// this Announcing section (computed by the dispatch loop), so multiple
/// kv rows column-align like the summary table.
pub(crate) fn dispatch(
    ctx: &Context,
    provider: &str,
    log_line: &str,
    key_width: usize,
    send: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let log = ctx.logger("announce");
    // kv register: the provider name is the key (several providers share
    // one Announcing section, so the name is genuine information), the
    // announcement line is the value.
    if ctx.is_dry_run() {
        log.kv(provider, &format!("(dry-run) {log_line}"), key_width);
    } else {
        log.kv(provider, log_line, key_width);
        send()?;
    }
    Ok(())
}
