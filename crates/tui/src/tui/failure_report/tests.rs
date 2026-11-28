use super::*;
use hcore::diagnostic::Diagnostic;
use hcore::events::{ExitData, LeftBehindData, TimedOutData};

/// A slot at `dir` holding `log` as its `log.txt`, filled as `fill_id`; the
/// reference a record keeps to it, expecting `want`.
fn slot(dir: &Path, fill_id: Option<&str>, want: Option<&str>, log: &str) -> LogRef {
    std::fs::create_dir_all(dir).expect("slot dir");
    let manifest = match fill_id {
        Some(id) => format!(r#"{{"version":1,"fill_id":"{id}"}}"#),
        None => r#"{"version":1}"#.to_owned(),
    };
    std::fs::write(dir.join(failure_log::MANIFEST), manifest).expect("manifest");
    std::fs::write(dir.join(failure_log::LOG), log).expect("log");
    LogRef {
        path: Some(dir.join(failure_log::LOG).display().to_string()),
        fill_id: want.map(str::to_owned),
        lines: 0,
        memory_tail: None,
    }
}

fn target(addr: &str, d: Diagnostic) -> FailureRecord {
    FailureRecord {
        addr: Some(addr.to_owned()),
        entry: addr.to_owned(),
        path: vec![addr.to_owned()],
        ..FailureRecord::new(d.with_addr(addr))
    }
}

fn loc(file: &str, line: u32, col: u32) -> SourceLoc {
    SourceLoc::checked(file.to_owned(), line, col).expect("relative")
}

fn ctx(root: &Path, mode: RenderMode) -> RenderCtx {
    RenderCtx {
        source_root: Some(root.to_path_buf()),
        mode,
        ..RenderCtx::default()
    }
}

const MODES: [RenderMode; 2] = [RenderMode::Line, RenderMode::Report];

const P_BUILD: &str = "target(\n    name = \"t\",\n    driver = \"exec\",\n    dep = [],\n)\n";

/// A workspace holding the fixtures' source files, and the fixtures: one
/// per kind of failure the Goal's five questions are asked about.
fn fixtures(ws: &Path) -> Vec<FailureRecord> {
    let write = |rel: &str, text: &str| {
        let path = ws.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(path, text).expect("source");
    };
    write("p/BUILD", P_BUILD);
    write("d/BUILD", "x = 1\n\n\ntarget(srcs = srcs)\n");
    write(
        "c/BUILD",
        "\n\n\n\n\ntarget(\n    deps = [\"//d:missing\"],\n)\n",
    );

    let exec = FailedIn::Exec;
    let ran = FailureRecord {
        exit: Some(ExitData {
            code: Some(1),
            signal: None,
            status: "exit status: 1".into(),
        }),
        failed_in: exec,
        path: vec!["//a:root".into(), "//p:t".into()],
        log: Some(slot(
            &ws.join("slots/run"),
            Some("f1"),
            Some("f1"),
            "compiling\nerror: boom\n",
        )),
        left_behind: Some(LeftBehindData {
            log_readable: Some(true),
            ..LeftBehindData::default()
        }),
        ..target("//p:t", Diagnostic::new(Stage::Run, "exit status 1"))
    };
    let file = FailureRecord {
        addr: None,
        file: Some("d/BUILD".into()),
        entry: "//d:t".into(),
        path: vec![
            "//a:root".into(),
            "//b:x".into(),
            "//c:y".into(),
            "//d:t".into(),
        ],
        ..FailureRecord::new(
            Diagnostic::new(Stage::Eval, "Variable `srcs` not found")
                .with_location(loc("d/BUILD", 4, 15))
                .with_snippet("target(srcs = srcs)"),
        )
    };
    let missing = FailureRecord {
        path: vec!["//a:root".into(), "//b:x".into(), "//c:y".into()],
        ..target(
            "//c:y",
            Diagnostic::new(Stage::Link, "depends on //d:missing, which does not exist")
                .with_location(loc("c/BUILD", 7, 13))
                .with_hint(Hint::new(HintKind::DidYouMean, "//d:mising")),
        )
    };
    let option = target(
        "//p:t",
        Diagnostic::new(Stage::Parse, "unknown option `dep` for driver exec")
            .with_location(loc("p/BUILD", 4, 5))
            .with_hint(Hint::new(HintKind::DidYouMean, "deps"))
            .with_hint(Hint::new(
                HintKind::ValidValues,
                "valid options: deps, tools",
            )),
    );
    let timeout = FailureRecord {
        timed_out: Some(TimedOutData {
            timeout_ms: 10_000,
            abandoned: false,
        }),
        failed_in: exec,
        ..target("//p:slow", Diagnostic::new(Stage::Run, "timed out"))
    };
    vec![ran, file, missing, option, timeout]
}

/// I3: every fixture answers the Goal's five questions the same in both
/// text modes and in `--json`'s facts: what (the culprit and its
/// location), why (the message or status), where (the path), what now (the
/// hints), and the agent's fields (stage, log). An absent fact is an
/// explicit `null` in the JSON.
#[test]
fn golden_facts_equal_across_renderers() {
    let ws = tempfile::tempdir().expect("ws");
    for rec in fixtures(ws.path()) {
        let json = serde_json::to_value(json_facts(&rec)).expect("json");
        let d = &rec.diagnostic;
        // The agent's fields, each present.
        assert_eq!(json["stage"], d.stage.as_str(), "{rec:?}");
        for key in ["location", "hints", "truncated", "log_replaced", "streamed"] {
            assert!(json.get(key).is_some(), "{key} missing: {json}");
        }
        assert_eq!(
            json["location"],
            serde_json::to_value(&d.location).expect("loc"),
            "null when absent"
        );
        assert_eq!(
            json["hints"],
            serde_json::to_value(&d.hints).expect("hints")
        );
        assert_eq!(json["log_replaced"], false);

        let boxes: Vec<String> = MODES
            .iter()
            .map(|m| render_box(&rec, &ctx(ws.path(), *m)))
            .collect();
        for b in &boxes {
            // What.
            let name = rec.addr.as_deref().or(rec.file.as_deref()).expect("name");
            let header = b.lines().next().expect("header");
            assert!(header.starts_with(&format!("✗ {name}")), "{b}");
            if let Some(l) = &d.location {
                assert!(
                    b.contains(&format!("╭─[{l}]")),
                    "the location frames its excerpt: {b}"
                );
            }
            // Why.
            match status(&rec) {
                Some(s) => assert!(header.contains(&s), "{b}"),
                None => assert!(b.contains(&d.message), "{b}"),
            }
            // Where.
            if rec.path.len() > 1 {
                assert!(b.contains("reached by: //a:root"), "{b}");
            }
            if rec.file.is_some() {
                assert!(
                    b.contains("//c:y depends on //d:t, which this file defines"),
                    "{b}"
                );
            }
            // What now.
            for h in &d.hints {
                assert!(b.contains(&h.text), "{h:?}: {b}");
            }
            // The log.
            assert_eq!(b.contains("╭─[log · 2 lines]"), rec.log.is_some(), "{b}");
        }
        // Both text modes say the same up to what they may name of heph's
        // home; these fixtures leave nothing there.
        assert_eq!(boxes[0], boxes[1], "{rec:?}");
    }
}

/// U4, in both text modes: an unknown option's box frames the source line
/// and puts the marker under the kwarg, the near-miss at its end, and the
/// valid options after the frame.
#[test]
fn unknown_option_framed_box_marker_under_kwarg() {
    let ws = tempfile::tempdir().expect("ws");
    let rec = fixtures(ws.path()).swap_remove(3);
    for mode in MODES {
        assert_eq!(
            render_box(&rec, &ctx(ws.path(), mode)),
            "✗ //p:t\n\
             \x20 unknown option `dep` for driver exec\n\
             \x20   ╭─[p/BUILD:4:5]\n\
             \x20 4 │     dep = [],\n\
             \x20   │     ─┬─\n\
             \x20   │      ╰── did you mean `deps`?\n\
             \x20   ╰────\n\
             \x20 valid options: deps, tools\n",
            "{mode:?}"
        );
    }
}

fn marked(rows: &[FrameRow]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut it = rows.iter().peekable();
    while let Some(row) = it.next() {
        if let FrameRow::Source { text, .. } = row {
            let marker = match it.peek() {
                Some(FrameRow::Marker(m)) => m.clone(),
                _ => String::new(),
            };
            out.push((text.clone(), marker));
        }
    }
    out
}

#[test]
fn reframe_tabs_before_kwarg() {
    let rows = reframe(
        Some("target(\n\tdep = [],\n)\n"),
        None,
        &loc("p/BUILD", 2, 2),
        None,
    )
    .expect("framed");
    assert_eq!(
        marked(&rows),
        [("        dep = [],".to_owned(), "        ───".to_owned())]
    );
}

#[test]
fn reframe_span_over_3_lines() {
    let src = "x = f(\n    a,\n    b)\ny = 1\n";
    let snippet = "x = f(\n    a,\n    b)";
    let rows = reframe(Some(src), Some(snippet), &loc("p/BUILD", 1, 5), None).expect("framed");
    assert_eq!(
        marked(&rows),
        [
            ("x = f(".to_owned(), "    ──".to_owned()),
            ("    a,".to_owned(), "    ──".to_owned()),
            ("    b)".to_owned(), "    ──".to_owned()),
        ]
    );
    let numbers: Vec<usize> = rows
        .iter()
        .filter_map(|r| match r {
            FrameRow::Source { line, .. } => Some(*line),
            FrameRow::Marker(_) => None,
        })
        .collect();
    assert_eq!(numbers, [1, 2, 3]);
    // A snippet the file no longer has: the one line at the location.
    let rows =
        reframe(Some(src), Some("other\nlines"), &loc("p/BUILD", 1, 5), None).expect("framed");
    assert_eq!(marked(&rows), [("x = f(".to_owned(), "    ─".to_owned())]);
}

#[test]
fn reframe_crlf() {
    let rows = reframe(
        Some("a\r\n  dep = 1\r\n"),
        None,
        &loc("p/BUILD", 2, 3),
        None,
    )
    .expect("framed");
    assert_eq!(
        marked(&rows),
        [("  dep = 1".to_owned(), "  ───".to_owned())]
    );
}

#[test]
fn reframe_bom() {
    let rows =
        reframe(Some("\u{feff}dep = 1\n"), None, &loc("p/BUILD", 1, 1), None).expect("framed");
    assert_eq!(marked(&rows), [("dep = 1".to_owned(), "───".to_owned())]);
}

#[test]
fn reframe_10k_char_line() {
    let line = format!("{} dep {}", "x".repeat(5000), "y".repeat(5000));
    let rows = reframe(Some(&line), None, &loc("p/BUILD", 1, 5002), None).expect("framed");
    let [(text, marker)] = marked(&rows).try_into().expect("one line");
    assert!(text.chars().count() <= SNIPPET_LINE_CAP, "{}", text.len());
    assert!(text.starts_with('…') && text.ends_with('…'), "{text}");
    let at = marker.find('─').expect("a marker");
    assert_eq!(marker.chars().filter(|&c| c == '─').count(), 3);
    // The marker's column is the span's: `dep` starts there.
    let under: String = text.chars().skip(at).take(3).collect();
    assert_eq!(under, "dep");
}

#[test]
fn reframe_e_acute_before_span() {
    let rows =
        reframe(Some("é = 1; dep = 2\n"), None, &loc("p/BUILD", 1, 8), None).expect("framed");
    assert_eq!(
        marked(&rows),
        [("é = 1; dep = 2".to_owned(), "       ───".to_owned())]
    );
}

#[test]
fn reframe_stale_span() {
    let src = "a = 1\nb = 2\n";
    assert_eq!(reframe(Some(src), None, &loc("p/BUILD", 9, 1), None), None);
    assert_eq!(
        reframe(Some(src), None, &loc("p/BUILD", 1, 40), None),
        None,
        "a column past the line's end"
    );
    // Rendered: `file:line`, no excerpt.
    let ws = tempfile::tempdir().expect("ws");
    std::fs::create_dir_all(ws.path().join("p")).expect("dir");
    std::fs::write(ws.path().join("p/BUILD"), src).expect("src");
    let rec = target(
        "//p:t",
        Diagnostic::new(Stage::Parse, "bad").with_location(loc("p/BUILD", 9, 1)),
    );
    let b = render_box(&rec, &ctx(ws.path(), RenderMode::Line));
    assert_eq!(b, "✗ //p:t\n  bad\n  at p/BUILD:9\n");
}

#[test]
fn reframe_builtin_source_no_file() {
    assert_eq!(
        reframe(None, Some("def f():"), &loc("x.bzl", 3, 1), None),
        None
    );
    let ws = tempfile::tempdir().expect("ws");
    let rec = target(
        "//p:t",
        Diagnostic::new(Stage::Eval, "bad")
            .with_location(loc("prelude/defs.bzl", 3, 1))
            .with_snippet("def f():"),
    );
    let b = render_box(&rec, &ctx(ws.path(), RenderMode::Report));
    assert!(b.contains("  at prelude/defs.bzl:3\n"), "{b}");
    assert!(!b.contains("╭─["), "{b}");
}

fn widths(rows: &[String]) -> Vec<usize> {
    rows.iter().map(|r| styled_width(r)).collect()
}

#[test]
fn wrap_cjk() {
    let rows = wrap("漢字漢字漢字", 5);
    assert_eq!(rows, ["漢字", "漢字", "漢字"]);
    assert!(widths(&rows).iter().all(|&w| w <= 5));
    assert_eq!(wrap("ab 漢字", 4), ["ab", "漢字"]);
}

#[test]
fn wrap_long_token() {
    assert_eq!(wrap("a abcdefghij", 4), ["a", "abcd", "efgh", "ij"]);
    // Never between a char and its combining mark.
    let rows = wrap("e\u{301}e\u{301}e\u{301}", 2);
    assert_eq!(rows, ["e\u{301}e\u{301}", "e\u{301}"]);
    // Spaces at a break start no row; leading spaces are kept.
    assert_eq!(wrap("  ab cd", 5), ["  ab", "cd"]);
    // An SGR takes no column and is never split.
    assert_eq!(wrap("\x1b[31mabcd\x1b[0m", 2), ["\x1b[31mab", "cd\x1b[0m"]);
}

#[test]
fn wrap_width_1() {
    assert_eq!(wrap("ab 漢", 1), ["a", "b", "漢"]);
    assert_eq!(wrap("ab", 0), ["a", "b"]);
    assert_eq!(wrap("", 1), [""]);
    // A whole box at width 1 renders.
    let ws = tempfile::tempdir().expect("ws");
    for rec in fixtures(ws.path()) {
        let c = RenderCtx {
            width: Some(1),
            ..ctx(ws.path(), RenderMode::Report)
        };
        assert!(!render_box(&rec, &c).is_empty());
    }
}

fn long_failure() -> FailureRecord {
    let msg = format!("{}cause-at-the-end", "word ".repeat(40));
    target("//p:t", Diagnostic::new(Stage::Parse, msg))
}

/// The width is the context's when the box is drawn: a terminal resized
/// between the run and the report wraps the report at the final width.
#[test]
fn wrap_resized_between_run_and_report() {
    let rec = long_failure();
    let draw = |w| {
        render_box(
            &rec,
            &RenderCtx {
                width: Some(w),
                ..RenderCtx::default()
            },
        )
    };
    let (narrow, wide) = (draw(40), draw(100));
    for (b, w) in [(&narrow, 40), (&wide, 100)] {
        assert!(b.lines().all(|l| width(l) <= w), "{w}: {b}");
    }
    assert!(narrow.lines().count() > wide.lines().count());
}

#[test]
fn wrap_cause_at_end_visible() {
    let b = render_box(
        &long_failure(),
        &RenderCtx {
            width: Some(30),
            ..RenderCtx::default()
        },
    );
    assert!(b.contains("cause-at-the-end"), "{b}");
    assert!(b.lines().all(|l| width(l) <= 30), "{b}");
    // A continuation is indented past the row it continues.
    assert!(
        b.lines().nth(2).expect("wrapped").starts_with("    word"),
        "{b}"
    );
}

/// Every fixture's box, in both modes, with colour off and on.
fn every_box(ws: &Path, extra: &[FailureRecord], rel: &WorkspaceRel) -> Vec<String> {
    let mut recs = fixtures(ws);
    recs.extend_from_slice(extra);
    let mut out = Vec::new();
    for rec in &recs {
        for mode in MODES {
            for color in [false, true] {
                let c = RenderCtx {
                    rel: rel.clone(),
                    color,
                    ..ctx(ws, mode)
                };
                out.push(render_box(rec, &c));
            }
        }
    }
    out
}

/// F2: the renderer adds no engine frame to what it was given.
#[test]
fn rendered_messages_have_no_internal_frames() {
    let ws = tempfile::tempdir().expect("ws");
    for b in every_box(ws.path(), &[], &WorkspaceRel::default()) {
        for frame in ["get_def:", "link:", "meta:", "execute", "get spec:"] {
            assert!(!b.contains(frame), "{frame}: {b}");
        }
    }
}

/// D2: heph's text names workspace paths relative: the message, a cause, a
/// hint, the location.
#[test]
fn rendered_messages_have_no_absolute_workspace_paths() {
    let ws = tempfile::tempdir().expect("ws");
    let root = ws.path().to_string_lossy().into_owned();
    let rec = FailureRecord {
        causes: vec![format!("{root}/q/BUILD: bad")],
        ..target(
            "//p:t",
            Diagnostic::new(Stage::Parse, format!("cannot read {root}/p/data.txt"))
                .with_hint(Hint::new(HintKind::Other, format!("see {root}/docs"))),
        )
    };
    let rel = WorkspaceRel::workspace(ws.path());
    for b in every_box(ws.path(), &[rec], &rel) {
        assert!(!b.contains(&format!("{root}/")), "{b}");
    }
    let b = render_box(
        &FailureRecord {
            causes: vec![format!("{root}/q/BUILD: bad")],
            ..target(
                "//p:t",
                Diagnostic::new(Stage::Parse, format!("cannot read {root}/p/data.txt")),
            )
        },
        &RenderCtx {
            rel,
            ..RenderCtx::default()
        },
    );
    assert_eq!(b, "✗ //p:t\n  cannot read p/data.txt\n  - q/BUILD: bad\n");
}

/// F2: nothing reaches a box through `Debug` (`Int(3)`, `Some(..)`, a
/// struct's braces): a record with every fact set draws only words, and the
/// values in its messages stay in the JSON notation they were written in.
#[test]
fn rendered_values_are_json_notation() {
    use hcore::htvalue::Value;
    let ws = tempfile::tempdir().expect("ws");
    let value = Value::Map(
        [
            ("X".to_owned(), Value::Bool(true)),
            ("Y".to_owned(), Value::Null()),
        ]
        .into(),
    );
    let full = FailureRecord {
        causes: vec![format!(
            "expected 3, got {}",
            Value::List(vec![Value::String("X".into())]).render()
        )],
        more_causes: 2,
        exit: Some(ExitData {
            code: None,
            signal: Some(libc::SIGKILL),
            status: "signal: 9".into(),
        }),
        failed_in: FailedIn::Exec,
        frozen_diff: Some("-a\n+b".into()),
        left_behind: Some(LeftBehindData {
            sandbox: Some("/h/sandbox/x (kept)".into()),
            log_readable: Some(false),
            ..LeftBehindData::default()
        }),
        ..target(
            "//p:t",
            Diagnostic::new(
                Stage::Run,
                format!("value {} is not a list", value.render()),
            )
            .with_location(loc("p/BUILD", 4, 5))
            .with_hint(Hint::new(HintKind::DeclareTool, "declare the tool")),
        )
    };
    for mode in MODES {
        let b = render_box(&full, &ctx(ws.path(), mode));
        assert!(
            b.contains(r#"value {"X": true, "Y": null} is not a list"#),
            "{b}"
        );
        assert!(b.contains(r#"got ["X"]"#), "{b}");
    }
    for b in every_box(ws.path(), &[full], &WorkspaceRel::default()) {
        for debug in [
            "Int(", "String(", "Error {", "Some(", "None", "True", "Exit", "Stage::", " { ",
        ] {
            assert!(!b.contains(debug), "{debug}: {b}");
        }
    }
}

/// F2: a failure before the run had no log; its box does not say none was
/// kept. A failure of the process that kept none still says so.
#[test]
fn no_log_line_for_failure_before_run() {
    let left = LeftBehindData {
        log_readable: Some(false),
        ..LeftBehindData::default()
    };
    let before = FailureRecord {
        left_behind: Some(left.clone()),
        ..target("//p:t", Diagnostic::new(Stage::Link, "bad dep"))
    };
    for mode in MODES {
        let b = render_box(
            &before,
            &RenderCtx {
                mode,
                ..RenderCtx::default()
            },
        );
        assert_eq!(b, "✗ //p:t\n  bad dep\n", "{mode:?}");
    }
    let ran = FailureRecord {
        failed_in: FailedIn::Exec,
        ..before
    };
    assert!(render_box(&ran, &RenderCtx::default()).contains("  log: none kept\n"));
}

/// The rows the report had before the renderers merged (D5): what the
/// culprit took down, and the kept `failure_out` groups, in both modes.
#[test]
fn box_shows_took_down_and_kept_groups_in_both_modes() {
    let group = |name: &str, files| hcore::events::KeptGroup {
        name: name.into(),
        files,
    };
    let rec = FailureRecord {
        failed_in: FailedIn::Exec,
        took_down: Some(hcore::events::TookDownData {
            named: vec!["//p:a".into(), "//p:b".into()],
            total: 9,
        }),
        left_behind: Some(LeftBehindData {
            outputs: Some("/h/failures/k/out".into()),
            kept: vec![group("a.txt", 1), group("one/", 1), group("report/", 3)],
            kept_more: 2,
            ..LeftBehindData::default()
        }),
        ..target("//p:t", Diagnostic::new(Stage::Run, "boom"))
    };
    let draw = |mode| {
        render_box(
            &rec,
            &RenderCtx {
                mode,
                ..RenderCtx::default()
            },
        )
    };
    assert_eq!(
        draw(RenderMode::Report),
        "✗ //p:t\n  boom\n  took down: //p:a, //p:b +7 more\n\
         \x20 outputs: a.txt, one/ (1 file), report/ (3 files), +2 more · heph inspect failure //p:t\n"
    );
    assert_eq!(
        draw(RenderMode::Line),
        "✗ //p:t\n  boom\n  took down: //p:a, //p:b +7 more\n\
         \x20 outputs: a.txt, one/ (1 file), report/ (3 files), +2 more · /h/failures/k/out\n"
    );
}

/// Report mode names no path into heph's home; line mode names the slot's.
#[test]
fn report_mode_names_commands_not_slot_paths() {
    let rec = FailureRecord {
        failed_in: FailedIn::Exec,
        left_behind: Some(LeftBehindData {
            log_path: Some("/h/failures/k/log.txt".into()),
            outputs: Some("/h/failures/k/out".into()),
            log_readable: Some(true),
            ..LeftBehindData::default()
        }),
        ..target("//p:t", Diagnostic::new(Stage::Run, "boom"))
    };
    let draw = |mode| {
        render_box(
            &rec,
            &RenderCtx {
                mode,
                ..RenderCtx::default()
            },
        )
    };
    assert_eq!(
        draw(RenderMode::Line),
        "✗ //p:t\n  boom\n  full log: /h/failures/k/log.txt\n  outputs: /h/failures/k/out\n  \
         log: heph log //p:t\n"
    );
    assert_eq!(
        draw(RenderMode::Report),
        "✗ //p:t\n  boom\n  outputs: kept · heph inspect failure //p:t\n  log: heph log //p:t\n"
    );
}

/// The log as `open_tail` finds it: this failure's, streamed (the pointer),
/// replaced by a newer failed run, cleared, or an older heph's slot read
/// with no claim. `--json` says replaced only when it was.
#[test]
fn log_rows_follow_the_slot() {
    let ws = tempfile::tempdir().expect("ws");
    let with = |log: LogRef, streamed: bool| FailureRecord {
        failed_in: FailedIn::Exec,
        log: Some(log),
        streamed,
        ..target("//p:t", Diagnostic::new(Stage::Run, "boom"))
    };
    let draw = |rec: &FailureRecord| render_box(rec, &RenderCtx::default());

    let own = with(
        slot(&ws.path().join("a"), Some("f1"), Some("f1"), "x\ny\n"),
        false,
    );
    assert_eq!(
        draw(&own),
        "✗ //p:t\n  boom\n    ╭─[log · 2 lines]\n  1 │ x\n  2 │ y\n    ╰────\n"
    );
    assert!(!json_facts(&own).log_replaced);

    let lines: String = (1..=5).map(|i| format!("l{i}\n")).collect();
    let short = RenderCtx {
        log_lines: 2,
        ..RenderCtx::default()
    };
    let cut = with(
        slot(&ws.path().join("cut"), Some("f1"), Some("f1"), &lines),
        false,
    );
    assert_eq!(
        render_box(&cut, &short),
        "✗ //p:t\n  boom\n    ╭─[log · 5 lines]\n    │ … 3 earlier lines\n  4 │ l4\n  \
         5 │ l5\n    ╰────\n"
    );

    let streamed = with(
        slot(&ws.path().join("b"), Some("f1"), Some("f1"), "x\ny\n"),
        true,
    );
    assert_eq!(
        draw(&streamed),
        "✗ //p:t\n  boom\n  (output not shown here: 2 lines · heph log //p:t)\n"
    );
    assert!(json_facts(&streamed).streamed);

    let empty = with(
        slot(&ws.path().join("e"), Some("f1"), Some("f1"), ""),
        false,
    );
    assert_eq!(draw(&empty), "✗ //p:t\n  boom\n  (no output)\n");

    let replaced = with(
        slot(&ws.path().join("c"), Some("f2"), Some("f1"), "x\n"),
        false,
    );
    assert_eq!(
        draw(&replaced),
        "✗ //p:t\n  boom\n  log replaced by a newer failed run: heph log //p:t\n"
    );
    assert!(json_facts(&replaced).log_replaced);

    let mut gone = slot(&ws.path().join("d"), Some("f1"), Some("f1"), "x\n");
    gone.path = Some(ws.path().join("nowhere/log.txt").display().to_string());
    let cleared = with(gone, false);
    assert_eq!(draw(&cleared), "✗ //p:t\n  boom\n  log cleared\n");
    assert!(!json_facts(&cleared).log_replaced);

    let older = with(slot(&ws.path().join("o"), None, None, "x\n"), false);
    assert_eq!(
        draw(&older),
        "✗ //p:t\n  boom\n    ╭─[log · 1 line]\n  1 │ x\n    ╰────\n"
    );

    let memory = with(
        LogRef {
            lines: 10,
            memory_tail: Some("i\nj".into()),
            ..LogRef::default()
        },
        false,
    );
    assert_eq!(
        draw(&memory),
        "✗ //p:t\n  boom\n     ╭─[log · 10 lines]\n     │ … 8 earlier lines\n   9 │ i\n  \
         10 │ j\n     ╰────\n"
    );

    // U26: a streamed target whose log `heph log` cannot open shows its
    // tail, not a pointer with no command: the memory tail of a log kept
    // nowhere, and a slot's log the engine says cannot be read.
    let streamed_memory = with(
        LogRef {
            lines: 10,
            memory_tail: Some("i\nj".into()),
            ..LogRef::default()
        },
        true,
    );
    assert_eq!(draw(&streamed_memory), draw(&memory));
    let mut streamed_unreadable = with(
        slot(&ws.path().join("u"), Some("f1"), Some("f1"), "x\ny\n"),
        true,
    );
    streamed_unreadable.left_behind = Some(LeftBehindData {
        log_readable: Some(false),
        ..LeftBehindData::default()
    });
    let drawn = draw(&streamed_unreadable);
    assert!(
        drawn.contains("  1 │ x\n  2 │ y\n") && !drawn.contains("output not shown here"),
        "{drawn}"
    );
}

/// A log line's controls are escaped, never sent; heph's home is redacted
/// from it, its workspace paths kept (D2, I9).
#[test]
fn log_lines_escaped_and_home_redacted() {
    let ws = tempfile::tempdir().expect("ws");
    let rec = FailureRecord {
        failed_in: FailedIn::Exec,
        log: Some(slot(
            &ws.path().join("a"),
            Some("f"),
            Some("f"),
            "\x1b[?1049h /h/.heph/x /ws/a\n",
        )),
        ..target("//p:t", Diagnostic::new(Stage::Run, "boom"))
    };
    let c = RenderCtx {
        redact: Redact::of([PathBuf::from("/h/.heph")]),
        rel: WorkspaceRel::of([PathBuf::from("/ws")]),
        ..RenderCtx::default()
    };
    let b = render_box(&rec, &c);
    assert!(b.contains("1 │ \\x1b[?1049h <heph home>/x /ws/a\n"), "{b}");
}

#[test]
/// A driver's own diagnostic beside an exit (CQ-1) is shown by the box, the
/// report's overflow line and the streamed line, hints too. A message that
/// only says how the process ended (compact's `process_end_message`) is not
/// repeated under a header that says it.
fn raised_diagnostic_with_exit_is_shown() {
    let exit = |rec: FailureRecord| FailureRecord {
        exit: Some(ExitData {
            code: Some(1),
            signal: None,
            status: "exit status: 1".into(),
        }),
        failed_in: FailedIn::Exec,
        ..rec
    };
    let raised = exit(target(
        "//p:vet",
        Diagnostic::new(Stage::Run, "go vet found 2 issues")
            .with_hint(Hint::new(HintKind::Other, "run go vet ./... locally")),
    ));
    let ctx = RenderCtx::default();
    let b = render_box(&raised, &ctx);
    assert!(b.starts_with("✗ //p:vet · exit status 1\n"), "{b}");
    assert!(b.contains("go vet found 2 issues"), "{b}");
    assert!(b.contains("run go vet ./... locally"), "{b}");
    assert_eq!(
        one_line(&raised, &ctx),
        "✗ //p:vet · exit status 1 · go vet found 2 issues"
    );
    assert_eq!(
        streamed_exec_line(&raised, false),
        "✗ //p:vet · exit status 1 · heph log //p:vet\n  \
         go vet found 2 issues\n  \
         run go vet ./... locally"
    );

    // Said only by the header: no row, no second line.
    let status_only = exit(target(
        "//p:t",
        Diagnostic::new(Stage::Run, "exit status 1"),
    ));
    let b = render_box(&status_only, &ctx);
    assert_eq!(b.matches("exit status 1").count(), 1, "{b}");
    assert_eq!(one_line(&status_only, &ctx), "✗ //p:t · exit status 1");
    assert_eq!(
        streamed_exec_line(&status_only, false),
        "✗ //p:t · exit status 1 · heph log //p:t"
    );
}

#[test]
fn header_names_exit_signal_timeout_and_diff() {
    let with = |f: fn(&mut FailureRecord)| {
        let mut rec = target("//p:t", Diagnostic::new(Stage::Run, "m"));
        f(&mut rec);
        render_box(&rec, &RenderCtx::default())
            .lines()
            .next()
            .expect("header")
            .to_owned()
    };
    assert_eq!(
        with(|r| {
            r.exit = Some(ExitData {
                code: Some(3),
                signal: None,
                status: String::new(),
            })
        }),
        "✗ //p:t · exit status 3"
    );
    assert_eq!(
        with(|r| {
            r.exit = Some(ExitData {
                code: None,
                signal: Some(libc::SIGKILL),
                status: String::new(),
            })
        }),
        format!("✗ //p:t · killed by signal {} (SIGKILL)", libc::SIGKILL)
    );
    assert_eq!(
        with(|r| {
            r.timed_out = Some(TimedOutData {
                timeout_ms: 90_000,
                abandoned: true,
            })
        }),
        "✗ //p:t · timed out after 1m30s (its timeout)"
    );
    assert_eq!(
        with(|r| r.frozen_diff = Some("-a\n+b".into())),
        "✗ //p:t · generated output differs from tree"
    );
    let file = FailureRecord {
        addr: None,
        file: Some("d/BUILD".into()),
        ..FailureRecord::new(Diagnostic::new(Stage::Eval, "m"))
    };
    assert_eq!(render_box(&file, &RenderCtx::default()), "✗ d/BUILD\n  m\n");
}

/// U7, the helper: of 80 failures the first 50 get a box, the other 30 a
/// line each, then the count and where the rest are.
#[test]
fn tui_report_80_failures_50_boxes_30_lines() {
    let recs: Vec<FailureRecord> = (0..80)
        .map(|i| target(&format!("//p:t{i}"), Diagnostic::new(Stage::Run, "boom")))
        .collect();
    let out = render_report(&recs, &RenderCtx::default());
    let lines: Vec<&str> = out.lines().collect();
    let boxes = lines
        .iter()
        .filter(|l| l.starts_with("✗ ") && !l.contains(" · "))
        .count();
    let short: Vec<&&str> = lines
        .iter()
        .filter(|l| l.starts_with("✗ ") && l.contains(" · "))
        .collect();
    assert_eq!(boxes, BOX_CAP);
    assert_eq!(short.len(), 30);
    assert_eq!(*short[0], "✗ //p:t50 · boom");
    assert_eq!(lines.last(), Some(&"30 more: heph inspect failure --json"));
    assert_eq!(lines.iter().filter(|l| **l == "  boom").count(), BOX_CAP);
    // Under the cap: boxes only.
    let out = render_report(&recs[..3], &RenderCtx::default());
    assert_eq!(
        out,
        "✗ //p:t0\n  boom\n\n✗ //p:t1\n  boom\n\n✗ //p:t2\n  boom\n"
    );
}

#[test]
fn totals_skipped_fail_fast_and_streamed_lines() {
    assert_eq!(
        totals_line(&["//c:y", "d/BUILD"], 3),
        "2 failed: //c:y, d/BUILD · 3 skipped"
    );
    assert_eq!(
        totals_line(&["a", "b", "c", "d", "e", "f", "g"], 0),
        "7 failed: a, b, c, d, e +2 more"
    );
    assert_eq!(totals_line::<&str>(&[], 1), "1 skipped");
    assert_eq!(totals_line::<&str>(&[], 0), "");
    assert_eq!(
        skipped_line("//a:root", "//c:y"),
        "– //a:root skipped: //c:y failed"
    );
    assert_eq!(
        fail_fast_lines(&["//p:slow"], 2),
        "stopped by --fail-fast: //p:slow (was running)\n\
         stopped by --fail-fast: 2 queued targets never started\n"
    );
    assert_eq!(fail_fast_lines::<&str>(&[], 0), "");
    let rec = FailureRecord {
        exit: Some(ExitData {
            code: Some(1),
            signal: None,
            status: "exit status: 1".into(),
        }),
        // As compact records a process failure with no diagnostic.
        ..target("//p:a b", Diagnostic::new(Stage::Run, "exit status 1"))
    };
    assert_eq!(
        streamed_exec_line(&rec, false),
        "✗ //p:a b · exit status 1 · heph log '//p:a b'"
    );
    let painted = streamed_exec_line(&rec, true);
    assert!(
        painted.contains("\x1b[") && painted.contains("heph log '//p:a b'"),
        "{painted:?}"
    );
}
