use super::*;
use crate::fixtures::{Row, args, row};
use crate::never_cancelled;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A writer whose bytes the test can read while it is still in use.
#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Vec<u8>>>);

impl Shared {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

impl Write for Shared {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .map_err(|e| io::Error::other(e.to_string()))?
            .extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Accepts `ok` writes, then fails every write with `kind`.
struct Failing {
    ok: usize,
    error: fn() -> io::Error,
    out: Vec<u8>,
}

impl Write for Failing {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.ok == 0 {
            return Err((self.error)());
        }
        self.ok -= 1;
        self.out.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn print_list(flags: &[&str], n: usize, style: Style) -> String {
    let args = args(flags);
    let mut out = Vec::new();
    args.printer_to(&mut out, style)
        .list_iter((0..n).map(row), &never_cancelled)
        .unwrap();
    String::from_utf8(out).unwrap()
}

fn print_record(flags: &[&str], style: Style) -> String {
    let args = args(flags);
    let mut out = Vec::new();
    args.printer_to(&mut out, style)
        .record(&row(0), &never_cancelled)
        .unwrap();
    String::from_utf8(out).unwrap()
}

#[test]
fn compact_layout() {
    assert_eq!(print_list(&["--json"], 0, Style::Compact), "[]\n");
    assert_eq!(
        print_list(&["--json"], 2, Style::Compact),
        "[\n{\"addr\":\"//p:t0\",\"labels\":[\"test\"]},\n{\"addr\":\"//p:t1\",\"labels\":[\"test\"]}\n]\n"
    );
    assert_eq!(
        print_record(&["--json"], Style::Compact),
        "{\"addr\":\"//p:t0\",\"labels\":[\"test\"]}\n"
    );
    let args = args(&["--json"]);
    let mut out = Vec::new();
    args.printer_to(&mut out, Style::Compact)
        .null(&never_cancelled)
        .unwrap();
    assert_eq!(out, b"null\n");
}

#[test]
fn fields_select_and_keep_key_order() {
    // Default set: free fields only, in declaration order.
    let all = print_record(&["--fields", "all"], Style::Compact);
    assert_eq!(
        all,
        "{\"addr\":\"//p:t0\",\"labels\":[\"test\"],\"driver\":\"exec\",\"deps\":[\"//p:d0\"],\"hashin\":\"h0\"}\n"
    );
    // A bare name replaces the default set; the order stays the record's.
    assert_eq!(
        print_record(&["--fields", "hashin,addr"], Style::Compact),
        "{\"addr\":\"//p:t0\",\"hashin\":\"h0\"}\n"
    );
    // `+name` adds to it.
    assert_eq!(
        print_record(&["--fields", "+driver"], Style::Compact),
        "{\"addr\":\"//p:t0\",\"labels\":[\"test\"],\"driver\":\"exec\"}\n"
    );
    // Selected and null stays null; not selected is absent.
    let args = args(&["--fields", "addr,hashin"]);
    let mut out = Vec::new();
    let mut r = row(0);
    r.hashin = None;
    args.printer_to(&mut out, Style::Compact)
        .record(&r, &never_cancelled)
        .unwrap();
    assert_eq!(out, b"{\"addr\":\"//p:t0\",\"hashin\":null}\n");
}

#[test]
fn record_keys_subset_of_fields() {
    crate::check_keys(&row(0)).unwrap();
}

#[test]
fn writer_tty_and_pipe_agree() {
    for flags in [&["--json"][..], &["--fields", "all"]] {
        for n in [0, 1, 3] {
            let pretty = print_list(flags, n, Style::Pretty);
            let compact = print_list(flags, n, Style::Compact);
            let p: serde_json::Value = serde_json::from_str(&pretty).unwrap();
            let c: serde_json::Value = serde_json::from_str(&compact).unwrap();
            assert_eq!(p, c, "n={n}");
            // Only whitespace differs, key order included.
            let strip = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
            assert_eq!(strip(&pretty), strip(&compact), "n={n}");
        }
        let pretty = print_record(flags, Style::Pretty);
        let compact = print_record(flags, Style::Compact);
        assert!(pretty.contains("\n  \"addr\""), "{pretty}");
        let strip = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
        assert_eq!(strip(&pretty), strip(&compact));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn list_writer_streams() {
    let args = args(&["--json"]);
    let sink = Shared::default();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<anyhow::Result<Row>>();
    let rows = tokio_stream_from(rx);
    tx.send(Ok(row(0))).unwrap();
    let printer = args.printer_to(io::BufWriter::new(sink.clone()), Style::Compact);
    let writer = printer.list(rows, &never_cancelled);
    let driver = async {
        // The first element reaches the sink while the producer still holds
        // the last one back.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !sink.text().contains("//p:t0") {
            assert!(
                tokio::time::Instant::now() < deadline,
                "first element never flushed"
            );
            tokio::task::yield_now().await;
        }
        assert!(!sink.text().ends_with("\n]\n"));
        tx.send(Ok(row(1))).unwrap();
        drop(tx);
    };
    let (written, ()) = tokio::join!(writer, driver);
    written.unwrap();
    let text = sink.text();
    assert!(text.ends_with("\n]\n"), "{text}");
    let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(doc.as_array().unwrap().len(), 2);
}

fn tokio_stream_from<T>(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<T>,
) -> impl futures::Stream<Item = T> {
    futures::stream::poll_fn(move |cx| rx.poll_recv(cx))
}

#[test]
fn list_writer_backpressure() {
    // A writer that blocks until the gate opens: the producer, on the same
    // task, cannot run ahead of it.
    struct Gated {
        gate: std::sync::mpsc::Receiver<()>,
        out: Vec<u8>,
    }
    impl Write for Gated {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if !self.out.is_empty() {
                self.gate.recv().map_err(io::Error::other)?;
            }
            self.out.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let (open, gate) = std::sync::mpsc::channel();
    let produced = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&produced);
    let worker = std::thread::spawn(move || {
        let args = args(&["--json"]);
        let rows = futures::stream::iter(0..100).map(move |i| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(row(i))
        });
        let gated = Gated {
            gate,
            out: Vec::new(),
        };
        futures::executor::block_on(
            args.printer_to(gated, Style::Compact)
                .list(rows, &never_cancelled),
        )
    });
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        produced.load(Ordering::SeqCst) <= 2,
        "the producer ran ahead of a stalled writer: {}",
        produced.load(Ordering::SeqCst)
    );
    for _ in 0..1000 {
        if open.send(()).is_err() {
            break;
        }
    }
    drop(open);
    worker.join().unwrap().unwrap();
    assert_eq!(produced.load(Ordering::SeqCst), 100);
}

#[tokio::test(flavor = "current_thread")]
async fn incomplete_list_is_unclosed() {
    // A mid-stream error: no `]`, the error returned with the reason.
    let args = args(&["--json"]);
    let mut out = Vec::new();
    let rows = futures::stream::iter([Ok(row(0)), Err(anyhow::anyhow!("provider failed"))]);
    let err = args
        .printer_to(&mut out, Style::Compact)
        .list(rows, &never_cancelled)
        .await
        .unwrap_err();
    let message = format!("{err:#}");
    assert!(message.contains("provider failed"), "{message}");
    assert!(message.contains("`]` was not written"), "{message}");
    let text = String::from_utf8(out).unwrap();
    assert_eq!(text, "[\n{\"addr\":\"//p:t0\",\"labels\":[\"test\"]}");
    serde_json::from_str::<serde_json::Value>(&text).unwrap_err();

    // A writer dropped without `finish` (keep-going gaps, a cancel): no `]`.
    let mut out = Vec::new();
    {
        let mut list = args
            .printer_to(&mut out, Style::Compact)
            .start_list()
            .unwrap();
        assert_eq!(list.push(&row(0)).unwrap(), Flow::Continue);
    }
    let text = String::from_utf8(out).unwrap();
    assert_eq!(text, "[\n{\"addr\":\"//p:t0\",\"labels\":[\"test\"]}");

    // Under --jq nothing is printed for an incomplete list.
    let args = crate::fixtures::args(&["--jq", ".[]"]);
    let mut out = Vec::new();
    let rows = futures::stream::iter([Ok(row(0)), Err(anyhow::anyhow!("cancelled"))]);
    args.printer_to(&mut out, Style::Compact)
        .list(rows, &never_cancelled)
        .await
        .unwrap_err();
    assert!(out.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn broken_pipe_stops_the_walk() {
    let args = args(&["--json"]);
    let polled = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&polled);
    // An endless walk: only the broken pipe can stop it.
    let rows = futures::stream::iter(0..).map(move |i| {
        counter.fetch_add(1, Ordering::SeqCst);
        Ok(row(i))
    });
    let pipe = Failing {
        ok: 2,
        error: || io::Error::from(io::ErrorKind::BrokenPipe),
        out: Vec::new(),
    };
    args.printer_to(pipe, Style::Compact)
        .list(rows, &never_cancelled)
        .await
        .unwrap();
    assert!(
        polled.load(Ordering::SeqCst) <= 3,
        "{}",
        polled.load(Ordering::SeqCst)
    );

    // A ListWriter driver is told to stop.
    let pipe = Failing {
        ok: 1,
        error: || io::Error::from(io::ErrorKind::BrokenPipe),
        out: Vec::new(),
    };
    let mut list = args.printer_to(pipe, Style::Compact).start_list().unwrap();
    assert_eq!(list.push(&row(0)).unwrap(), Flow::Stop);
    assert_eq!(list.push(&row(1)).unwrap(), Flow::Stop);
    list.finish(&never_cancelled).unwrap();

    // A record on a closed pipe is not an error either.
    let pipe = Failing {
        ok: 0,
        error: || io::Error::from(io::ErrorKind::BrokenPipe),
        out: Vec::new(),
    };
    args.printer_to(pipe, Style::Compact)
        .record(&row(0), &never_cancelled)
        .unwrap();
}

#[test]
fn stdout_write_error_is_reported() {
    let enospc = || io::Error::from_raw_os_error(28);
    let args = args(&["--json"]);
    let full = Failing {
        ok: 0,
        error: enospc,
        out: Vec::new(),
    };
    let err = args
        .printer_to(full, Style::Compact)
        .record(&row(0), &never_cancelled)
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("writing the JSON output to stdout"),
        "{err:#}"
    );
    assert_eq!(
        err.root_cause()
            .downcast_ref::<io::Error>()
            .unwrap()
            .raw_os_error(),
        Some(28)
    );

    let full = Failing {
        ok: 2,
        error: enospc,
        out: Vec::new(),
    };
    let err = args
        .printer_to(full, Style::Compact)
        .list_iter((0..10).map(row), &never_cancelled)
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("writing the JSON output to stdout"),
        "{err:#}"
    );
}
