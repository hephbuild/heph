/// Pause the TUI for the duration of a block, then resume on drop.
///
/// Accepts anything exposing async `.pause()` returning a guard
/// (`AppContext`, `Pauser`). The guard is held until the block exits,
/// so any prints, stdin reads, or subprocess I/O inside the block see
/// a clean terminal. The block's trailing expression is the macro's
/// value.
///
/// Must be invoked in an `async` context — expands to `.await`.
///
/// ```ignore
/// let n = paused!(ctx, {
///     println!("hello");
///     42
/// });
/// ```
#[macro_export]
macro_rules! tui_paused {
    ($pauser:expr, $body:block) => {{
        let _guard = $pauser.pause().await;
        $body
    }};
}

pub use tui_paused as paused;
