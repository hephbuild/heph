//! A panic in plugin code at the seam: who it is named after, and where its
//! text goes.
//!
//! The SDK catches every panic of a call the host makes into the plugin and
//! returns it as that call's error ([`SeamLabel::panicked`]). The message is
//! the error's; std's panic hook would print it a second time, raw, on
//! stderr, before the error was even rendered. So a panic inside a
//! [`guarded`] call goes to `tracing` at `debug`, with its location, and only
//! a panic outside one (a cdylib's create entry, which still aborts) keeps
//! the hook it had.

use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Once};
use std::task::{Context, Poll};

/// What a plugin component is called in a seam diagnostic.
#[derive(Debug, Clone)]
pub(crate) struct SeamLabel {
    /// The plugin's name, from its manifest. `None` for a component wrapped
    /// without one (an in-process test).
    plugin: Option<Arc<str>>,
    /// The component's kind: `driver`, `provider` or `function`.
    kind: &'static str,
    /// The component's name in the engine: `<plugin>.<local>`, `<plugin>`
    /// for one with no local name, or the local name alone with no plugin.
    component: Arc<str>,
}

impl SeamLabel {
    /// A component of `plugin`, `local` being the name its own `config()`
    /// reports (empty for none).
    pub(crate) fn new(plugin: Option<&str>, kind: &'static str, local: &str) -> Self {
        let component: Arc<str> = match (plugin, local) {
            (Some(p), "") => p.into(),
            (Some(p), l) => format!("{p}.{l}").into(),
            (None, "") => "<unnamed>".into(),
            (None, l) => l.into(),
        };
        Self {
            plugin: plugin.map(Into::into),
            kind,
            component,
        }
    }

    /// The plugin's name, when the component was wrapped with one.
    pub(crate) fn plugin(&self) -> Option<&str> {
        self.plugin.as_deref()
    }

    /// The component's name in the engine.
    pub(crate) fn component(&self) -> &str {
        &self.component
    }

    /// The error a panic in `method` turns into, `key` naming what it was
    /// called on (empty for nothing):
    /// "plugin `fault` panicked in driver `fault.drv` during run(//f:l3): boom".
    pub(crate) fn panicked(&self, method: &str, key: &str, msg: &str) -> String {
        self.failed("panicked", method, key, msg)
    }

    /// The error a call the plugin's runtime dropped before it answered
    /// turns into.
    pub(crate) fn aborted(&self, method: &str, key: &str) -> String {
        self.failed("aborted", method, key, "plugin runtime shut down")
    }

    fn failed(&self, what: &str, method: &str, key: &str, msg: &str) -> String {
        let call = if key.is_empty() {
            method.to_owned()
        } else {
            format!("{method}({key})")
        };
        match &self.plugin {
            Some(p) => format!(
                "plugin `{p}` {what} in {} `{}` during {call}: {msg}",
                self.kind, self.component
            ),
            None => format!(
                "{} `{}` {what} during {call}: {msg}",
                self.kind, self.component
            ),
        }
    }
}

thread_local! {
    /// Whether this thread is inside a [`guarded`] call, whose panic is
    /// caught and returned as its error.
    static GUARDED: Cell<bool> = const { Cell::new(false) };
}

/// Restores [`GUARDED`] on drop, an unwind included.
struct Restore(bool);

impl Drop for Restore {
    fn drop(&mut self) {
        GUARDED.with(|g| g.set(self.0));
    }
}

fn enter() -> Restore {
    install_hook();
    Restore(GUARDED.with(|g| g.replace(true)))
}

/// Chain a hook in front of the one std has: a guarded panic is logged at
/// `debug`, any other goes to the hook it replaced. Once per std instance,
/// which for a cdylib is its own.
fn install_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if GUARDED.with(Cell::get) {
                let location = info
                    .location()
                    .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                    .unwrap_or_default();
                tracing::debug!(
                    %location,
                    panic = %hplugin_stabby::seam::panic_text(info.payload()),
                    "plugin code panicked at the seam; the call returns it as its error"
                );
            } else {
                previous(info);
            }
        }));
    });
}

/// `f`, with a panic caught and returned as its text.
pub(crate) fn catch<T>(f: impl FnOnce() -> T) -> Result<T, String> {
    let _restore = enter();
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
        .map_err(|p| hplugin_stabby::seam::panic_text(p.as_ref()).to_owned())
}

/// `fut`, each poll inside a guarded call. The caller still catches the
/// panic: this only routes its hook output.
pub(crate) fn guarded<F: Future>(fut: F) -> Guarded<F> {
    Guarded(Box::pin(fut))
}

pub(crate) struct Guarded<F>(Pin<Box<F>>);

impl<F: Future> Future for Guarded<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let _restore = enter();
        self.0.as_mut().poll(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_driver_is_named_by_its_plugin() {
        let l = SeamLabel::new(Some("fault"), "driver", "drv");
        assert_eq!(
            l.panicked("run", "//f:l3", "boom"),
            "plugin `fault` panicked in driver `fault.drv` during run(//f:l3): boom"
        );
        let l = SeamLabel::new(Some("go"), "provider", "");
        assert_eq!(
            l.panicked("get", "//p:t", "x"),
            "plugin `go` panicked in provider `go` during get(//p:t): x"
        );
        let l = SeamLabel::new(None, "driver", "drv");
        assert_eq!(
            l.panicked("config", "", "x"),
            "driver `drv` panicked during config: x"
        );
    }

    #[test]
    fn catch_returns_the_payload_and_restores_the_flag() {
        let msg =
            catch::<()>(|| std::panic::panic_any(format!("boom {}", 1))).expect_err("it panics");
        assert_eq!(msg, "boom 1");
        assert!(
            !GUARDED.with(Cell::get),
            "the flag is restored after an unwind"
        );
        assert_eq!(catch(|| 3), Ok(3));
    }
}
