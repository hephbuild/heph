//! Small helpers shared by both sides of the spawn-at-the-seam wrappers
//! (host adapters here; guest wrappers in `plugin-sdk::serve`). Always
//! compiled — no transport or host deps — so the guest SDK can use them with
//! `default-features = false`.

/// Best-effort text of a panic payload (`panic!` `&str`/`String`, else a stub).
///
/// Used when mapping a seam task's `JoinError::is_panic` payload into an error
/// body — the wrapper futures must never `resume_unwind` (their polls run
/// inside `extern "C"` shims, where an unwind aborts the process).
pub fn panic_text(p: &(dyn std::any::Any + Send)) -> &str {
    if let Some(s) = p.downcast_ref::<&'static str>() {
        s
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.as_str()
    } else {
        "<non-string panic payload>"
    }
}
