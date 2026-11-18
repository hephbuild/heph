use std::any::Any;
use std::sync::Mutex;
use std::thread::ThreadId;

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

/// Install a panic hook that records the panic (see [`LAST_PANIC`]) and
/// invokes the previous hook.
///
/// It touches neither the terminal nor the sink (I12): it also fires for a
/// panic that is caught, under a TUI that stays up and must keep its raw mode,
/// its hidden cursor and its buffered sink. A panic that unwinds the
/// interactive backend is restored by the backend's guards on the way out;
/// one that aborts goes through `hcore::shutdown`'s restore closure.
pub fn install() {
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
        with_newlines(|| previous(info));
    }));
}

/// Run `f` with stderr's output processing on, when raw mode turned it off,
/// so the default hook's `\n` starts a new line instead of a staircase. The
/// output flags only: the keyboard stays raw (a caught panic's TUI keeps
/// reading keys as they are pressed), and they are put back as they were.
fn with_newlines(f: impl FnOnce()) {
    with_newlines_on(libc::STDERR_FILENO, f);
}

/// [`with_newlines`] on `fd`. The output flags are put back only while the
/// keyboard is still raw: a backend that tore down meanwhile (another task's
/// caught panic, printing while the TUI exits normally) left cooked mode,
/// and raw output flags over it would staircase the user's shell.
fn with_newlines_on(fd: libc::c_int, f: impl FnOnce()) {
    let oflag = |t: &mut libc::termios| {
        // SAFETY: `t` points at valid, correctly-sized storage; a non-tty fd
        // fails with ENOTTY and writes nothing that is read.
        (unsafe { libc::tcgetattr(fd, t) } == 0).then_some(t.c_oflag)
    };
    // SAFETY: plain old data; read only after `tcgetattr` filled it.
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    let saved = oflag(&mut t).filter(|o| o & libc::OPOST == 0);
    if saved.is_some() {
        t.c_oflag |= libc::OPOST | libc::ONLCR;
        // SAFETY: `t` is the terminal's own settings, read just above.
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &t) };
    }
    f();
    // Put back only the output flags, onto whatever the terminal holds now,
    // and only if that is still raw.
    if let Some(saved) = saved
        && oflag(&mut t).is_some()
        && t.c_lflag & libc::ICANON == 0
    {
        t.c_oflag = saved;
        // SAFETY: as above.
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &t) };
    }
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
/// (an app task's, through its `JoinError`). Matched on the payload's text,
/// and a payload with none (`panic_any(5)`) on a record with none; without a
/// matching record, the payload's own text, or a line saying it had none, so
/// something always shows once the full screen it was printed on is gone.
pub(crate) fn take_for_payload(payload: &(dyn Any + Send)) -> String {
    let message = payload_message(payload);
    take_if(|r| r.message.as_deref() == message).unwrap_or_else(|| match message {
        Some(m) => format!("heph panicked:\n{m}"),
        None => NO_TEXT.to_owned(),
    })
}

/// What [`take_for_payload`] says of a payload with no text and no record.
const NO_TEXT: &str = "heph panicked (the panic carried no message)";

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
            take_for_payload(&*other),
            "heph panicked:\nother",
            "a payload that does not match prints its own text"
        );
        assert_eq!(
            take_for_payload(&*boom),
            "thread 'worker-7' panicked at src/a.rs:1:2:\nboom 42",
        );
        assert_eq!(
            take_for_payload(&*boom),
            "heph panicked:\nboom 42",
            "taken once"
        );
        assert_eq!(
            take_for_payload(&*opaque),
            NO_TEXT,
            "no text and no record still says something"
        );
        std::thread::Builder::new()
            .name("worker-8".into())
            .spawn(|| record(&7_u8, Some("src/b.rs:3:4".into())))
            .expect("spawn")
            .join()
            .expect("join");
        assert_eq!(
            take_for_payload(&*opaque),
            "thread 'worker-8' panicked at src/b.rs:3:4:\nBox<dyn Any>",
            "a payload with no text takes the record with none"
        );

        record(&"here", None);
        let text = take_for_current_thread().expect("this thread's report");
        assert!(text.ends_with("panicked at <unknown>:\nhere"), "{text}");
    }

    /// A pty's follower in raw mode, as the interactive backend leaves stderr.
    fn raw_pty() -> (std::fs::File, std::fs::File) {
        use std::os::fd::FromRawFd as _;
        let (mut leader, mut follower) = (0, 0);
        // SAFETY: two valid out-pointers; null name, termios and winsize.
        let rc = unsafe {
            libc::openpty(
                &mut leader,
                &mut follower,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, 0, "openpty");
        // SAFETY: `openpty` returned this fd; this test now owns it.
        let leader = unsafe { std::fs::File::from_raw_fd(leader) };
        // SAFETY: as above.
        let follower = unsafe { std::fs::File::from_raw_fd(follower) };
        let mut t = termios(&follower);
        // SAFETY: `t` was filled by `tcgetattr`.
        unsafe { libc::cfmakeraw(&mut t) };
        set(&follower, &t);
        (leader, follower)
    }

    fn termios(f: &std::fs::File) -> libc::termios {
        use std::os::fd::AsRawFd as _;
        // SAFETY: plain old data, filled by `tcgetattr` before it is read.
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: a valid fd and valid storage.
        assert_eq!(unsafe { libc::tcgetattr(f.as_raw_fd(), &mut t) }, 0);
        t
    }

    fn set(f: &std::fs::File, t: &libc::termios) {
        use std::os::fd::AsRawFd as _;
        // SAFETY: a valid fd and settings read from it.
        let rc = unsafe { libc::tcsetattr(f.as_raw_fd(), libc::TCSANOW, t) };
        assert_eq!(rc, 0, "tcsetattr");
    }

    /// The hook prints with output processing on and puts raw output back
    /// while the terminal is still raw; once the backend restored cooked
    /// mode meanwhile, it leaves the cooked flags alone.
    #[test]
    fn newlines_restore_raw_output_only_onto_a_raw_terminal() {
        use std::os::fd::AsRawFd as _;
        let (_leader, follower) = raw_pty();
        let fd = follower.as_raw_fd();

        with_newlines_on(fd, || {
            assert_ne!(termios(&follower).c_oflag & libc::OPOST, 0, "printing");
        });
        assert_eq!(
            termios(&follower).c_oflag & libc::OPOST,
            0,
            "still raw: raw output put back"
        );

        with_newlines_on(fd, || {
            // The backend tears down while the hook prints.
            let mut cooked = termios(&follower);
            cooked.c_lflag |= libc::ICANON | libc::ECHO;
            cooked.c_oflag |= libc::OPOST | libc::ONLCR;
            set(&follower, &cooked);
        });
        let after = termios(&follower);
        assert_ne!(after.c_oflag & libc::OPOST, 0, "cooked output kept");
        assert_ne!(after.c_lflag & libc::ICANON, 0);
    }
}
