pub mod ci;
pub mod interactive;

use crate::tui::app::App;
use crate::tui::session::{HolderNames, SessionSlot};

/// How the run's view names a lock holder's pid: the app's own names when it
/// has them (a viewer, from the sessions it watches), else the process's
/// session slot's.
///
/// Call it only **after** [`SessionSlot::open`]: the slot names every live
/// session but its own, and before `open` it has no own session to leave
/// out, so the process would name itself as a holder.
fn lock_holders(app: &impl App, slot: Option<&SessionSlot>) -> HolderNames {
    app.lock_holders()
        .or_else(|| slot.map(SessionSlot::holder_names))
        .unwrap_or_default()
}
