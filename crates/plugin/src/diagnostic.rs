//! Typed diagnostics: the user-facing [`Diagnostic`] (defined in `hcore`, so
//! the event wire can carry it) and the engine's typed boundary frames
//! ([`StageFrame`]), which replace the `get_def: //p:t` / `link` / `execute
//! //p:t` strings the engine used to add as contexts.
//!
//! A `StageFrame` keeps today's text for `{:#}` logs and `RUST_LOG`, but a
//! renderer recognises it by type: it takes the stage and the target from the
//! innermost one ([`innermost_stage_frame`]) and leaves the frames themselves
//! out of the cause it prints ([`cause_frames`]).
//!
//! **Never hashed** (I1). None of these types implements `Hash`, so nothing
//! about a diagnostic or a stage frame can reach a def hash, `hashin` or a
//! cache key. Each refusal below follows a twin that compiles and differs only
//! in the derive, and is pinned to `E0277` (the missing `Hash`), so it cannot
//! pass for another reason.
//!
//! ```
//! #[derive(Clone)]
//! struct Keyed(plugin::diagnostic::Diagnostic, plugin::diagnostic::SourceLoc);
//! #[derive(Clone)]
//! struct Framed(plugin::diagnostic::StageFrame);
//! #[derive(Clone)]
//! struct Recorded(plugin::error::FailureRecord);
//! # fn main() {}
//! ```
//!
//! `failure_record_has_no_hash` (the compacted failure, `hcore::failure`):
//! ```compile_fail,E0277
//! #[derive(Hash)]
//! struct Recorded(plugin::error::FailureRecord);
//! # fn main() {}
//! ```
//!
//! `no_hash_on_diagnostic`:
//! ```compile_fail,E0277
//! #[derive(Hash)]
//! struct Keyed(plugin::diagnostic::Diagnostic);
//! # fn main() {}
//! ```
//!
//! `no_hash_on_source_loc`:
//! ```compile_fail,E0277
//! #[derive(Hash)]
//! struct Keyed(plugin::diagnostic::SourceLoc);
//! # fn main() {}
//! ```
//!
//! `no_hash_on_stage_frame`:
//! ```compile_fail,E0277
//! #[derive(Hash)]
//! struct Framed(plugin::diagnostic::StageFrame);
//! # fn main() {}
//! ```

use crate::error::MultiError;
use hmodel::htaddr::Addr;
use std::fmt;

pub use hcore::diagnostic::{Diagnostic, Hint, HintKind, SourceLoc, Stage, escape_control};

/// The engine boundary a [`StageFrame`] was added at. Each keeps the text the
/// string context it replaces had.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Boundary {
    /// `get spec: //p:t`: looking up a runtime input's spec while collecting
    /// its consumer's transitive deps. The error is the input's own.
    GetSpec,
    /// `get_def: //p:t`: resolving a target's def (its spec, then the driver's
    /// parse).
    GetDef,
    /// `link`: resolving a def's inputs and outputs.
    Link,
    /// `meta`: hashing a def's inputs.
    Meta,
    /// `execute //p:t`: running a target, or reading its cached result.
    Execute,
    /// `run`: the driver's run of a target.
    Run,
}

impl Boundary {
    /// The stage a failure under this boundary belongs to.
    pub fn stage(self) -> Stage {
        match self {
            Self::GetSpec => Stage::Spec,
            Self::GetDef => Stage::Parse,
            Self::Link | Self::Meta => Stage::Link,
            Self::Execute | Self::Run => Stage::Run,
        }
    }
}

/// A typed engine-boundary context: which stage, which target. Added with
/// [`StageContext::stage`], never with a bare `.context(..)`, so each frame is
/// alone of its type on its anyhow level (see `hcore::hmemoizer::nest`) and the
/// walker can tell two of them apart.
#[derive(Debug, Clone)]
pub struct StageFrame {
    pub boundary: Boundary,
    pub addr: Addr,
}

impl StageFrame {
    pub fn stage(&self) -> Stage {
        self.boundary.stage()
    }
}

impl fmt::Display for StageFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.boundary {
            Boundary::GetSpec => write!(f, "get spec: {}", self.addr.format()),
            Boundary::GetDef => write!(f, "get_def: {}", self.addr.format()),
            Boundary::Link => f.write_str("link"),
            Boundary::Meta => f.write_str("meta"),
            Boundary::Execute => write!(f, "execute {}", self.addr.format()),
            Boundary::Run => f.write_str("run"),
        }
    }
}

impl std::error::Error for StageFrame {}

