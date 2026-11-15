use std::any::Any;
use std::io;
use std::sync::Mutex;
use std::thread::ThreadId;

use crossterm::execute;
use crossterm::terminal::disable_raw_mode;

use super::log_sink::LogSink;

/// The last panic the hook saw: what the default hook printed, kept so the
/// interactive backend can print it again once it has left the alternate
/// screen. The hook never touches the alternate screen (it also fires for
/// panics that are caught, under a screen that stays up), so a panic that
/// ends the run is first printed onto the dashboard's screen and lost when
/// the restore guard leaves it.
///
/// One slot, overwritten by each panic: the backend matches what it takes
/// against its own panic (the thread it unwinds on, or the payload it
/// resumes), so a caught panic elsewhere is never printed in its place.
static LAST_PANIC: Mutex<Option<PanicReport>> = Mutex::new(None);

#[derive(Debug)]
struct PanicReport {
    thread: ThreadId,
    /// The payload's text, when it has one: what a resumed payload matches.
    message: Option<String>,
    /// The default hook's first lines: thread, location and message.
    text: String,
}

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
        record(info.payload(), info.location().map(ToString::to_string));
        // A tool's PTY session is not restored here. This hook also fires for a
        // panic tokio catches, which would leave the terminal cooked under a live
        // tool. An uncaught panic unwinds into `RawTerminal`'s `Drop` instead, and
        // a panic that aborts leaves the terminal raw until `reset`.
        drop(disable_raw_mode());
        drop(execute!(io::stderr(), crossterm::cursor::Show));
        sink.switch_to_direct();
        previous(info);
    }));
}

/// The text a panic payload carries: `panic!("…")` with or without
/// arguments, `expect`, an index out of bounds. `None` for any other type.
pub(crate) fn payload_message(payload: &(dyn Any + Send)) -> Option<&str> {
    payload
        .downcast_ref::<&'static str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
}

/// Keep the current thread's panic, in the default hook's words.
fn record(payload: &(dyn Any + Send), location: Option<String>) {
    let thread = std::thread::current();
    let message = payload_message(payload).map(str::to_owned);
    let text = format!(
        "thread '{}' panicked at {}:\n{}",
        thread.name().unwrap_or("<unnamed>"),
        location.as_deref().unwrap_or("<unknown>"),
        message.as_deref().unwrap_or("Box<dyn Any>"),
    );
    let report = PanicReport {
        thread: thread.id(),
        message,
        text,
    };
    // A poisoned slot still holds a usable `Option`: the hook must not panic.
    *LAST_PANIC
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(report);
}

/// Take the recorded panic if `matches` says it is the one being reported.
fn take_if(matches: impl FnOnce(&PanicReport) -> bool) -> Option<String> {
    let mut slot = LAST_PANIC
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if slot.as_ref().is_some_and(matches) {
        slot.take().map(|r| r.text)
    } else {
        None
    }
}

/// The text of the panic unwinding the current thread, as the hook printed it.
pub(crate) fn take_for_current_thread() -> Option<String> {
    let me = std::thread::current().id();
    take_if(|r| r.thread == me)
}

/// The text of the panic whose payload is being resumed on another thread
/// (an app task's, through its `JoinError`). Matched on the payload's text;
/// without a hook record, the payload's own text, so the message still shows.
pub(crate) fn take_for_payload(payload: &(dyn Any + Send)) -> Option<String> {
    let message = payload_message(payload);
    take_if(|r| message.is_some() && r.message.as_deref() == message)
        .or_else(|| message.map(|m| format!("heph panicked:\n{m}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    // One test, so that nothing else in this binary writes the slot between
    // the steps (the slot is process-wide).
    #[test]
    fn a_report_goes_only_to_the_panic_it_belongs_to() {
        let boom: Box<dyn Any + Send> = Box::new(String::from("boom 42"));
        let other: Box<dyn Any + Send> = Box::new("other");
        let opaque: Box<dyn Any + Send> = Box::new(7_u8);
        assert_eq!(payload_message(&*boom), Some("boom 42"));
        assert_eq!(payload_message(&*other), Some("other"));
        assert_eq!(payload_message(&*opaque), None);

        // Recorded on another thread, as an app task's panic is.
        std::thread::Builder::new()
            .name("worker-7".into())
            .spawn(|| record(&String::from("boom 42"), Some("src/a.rs:1:2".into())))
            .expect("spawn")
            .join()
            .expect("join");
        assert_eq!(
            take_for_current_thread(),
            None,
            "another thread's panic is not this one's"
        );
        assert_eq!(
            take_for_payload(&*other).as_deref(),
            Some("heph panicked:\nother"),
            "a payload that does not match prints its own text"
        );
        assert_eq!(
            take_for_payload(&*boom).as_deref(),
            Some("thread 'worker-7' panicked at src/a.rs:1:2:\nboom 42"),
        );
        assert_eq!(
            take_for_payload(&*boom).as_deref(),
            Some("heph panicked:\nboom 42"),
            "taken once"
        );
        assert_eq!(take_for_payload(&*opaque), None);

        record(&"here", None);
        let text = take_for_current_thread().expect("this thread's report");
        assert!(text.ends_with("panicked at <unknown>:\nhere"), "{text}");
    }
}
