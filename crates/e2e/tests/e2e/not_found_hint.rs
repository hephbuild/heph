//! The "not on this target's PATH" hint (error-reporting D8, RUN-06/07): a
//! target that runs a program it never declared gets a hint naming it, from
//! a shell's exit 127 or the exec driver's spawn `ENOENT`. The wording and
//! the adversarial rows are unit tests in `plugin-exec`'s `not_found`.

use crate::common::Workspace;
use crate::source_location::{fail_in, text};
use hcore::diagnostic::HintKind;

/// The hints and the text of the one failure of `//p:t` when `p/BUILD` holds
/// `build`.
async fn fails(build: &str) -> (Vec<(HintKind, String)>, String) {
    let ws = Workspace::new();
    ws.write_build_file("p", build);
    let rs = ws.engine.new_state_with_events(false, None);
    let failures = fail_in(&ws, &rs, &["//p:t"]).await;
    assert_eq!(failures.len(), 1, "one failure");
    let f = failures.first().expect("a failure");
    let hints = f
        .record
        .diagnostic
        .hints
        .iter()
        .map(|h| (h.kind, h.text.clone()))
        .collect();
    (hints, text(f))
}

/// RUN-06: bash's `jq: command not found` and its exit 127 said only
/// `exit status 127`. The program is named `heph-e2e-jq` so that no host's
/// `/usr/bin` has it.
#[tokio::test]
async fn bash_127_jq_gets_path_hint() {
    let (hints, msg) = fails(
        r#"target(name = "t", driver = "bash", run = "heph-e2e-jq . in.json > $OUT", out = "o")"#,
    )
    .await;
    assert!(msg.contains("exit status 127"), "{msg}");
    let [(kind, hint)] = hints.as_slice() else {
        panic!("one hint: {hints:?}");
    };
    assert_eq!(*kind, HintKind::DeclareTool);
    assert!(
        hint.starts_with("`heph-e2e-jq` is not on this target's PATH: a target sees only"),
        "{hint}"
    );
    assert!(hint.contains("Declare it in `tools = [...]`."), "{hint}");
}

/// A script that exits 127 itself, with no not-found line, gets no hint.
#[tokio::test]
async fn bash_exit_127_without_a_not_found_line_has_no_hint() {
    let (hints, msg) = fails(
        r#"target(name = "t", driver = "bash", run = "echo giving up >&2; exit 127", out = "o")"#,
    )
    .await;
    assert!(msg.contains("exit status 127"), "{msg}");
    assert!(hints.is_empty(), "{hints:?}");
}

/// RUN-07: the exec driver's spawn `ENOENT` for its program gets the hint,
/// and no longer tells the reader to check a working directory that exists.
#[tokio::test]
async fn exec_spawn_not_found_hint_has_no_cwd_clause() {
    let (hints, msg) =
        fails(r#"target(name = "t", driver = "exec", run = ["heph-e2e-jq", "."], out = "o")"#)
            .await;
    assert!(msg.contains("spawn child process \"heph-e2e-jq\""), "{msg}");
    assert!(!msg.contains("working directory"), "{msg}");
    let [(kind, hint)] = hints.as_slice() else {
        panic!("one hint: {hints:?}");
    };
    assert_eq!(*kind, HintKind::DeclareTool);
    assert!(
        hint.starts_with("`heph-e2e-jq` is not on this target's PATH"),
        "{hint}"
    );
}