/// `e` under a [`StageFrame`] for `boundary` and `addr`.
pub fn with_stage(e: anyhow::Error, boundary: Boundary, addr: &Addr) -> anyhow::Error {
    hcore::hmemoizer::nest(e).context(StageFrame {
        boundary,
        addr: addr.clone(),
    })
}

/// Adds a [`StageFrame`] to a failed result; nothing on success.
pub trait StageContext<T> {
    fn stage(self, boundary: Boundary, addr: &Addr) -> anyhow::Result<T>;
}

impl<T> StageContext<T> for anyhow::Result<T> {
    fn stage(self, boundary: Boundary, addr: &Addr) -> anyhow::Result<T> {
        self.map_err(|e| with_stage(e, boundary, addr))
    }
}

/// The innermost [`StageFrame`] in `e`: the boundary closest to the cause,
/// whose stage and target a renderer reports.
pub fn innermost_stage_frame(e: &anyhow::Error) -> Option<&StageFrame> {
    hcore::hmemoizer::innermost::<StageFrame>(e)
}

/// `e`'s own [`Diagnostic`]: the first on its chain, through shared errors
/// ([`hcore::hmemoizer::frames`]). One inside a [`MultiError`]'s member is that
/// member's, one failure among several, and is not `e`'s: taking it would
/// print one member as the whole error and drop the rest.
pub fn find_diagnostic(e: &anyhow::Error) -> Option<&Diagnostic> {
    hcore::hmemoizer::frames(e)
        .into_iter()
        .find_map(|f| f.downcast_ref::<Diagnostic>())
}

/// `e`'s cause as text frames, outermost first: every frame but the
/// [`StageFrame`]s, an empty frame (data riding the chain) dropped, and a frame
/// that ends with its source's text cut back to its own part (as
/// `hcore::hmemoizer::chain_frames` does). `e`'s own [`Diagnostic`]
/// ([`find_diagnostic`]) is printed alone: when there is one, it is the only
/// frame. A [`MultiError`] is one frame listing each member's cause, so every
/// member is shown, each as this function shows it.
pub fn cause_frames(e: &anyhow::Error) -> Vec<String> {
    if let Some(d) = find_diagnostic(e) {
        return vec![d.to_string()];
    }
    let texts: Vec<String> = hcore::hmemoizer::frames(e)
        .into_iter()
        .filter(|f| f.downcast_ref::<StageFrame>().is_none())
        .map(|f| match f.downcast_ref::<MultiError>() {
            Some(multi) => members_cause(multi),
            None => f.error().to_string(),
        })
        .filter(|s| !s.is_empty())
        .collect();
    hcore::hmemoizer::own_parts(texts)
}

