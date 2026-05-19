use std::io;
use std::sync::Mutex;

use crossterm::execute;
use crossterm::terminal::disable_raw_mode;

use super::log_sink::LogSink;

/// Install a panic hook that restores the terminal and switches the log sink
/// back to direct stderr passthrough before invoking the previous hook, so
/// the panic message always reaches the user even if a TUI was active.
pub fn install(sink: LogSink) {
    static INSTALLED: Mutex<bool> = Mutex::new(false);
    {
        let mut guard = INSTALLED.lock().expect("panic hook mutex");
        if *guard {
            return;
        }
        *guard = true;
    }

    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        drop(disable_raw_mode());
        drop(execute!(io::stderr(), crossterm::cursor::Show));
        sink.switch_to_direct();
        previous(info);
    }));
}
