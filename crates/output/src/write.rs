//! The writers: one record, or a list written as it is found.
//!
//! Writes are synchronous, into a `BufWriter` over stdout, so a slow reader
//! slows the producer instead of growing a buffer (rule 2). `BufferedStdout`
//! is not used: it drops write errors. Under `--jq` the document is collected
//! instead, compact, and handed to the filter at the end.

use crate::filter::Filtered;
use crate::jq::Jq;
use crate::{Cancelled, Record, Selection};
use anyhow::Context as _;
use futures::{FutureExt as _, Stream, StreamExt as _};
use serde::Serialize;
use std::io::{self, Write};
use std::marker::PhantomData;

/// How the document is laid out. Only whitespace differs (I5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// For a terminal: indented.
    Pretty,
    /// For a pipe: a record on one line; a list one element per line between
    /// `[` and `]`.
    Compact,
}

impl Style {
    /// [`Style::Pretty`] when stdout is a terminal, else [`Style::Compact`].
    pub fn for_stdout() -> Self {
        use std::io::IsTerminal as _;
        if io::stdout().is_terminal() {
            Style::Pretty
        } else {
            Style::Compact
        }
    }
}

/// Whether a list writer should keep going.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// Keep producing.
    Continue,
    /// stdout was closed (`| head -1`): stop the walk; the command exits 0.
    Stop,
}

/// `Ok(true)` for a broken pipe, `Ok(false)` for success, an error with
/// context for any other write failure (ENOSPC is never swallowed).
pub(crate) fn closed(result: io::Result<()>) -> anyhow::Result<bool> {
    match result {
        Ok(()) => Ok(false),
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(true),
        Err(e) => Err(e).context("writing the JSON output to stdout"),
    }
}

/// Prints one JSON document for a command. Made by [`crate::JsonArgs::printer`].
pub struct Printer<'a, R: Record, W: Write> {
    w: W,
    style: Style,
    selection: Selection,
    jq: Option<&'a Jq>,
    _record: PhantomData<fn() -> R>,
}

impl<'a, R: Record, W: Write> Printer<'a, R, W> {
    pub(crate) fn new(w: W, style: Style, selection: Selection, jq: Option<&'a Jq>) -> Self {
        Self {
            w,
            style,
            selection,
            jq,
            _record: PhantomData,
        }
    }

    /// Print `value` as the whole document.
    pub fn record<T>(self, value: &T, cancelled: Cancelled<'_>) -> anyhow::Result<()>
    where
        T: Record<Static = R::Static> + Serialize,
    {
        let style = if self.jq.is_some() {
            Style::Compact
        } else {
            self.style
        };
        let mut buf = Vec::new();
        serialize(&mut buf, value, self.selection, style)?;
        self.document(buf, cancelled)
    }

    /// Print `null`: a record-shaped command with nothing to report.
    pub fn null(self, cancelled: Cancelled<'_>) -> anyhow::Result<()> {
        self.document(b"null".to_vec(), cancelled)
    }

    /// A complete document in `buf`: print it, or run the filter over it.
    fn document(mut self, buf: Vec<u8>, cancelled: Cancelled<'_>) -> anyhow::Result<()> {
        write_document(&mut self.w, buf, self.jq, cancelled)
    }

    /// Start a list: `[` is written now, each [`ListWriter::push`] adds an
    /// element, and [`ListWriter::finish`] closes it.
    pub fn start_list(self) -> anyhow::Result<ListWriter<'a, R, W>> {
        let mut list = ListWriter {
            printer: self,
            doc: Vec::new(),
            scratch: Vec::new(),
            len: 0,
            stopped: false,
        };
        // A closed pipe here makes every push answer `Flow::Stop`.
        list.write(b"[")?;
        Ok(list)
    }