/// A [`MultiError`]'s text with each member's [`cause_frames`] in place of its
/// `{:#}`, in the layout its `Display` has.
fn members_cause(multi: &MultiError) -> String {
    let mut out = format!("{} errors:", multi.0.len());
    for (i, member) in multi.0.iter().enumerate() {
        out.push_str(&format!("\n  [{i}] {}", cause_frames(member).join(": ")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{CancelledError, is_cancelled};
    use std::sync::Arc;

    fn addr(s: &str) -> Addr {
        hmodel::htaddr::parse_addr(s).expect("addr")
    }

    /// `e` as a memoizer hands it to a second waiter.
    fn shared(e: anyhow::Error) -> anyhow::Error {
        let arc = Arc::new(e);
        let keep = Arc::clone(&arc);
        let out = hcore::hmemoizer::unwrap_arc_err(arc);
        drop(keep);
        out
    }

    #[test]
    fn walker_finds_cancel_through_shared_anyhow() {
        let e = shared(with_stage(
            shared(anyhow::Error::new(CancelledError).context("waiting")),
            Boundary::Execute,
            &addr("//a:x"),
        ))
        .context("outer");
        assert!(e.downcast_ref::<CancelledError>().is_none());
        assert!(hcore::hmemoizer::find::<CancelledError>(&e).is_some());
        assert!(is_cancelled(&e));
    }

    #[test]
    fn walker_innermost_stage_frame_wins() {
        let leaf = anyhow::anyhow!("boom");
        let c = with_stage(leaf, Boundary::GetDef, &addr("//c:t"));
        let b = with_stage(
            shared(c).context("collecting"),
            Boundary::GetDef,
            &addr("//b:t"),
        );
        let a = with_stage(
            with_stage(b, Boundary::Link, &addr("//a:t")),
            Boundary::Execute,
            &addr("//a:t"),
        );
        let frame = innermost_stage_frame(&a).expect("a stage frame");
        assert_eq!(frame.addr, addr("//c:t"));
        assert_eq!(frame.stage(), Stage::Parse);
        // The outermost is still reachable, and every frame keeps its text.
        assert_eq!(
            hcore::hmemoizer::find::<StageFrame>(&a).map(|f| f.boundary),
            Some(Boundary::Execute)
        );
        assert_eq!(
            format!("{a:#}"),
            "execute //a:t: link: get_def: //b:t: collecting: get_def: //c:t: boom"
        );
    }

    /// A transitive dep's spec lookup keeps its `get spec:` text and is a
    /// spec-stage failure of the dep, not of its consumer.
    #[test]
    fn get_spec_frame_is_the_deps_spec_stage() {
        let e = with_stage(
            with_stage(anyhow::anyhow!("boom"), Boundary::GetSpec, &addr("//d:t")),
            Boundary::Link,
            &addr("//a:t"),
        );
        let frame = innermost_stage_frame(&e).expect("a stage frame");
        assert_eq!(
            (frame.boundary, frame.addr.clone(), frame.stage()),
            (Boundary::GetSpec, addr("//d:t"), Stage::Spec)
        );
        assert_eq!(format!("{e:#}"), "link: get spec: //d:t: boom");
        assert_eq!(cause_frames(&e), ["boom"]);
    }

    /// F2: no engine frame in what a renderer prints as the cause.
    #[test]
    fn rendered_messages_have_no_internal_frames() {
        let a = addr("//a:x");
        let e = anyhow::anyhow!("process exited with status: exit status: 5").context("driver run");
        let e = with_stage(e, Boundary::Run, &a);
        let e = with_stage(shared(e), Boundary::Execute, &a);
        let e = with_stage(with_stage(e, Boundary::Meta, &a), Boundary::Link, &a);
        let e = with_stage(e, Boundary::GetDef, &a);
        let frames = cause_frames(&e);
        assert_eq!(
            frames,
            ["driver run", "process exited with status: exit status: 5"]
        );
        for f in &frames {
            for internal in ["get_def:", "link", "meta", "execute "] {
                assert!(!f.starts_with(internal), "{f}");
            }
        }
    }

    #[test]
    fn a_diagnostic_is_printed_alone() {
        let d = Diagnostic::new(Stage::Eval, "name `x` is not defined")
            .with_location(SourceLoc::checked("p/BUILD".into(), 3, 1).expect("loc"));
        let e = anyhow::Error::new(d)
            .context("pkg: `p`")
            .context("probing package `//p`");
        let e = with_stage(e, Boundary::GetDef, &addr("//p:t"));
        assert_eq!(cause_frames(&e), ["p/BUILD:3:1: name `x` is not defined"]);
    }

    /// A keep-going link fanout records every broken input as one
    /// `MultiError`: one member's diagnostic is that member's, so it does not
    /// stand for the whole error, and no member's stage frames are printed.
    #[test]
    fn multierror_members_each_keep_their_cause() {
        let d = Diagnostic::new(Stage::Eval, "name `x` is not defined")
            .with_location(SourceLoc::checked("a/BUILD".into(), 3, 1).expect("loc"));
        let with_d = with_stage(anyhow::Error::new(d), Boundary::GetDef, &addr("//a:x"));
        let plain = with_stage(
            anyhow::anyhow!("target //b:y not found"),
            Boundary::GetDef,
            &addr("//b:y"),
        );
        let e = with_stage(
            anyhow::Error::new(MultiError(vec![with_d, plain])),
            Boundary::Link,
            &addr("//app:bin"),
        );
        assert!(
            find_diagnostic(&e).is_none(),
            "a member's is not the error's"
        );
        assert_eq!(
            cause_frames(&e),
            ["2 errors:\n  [0] a/BUILD:3:1: name `x` is not defined\n  [1] target //b:y not found"]
        );

        // Two members with stage frames and no diagnostic.
        let staged = |s: &str, a: &str| {
            with_stage(
                with_stage(anyhow::anyhow!("{s}"), Boundary::Run, &addr(a)),
                Boundary::Execute,
                &addr(a),
            )
        };
        let e = anyhow::Error::new(MultiError(vec![
            staged("exit 1", "//a:x"),
            staged("exit 2", "//b:y"),
        ]));
        let frames = cause_frames(&e);
        assert_eq!(frames, ["2 errors:\n  [0] exit 1\n  [1] exit 2"]);
        assert!(!frames[0].contains("execute "), "{frames:?}");
    }
}
