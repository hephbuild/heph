//! Output buffer with lazy indentation and line-length tracking.
//!
//! Mirrors the Go reference's `Builder` + `indentWriter`: indentation is applied
//! lazily at the first non-newline character of each line, and `write_raw`
//! emits multi-line string literals without re-indenting their continuation
//! lines (so triple-quoted strings keep their original interior layout).

pub(crate) struct Printer {
    out: String,
    indent_unit: String,
    level: usize,
    need_indent: bool,
    line_len: usize,
}

impl Printer {
    pub(crate) fn new(indent_unit: String) -> Self {
        Printer {
            out: String::new(),
            indent_unit,
            level: 0,
            need_indent: true,
            line_len: 0,
        }
    }

    pub(crate) fn into_string(self) -> String {
        self.out
    }

    pub(crate) fn level(&self) -> usize {
        self.level
    }

    pub(crate) fn push(&mut self) {
        self.level += 1;
    }

    pub(crate) fn pop(&mut self) {
        self.level -= 1;
    }

    /// Current column on the line being written (characters since last `\n`).
    pub(crate) fn line_length(&self) -> usize {
        self.line_len
    }

    pub(crate) fn ends_with_newline(&self) -> bool {
        self.out.is_empty() || self.out.ends_with('\n')
    }

    fn emit_indent(&mut self) {
        for _ in 0..self.level {
            self.out.push_str(&self.indent_unit);
            self.line_len += self.indent_unit.chars().count();
        }
        self.need_indent = false;
    }

    pub(crate) fn write(&mut self, s: &str) {
        for ch in s.chars() {
            if self.need_indent && ch != '\n' {
                self.emit_indent();
            }
            self.out.push(ch);
            if ch == '\n' {
                self.need_indent = true;
                self.line_len = 0;
            } else {
                self.line_len += 1;
            }
        }
    }

    /// Write a (possibly multi-line) literal. The first line is indented like
    /// any other text, but continuation lines are emitted verbatim at column 0.
    pub(crate) fn write_raw(&mut self, s: &str) {
        match s.split_once('\n') {
            None => self.write(s),
            Some((first, rest)) => {
                self.write(first);
                self.write("\n");
                // Emit the remainder verbatim, bypassing indentation.
                self.need_indent = false;
                self.out.push_str(rest);
                match rest.rfind('\n').and_then(|idx| rest.get(idx + 1..)) {
                    Some(tail) => self.line_len = tail.chars().count(),
                    None => self.line_len += rest.chars().count(),
                }
            }
        }
    }
}