    /// Print `rows` as a list, each one as it arrives: the first element is
    /// on stdout before the last is produced. An `Err` row stops the list
    /// without its `]` and is returned; a broken pipe stops polling `rows`
    /// and returns `Ok`.
    pub async fn list<T, S>(self, rows: S, cancelled: Cancelled<'_>) -> anyhow::Result<()>
    where
        T: Record<Static = R::Static> + Serialize,
        S: Stream<Item = anyhow::Result<T>>,
    {
        let mut list = self.start_list()?;
        if list.push_stream(rows).await? == Flow::Stop {
            return Ok(());
        }
        list.finish(cancelled)
    }

    /// [`Printer::list`] over rows that are already computed.
    pub fn list_iter<T, I>(self, rows: I, cancelled: Cancelled<'_>) -> anyhow::Result<()>
    where
        T: Record<Static = R::Static> + Serialize,
        I: IntoIterator<Item = T>,
    {
        let mut list = self.start_list()?;
        if list.push_iter(rows)? == Flow::Stop {
            return Ok(());
        }
        list.finish(cancelled)
    }
}

/// A list being written. Dropping it without [`ListWriter::finish`] leaves
/// the list without its `]` (D10): what was pushed reaches stdout (the
/// `BufWriter` flushes on drop), and a `jq` reading it fails.
pub struct ListWriter<'a, R: Record, W: Write> {
    printer: Printer<'a, R, W>,
    /// Under `--jq`, the whole document, compact.
    doc: Vec<u8>,
    /// One element's bytes, reused.
    scratch: Vec<u8>,
    len: usize,
    stopped: bool,
}

impl<R: Record, W: Write> ListWriter<'_, R, W> {
    /// Add one element. [`Flow::Stop`] once stdout is closed: the caller
    /// stops its walk, and every later push is a no-op.
    #[must_use = "a closed stdout means the caller should stop its walk"]
    pub fn push<T>(&mut self, row: &T) -> anyhow::Result<Flow>
    where
        T: Record<Static = R::Static> + Serialize,
    {
        if self.stopped {
            return Ok(Flow::Stop);
        }
        let style = self.element_style();
        self.scratch.clear();
        let sep: &[u8] = match (self.len, style) {
            (_, None) if self.len > 0 => b",",
            (_, None) => b"",
            (0, Some(_)) => b"\n",
            (_, Some(_)) => b",\n",
        };
        self.scratch.extend_from_slice(sep);
        match style {
            Some(Style::Pretty) => {
                // Indent the element one level: serde_json never writes a raw
                // newline inside a string, so every `\n` is layout.
                self.scratch.extend_from_slice(b"  ");
                let mut element = Vec::new();
                serialize(&mut element, row, self.printer.selection, Style::Pretty)?;
                for &b in &element {
                    self.scratch.push(b);
                    if b == b'\n' {
                        self.scratch.extend_from_slice(b"  ");
                    }
                }
            }
            Some(Style::Compact) | None => {
                serialize(
                    &mut self.scratch,
                    row,
                    self.printer.selection,
                    Style::Compact,
                )?;
            }
        }
        self.len += 1;
        let bytes = std::mem::take(&mut self.scratch);
        let flow = self.write(&bytes);
        self.scratch = bytes;
        flow
    }

    /// [`ListWriter::push`] each of `rows`, stopping at a closed stdout.
    pub fn push_iter<T, I>(&mut self, rows: I) -> anyhow::Result<Flow>
    where
        T: Record<Static = R::Static> + Serialize,
        I: IntoIterator<Item = T>,
    {
        for row in rows {
            if self.push(&row)? == Flow::Stop {
                return Ok(Flow::Stop);
            }
        }
        Ok(Flow::Continue)
    }

    /// [`ListWriter::push`] each row of `rows` as it arrives, flushing
    /// whenever the producer has nothing ready, so a reader sees each element
    /// as soon as the walk pauses. An `Err` row is returned (the list stays
    /// unclosed); a closed stdout stops polling `rows` and is [`Flow::Stop`].
    pub async fn push_stream<T, S>(&mut self, rows: S) -> anyhow::Result<Flow>
    where
        T: Record<Static = R::Static> + Serialize,
        S: Stream<Item = anyhow::Result<T>>,
    {
        let mut rows = std::pin::pin!(rows);
        loop {
            // Flush only when the producer has nothing ready: a reader sees
            // each element as soon as the walk pauses, without a write per row.
            let next = match rows.next().now_or_never() {
                Some(next) => next,
                None => {
                    if self.flush()? == Flow::Stop {
                        return Ok(Flow::Stop);
                    }
                    rows.next().await
                }
            };
            let Some(row) = next else {
                return Ok(Flow::Continue);
            };
            let row = row.context("the list stopped before its end; its `]` was not written")?;
            if self.push(&row)? == Flow::Stop {
                return Ok(Flow::Stop);
            }
        }
    }

