//! Terminal UI: progress rendering (interactive ratatui + CI line modes), log
//! sink, and the paused-prompt machinery. Depends only on heph-core (events,
//! shutdown trigger); the bin drives it. Isolates the ratatui/crossterm surface.
#![cfg_attr(
    test,
    expect(
        clippy::assertions_on_result_states,
        clippy::err_expect,
        unused_imports,
        reason = "restriction/style lints scoped to production code; tests are exempt"
    )
)]

pub mod tui;