    /// Write `]` and end the document; under `--jq`, run the filter over it.
    pub fn finish(mut self, cancelled: Cancelled<'_>) -> anyhow::Result<()> {
        if self.stopped {
            return Ok(());
        }
        if let Some(jq) = self.printer.jq {
            self.doc.push(b']');
            let doc = std::mem::take(&mut self.doc);
            return jq.run(doc, &mut self.printer.w, cancelled);
        }
        let close: &[u8] = if self.len == 0 { b"]\n" } else { b"\n]\n" };
        if self.write(close)? == Flow::Continue {
            // A pipe closed after the last byte changes nothing: exit 0.
            self.flush()?;
        }
        Ok(())
    }

    /// Let what was written so far out. A no-op under `--jq`.
    pub fn flush(&mut self) -> anyhow::Result<Flow> {
        if self.stopped {
            return Ok(Flow::Stop);
        }
        if self.printer.jq.is_some() {
            return Ok(Flow::Continue);
        }
        let result = self.printer.w.flush();
        self.flow(result)
    }

    /// The layout of an element on stdout; `None` under `--jq`, where the
    /// document is collected compact.
    fn element_style(&self) -> Option<Style> {
        self.printer.jq.is_none().then_some(self.printer.style)
    }

    fn write(&mut self, bytes: &[u8]) -> anyhow::Result<Flow> {
        if self.printer.jq.is_some() {
            self.doc.extend_from_slice(bytes);
            return Ok(Flow::Continue);
        }
        let result = self.printer.w.write_all(bytes);
        self.flow(result)
    }

    fn flow(&mut self, result: io::Result<()>) -> anyhow::Result<Flow> {
        if closed(result)? {
            self.stopped = true;
            return Ok(Flow::Stop);
        }
        Ok(Flow::Continue)
    }
}

/// A complete document in `buf` on `w`: printed with its newline, or run
/// through `jq`. A broken pipe is `Ok`.
fn write_document(
    w: &mut impl Write,
    mut buf: Vec<u8>,
    jq: Option<&Jq>,
    cancelled: Cancelled<'_>,
) -> anyhow::Result<()> {
    if let Some(jq) = jq {
        return jq.run(buf, w, cancelled);
    }
    buf.push(b'\n');
    if !closed(w.write_all(&buf))? {
        closed(w.flush())?;
    }
    Ok(())
}

/// Any serializable `value` as the whole document on `w`, with no field
/// selection: the printer of [`crate::DocArgs`]. Compact under `--jq`.
pub(crate) fn value_document<T: Serialize + ?Sized>(
    w: &mut impl Write,
    value: &T,
    style: Style,
    jq: Option<&Jq>,
    cancelled: Cancelled<'_>,
) -> anyhow::Result<()> {
    let mut buf = Vec::new();
    let result = match (style, jq) {
        (Style::Pretty, None) => value.serialize(&mut serde_json::Serializer::pretty(&mut buf)),
        _ => value.serialize(&mut serde_json::Serializer::new(&mut buf)),
    };
    result.context("serializing the JSON output")?;
    write_document(w, buf, jq, cancelled)
}

/// `value` with only `selection`, appended to `buf` in `style`. Into a buffer
/// first, so a serialization error never leaves half an element on stdout.
fn serialize<T: Record + Serialize>(
    buf: &mut Vec<u8>,
    value: &T,
    selection: Selection,
    style: Style,
) -> anyhow::Result<()> {
    let result = match style {
        Style::Pretty => value.serialize(Filtered {
            inner: &mut serde_json::Serializer::pretty(&mut *buf),
            selection,
            fields: T::FIELDS,
        }),
        Style::Compact => value.serialize(Filtered {
            inner: &mut serde_json::Serializer::new(&mut *buf),
            selection,
            fields: T::FIELDS,
        }),
    };
    result.context("serializing the JSON output")
}

#[cfg(test)]
mod tests;
