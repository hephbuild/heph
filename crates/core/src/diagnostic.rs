//! A user-facing diagnostic: the one error type renderers *recognise*.
//!
//! An `anyhow` context chain answers "which code path raised this"; a
//! [`Diagnostic`] answers what the user needs: what is wrong, at which stage,
//! where in their source, and what to do. A producer (the BUILD-file
//! evaluator, a spec parser, a cdylib's error decoded at the seam) raises one;
//! a renderer finds it through the error walker
//! ([`crate::hmemoizer::walk`]) and prints it alone, so none of the engine's
//! boundary frames reach the user.
//!
//! The field set is defined here, once, with addresses as `String`s, because
//! `hcore` has no workspace dependencies: `hplugin` re-exports these types and
//! adds the typed engine-boundary frame (`StageFrame`) next to them.
//!
//! **Never hashed.** None of these types implements `Hash`, so a driver that
//! hashes its spec or its error cannot pick one up by accident: nothing about a
//! diagnostic reaches a def hash, `hashin` or a cache key. The `compile_fail`
//! doctests that hold this are in `hplugin::diagnostic`: this package is named
//! `core`, so a doctest here cannot name it as `hcore` and would fail to
//! compile whatever these types derive.
//!
//! **Serialized** (`serde`) for the failure record ([`crate::failure`]) that
//! rides the event wire and the failure slot's `diagnostic.json`. Every field
//! but `stage` and `message` may be absent, so a sparse record decodes. The
//! decode is forward-compatible, since a slot outlives the heph that wrote
//! it: an unknown stage reads as [`Stage::Run`], an unknown hint kind as
//! [`HintKind::Other`]. A diagnostic's location goes through
//! [`SourceLoc::checked`], and one that names an absolute or climbing path is
//! dropped, never the diagnostic around it.

use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fmt;
use std::fmt::Write as _;

/// The most bytes a message keeps; a longer one is cut at a char boundary and
/// marked with `…`.
pub const MESSAGE_CAP: usize = 2048;
/// The most hints a diagnostic keeps; the rest are counted in a `+N more` hint.
pub const HINTS_CAP: usize = 8;
/// The most bytes one hint's text keeps.
pub const HINT_CAP: usize = 256;
/// The most lines a snippet keeps.
pub const SNIPPET_LINES_CAP: usize = 10;
/// The most chars one snippet line keeps.
pub const SNIPPET_LINE_CAP: usize = 200;
/// The most bytes a location's file keeps.
pub const FILE_CAP: usize = 1024;

/// Which stage of a build raised a failure. Serialized as its
/// [`as_str`](Stage::as_str) name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// A BUILD file (or a file it loads) evaluated.
    Eval,
    /// A provider resolved a target's spec.
    Spec,
    /// A driver parsed a spec into a def.
    Parse,
    /// The engine linked a def's inputs and outputs, or hashed them.
    Link,
    /// A target ran.
    Run,
    /// A cache read or write.
    Cache,
    /// `.hephconfig`.
    Config,
    /// The command line.
    Cli,
}

impl Stage {
    /// The stage's name, as `--json` and the text renderers spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Eval => "eval",
            Self::Spec => "spec",
            Self::Parse => "parse",
            Self::Link => "link",
            Self::Run => "run",
            Self::Cache => "cache",
            Self::Config => "config",
            Self::Cli => "cli",
        }
    }

    /// The stage named `s` by [`Stage::as_str`], or `None`.
    pub fn from_name(s: &str) -> Option<Self> {
        Some(match s {
            "eval" => Self::Eval,
            "spec" => Self::Spec,
            "parse" => Self::Parse,
            "link" => Self::Link,
            "run" => Self::Run,
            "cache" => Self::Cache,
            "config" => Self::Config,
            "cli" => Self::Cli,
            _ => return None,
        })
    }
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where in the user's source a diagnostic points: a workspace-relative file
/// and a 1-based line and column, the column counted in chars, not bytes.
///
/// `file` is never absolute and never climbs out of the workspace (`..`);
/// [`SourceLoc::checked`] refuses one that would, and the plugin seam decodes
/// through it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawSourceLoc")]
pub struct SourceLoc {
    pub file: String,
    pub line: u32,
    pub col: u32,
}

/// A [`SourceLoc`] as decoded, before [`SourceLoc::checked`].
#[derive(Deserialize)]
struct RawSourceLoc {
    file: String,
    line: u32,
    col: u32,
}

impl TryFrom<RawSourceLoc> for SourceLoc {
    type Error = String;

    fn try_from(raw: RawSourceLoc) -> Result<Self, String> {
        Self::checked(raw.file, raw.line, raw.col)
    }
}

/// [`Diagnostic::stage`] decoded leniently: an unknown name is
/// [`Stage::Run`]. Only a value that is not a string fails the decode.
fn lenient_stage<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Stage, D::Error> {
    let name = <std::borrow::Cow<'de, str>>::deserialize(d)?;
    Ok(Stage::from_name(&name).unwrap_or(Stage::Run))
}

/// [`Diagnostic::location`] decoded leniently: a location of the wrong shape,
/// or one [`SourceLoc::checked`] refuses, is `None`.
fn lenient_location<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<SourceLoc>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Loose {
        Loc(RawSourceLoc),
        Other(serde::de::IgnoredAny),
    }
    Ok(match Option::<Loose>::deserialize(d)? {
        Some(Loose::Loc(raw)) => SourceLoc::try_from(raw).ok(),
        Some(Loose::Other(_)) | None => None,
    })
}

impl SourceLoc {
    /// `file`, `line`, `col` as a location, or why not: an empty, absolute or
    /// `..`-climbing file, or one over [`FILE_CAP`].
    pub fn checked(file: String, line: u32, col: u32) -> Result<Self, String> {
        if file.is_empty() {
            return Err("the location's file is empty".to_owned());
        }
        if file.len() > FILE_CAP {
            return Err(format!("the location's file is over {FILE_CAP} bytes"));
        }
        if file.starts_with('/') || file.starts_with('\\') {
            return Err(format!(
                "the location's file {} is absolute, not workspace-relative",
                escape_control(&file)
            ));
        }
        if file.split(['/', '\\']).any(|c| c == "..") {
            return Err(format!(
                "the location's file {} climbs out of the workspace",
                escape_control(&file)
            ));
        }
        if file.chars().any(char::is_control) {
            return Err(format!(
                "the location's file {} holds a control character",
                escape_control(&file)
            ));
        }
        Ok(Self { file, line, col })
    }
}

impl fmt::Display for SourceLoc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.file, self.line, self.col)
    }
}

/// What a [`Hint`] offers. Serialized as its [`as_str`](HintKind::as_str)
/// name; an unknown name (a newer peer's kind) decodes as
/// [`HintKind::Other`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HintKind {
    /// A near-miss name: "did you mean `deps`?".
    DidYouMean,
    /// The values that would have been accepted.
    ValidValues,
    /// A tool the target uses but does not declare.
    DeclareTool,
    /// Where the full log is.
    SeeLog,
    /// Anything else.
    #[serde(other)]
    Other,
}

impl HintKind {
    /// The kind's name, as `--json` spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DidYouMean => "did_you_mean",
            Self::ValidValues => "valid_values",
            Self::DeclareTool => "declare_tool",
            Self::SeeLog => "see_log",
            Self::Other => "other",
        }
    }

    /// The kind named `s` by [`HintKind::as_str`]; an unknown name (a newer
    /// peer's kind) is [`HintKind::Other`].
    pub fn from_name(s: &str) -> Self {
        match s {
            "did_you_mean" => Self::DidYouMean,
            "valid_values" => Self::ValidValues,
            "declare_tool" => Self::DeclareTool,
            "see_log" => Self::SeeLog,
            _ => Self::Other,
        }
    }
}

/// One thing the user can do about a diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hint {
    pub kind: HintKind,
    pub text: String,
}

impl Hint {
    pub fn new(kind: HintKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
        }
    }
}

/// A failure in the user's words: one sentence of what is wrong, where, and
/// what to do. See the module docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// A stage this heph does not know (a newer peer's, or a slot's
    /// `diagnostic.json` from a newer heph) decodes as [`Stage::Run`].
    #[serde(deserialize_with = "lenient_stage")]
    pub stage: Stage,
    /// The target it is about (`//pkg:name`); `None` for a file culprit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addr: Option<String>,
    /// One sentence, in the user's words.
    pub message: String,
    /// A location [`SourceLoc::checked`] refuses, or one that is not a
    /// location at all, decodes as `None`: the rest of the diagnostic is kept.
    #[serde(
        default,
        deserialize_with = "lenient_location",
        skip_serializing_if = "Option::is_none"
    )]
    pub location: Option<SourceLoc>,
    /// Source lines around `location`, as the producer framed them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hints: Vec<Hint>,
    /// Which plugin or driver raised it. For a plugin's diagnostic, the host
    /// sets it (the plugin's own claim is not kept).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
    /// A cap cut something ([`Diagnostic::bounded`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

impl Diagnostic {
    pub fn new(stage: Stage, message: impl Into<String>) -> Self {
        Self {
            stage,
            addr: None,
            message: message.into(),
            location: None,
            snippet: None,
            hints: Vec::new(),
            plugin: None,
            truncated: false,
        }
    }

    pub fn with_addr(mut self, addr: impl Into<String>) -> Self {
        self.addr = Some(addr.into());
        self
    }

    pub fn with_location(mut self, location: SourceLoc) -> Self {
        self.location = Some(location);
        self
    }

    pub fn with_snippet(mut self, snippet: impl Into<String>) -> Self {
        self.snippet = Some(snippet.into());
        self
    }

    pub fn with_hint(mut self, hint: Hint) -> Self {
        self.hints.push(hint);
        self
    }

    pub fn with_plugin(mut self, plugin: impl Into<String>) -> Self {
        self.plugin = Some(plugin.into());
        self
    }

    /// `self` within the caps, with control characters escaped: what the host
    /// keeps of a diagnostic it did not raise itself (a plugin's, decoded at
    /// the seam). Each cut sets [`Diagnostic::truncated`].
    pub fn bounded(mut self) -> Self {
        let mut cut = false;
        self.message = bound_text(&self.message, MESSAGE_CAP, &mut cut);
        if let Some(addr) = &mut self.addr {
            *addr = bound_text(addr, FILE_CAP, &mut cut);
        }
        if let Some(plugin) = &mut self.plugin {
            *plugin = bound_text(plugin, HINT_CAP, &mut cut);
        }
        if self.hints.len() > HINTS_CAP {
            let more = self.hints.len() - (HINTS_CAP - 1);
            self.hints.truncate(HINTS_CAP - 1);
            self.hints
                .push(Hint::new(HintKind::Other, format!("+{more} more")));
            cut = true;
        }
        for hint in &mut self.hints {
            hint.text = bound_text(&hint.text, HINT_CAP, &mut cut);
        }
        if let Some(snippet) = &mut self.snippet {
            *snippet = bound_snippet(snippet, &mut cut);
        }
        self.truncated |= cut;
        self
    }
}

impl fmt::Display for Diagnostic {
    /// `file:line:col: message`, or the message alone without a location: the
    /// text a `{:#}` log line and an older host read.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(loc) = &self.location {
            write!(f, "{loc}: ")?;
        }
        f.write_str(&self.message)
    }
}

impl std::error::Error for Diagnostic {}

/// `s` with control escaped and cut to `cap` bytes at a char boundary, `…`
/// marking the cut (within `cap`); `cut` set when it was.
pub fn bound_text(s: &str, cap: usize, cut: &mut bool) -> String {
    let escaped = escape_control(s);
    if escaped.len() <= cap {
        return escaped.into_owned();
    }
    *cut = true;
    let mut end = cap.saturating_sub('…'.len_utf8());
    while !escaped.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(end + '…'.len_utf8());
    out.push_str(escaped.get(..end).unwrap_or_default());
    out.push('…');
    out
}

/// At most [`SNIPPET_LINES_CAP`] lines of `s`, each of at most
/// [`SNIPPET_LINE_CAP`] chars, with control characters escaped; a cut line
/// ends with `…` and cut lines are followed by a `…` line. `cut` set when
/// anything was cut.
pub fn bound_snippet(s: &str, cut: &mut bool) -> String {
    let mut out = String::new();
    for (i, line) in s.lines().enumerate() {
        if i == SNIPPET_LINES_CAP {
            *cut = true;
            out.push_str("\n…");
            break;
        }
        if i > 0 {
            out.push('\n');
        }
        let line = escape_control(line);
        match line.char_indices().nth(SNIPPET_LINE_CAP) {
            None => out.push_str(&line),
            Some(_) => {
                *cut = true;
                let end = line
                    .char_indices()
                    .nth(SNIPPET_LINE_CAP - 1)
                    .map_or(line.len(), |(at, _)| at);
                out.push_str(line.get(..end).unwrap_or_default());
                out.push('…');
            }
        }
    }
    out
}

/// Whether `c` is a character a terminal could act on, or one that changes
/// how the text around it is drawn without being seen itself: C0 (but tab
/// and newline), DEL, or C1; a bidi embedding, override or isolate
/// (U+202A–U+202E, U+2066–U+2069), which would make the terminal draw the
/// text after it reordered (U+202E shows `exe.txt` for `txt.exe`); a
/// direction mark (U+200E, U+200F, U+061C); the line and paragraph
/// separators (U+2028, U+2029); and the zero-width no-break space (U+FEFF).
///
/// The one set every escaper of untrusted text uses: [`escape_control`]
/// (messages, source excerpts), the log-tail escaper, and session replay.
pub fn is_terminal_control(c: char) -> bool {
    matches!(
        c,
        '\0'..='\x08'
            | '\x0b'..='\x1f'
            | '\x7f'..='\u{9f}'
            | '\u{61c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
            | '\u{feff}'
    )
}

/// `s` with every [`is_terminal_control`] character written as an escape
/// (`\x1b`, `\u{85}`, `\u{202e}`), so text shown on a terminal never moves its
/// cursor, changes its colors or draws reordered. `\n` and `\t` are kept.
/// Borrowed when there is nothing to escape.
pub fn escape_control(s: &str) -> Cow<'_, str> {
    if !s.chars().any(is_terminal_control) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if !is_terminal_control(c) {
            out.push(c);
        } else if c.is_ascii() {
            // Writing to a `String` cannot fail.
            write!(out, "\\x{:02x}", u32::from(c)).unwrap_or_default();
        } else {
            write!(out, "\\u{{{:x}}}", u32::from(c)).unwrap_or_default();
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_control_escapes_csi_and_c1_but_keeps_newlines() {
        assert_eq!(escape_control("a\x1b[2Jb"), "a\\x1b[2Jb");
        assert_eq!(escape_control("a\u{85}b"), "a\\u{85}b");
        assert_eq!(escape_control("a\r\nb\tc\x7f"), "a\\x0d\nb\tc\\x7f");
        assert!(matches!(escape_control("plain é"), Cow::Borrowed(_)));
    }

    /// U36: the format characters that reorder or hide text are escaped in
    /// messages and source excerpts as they are in log tails, and their
    /// neighbours (the zero-width joiners, U+202F, U+2065) are not.
    #[test]
    fn escape_control_escapes_every_format_character() {
        let escaped = [
            '\u{61c}', '\u{200e}', '\u{200f}', '\u{2028}', '\u{2029}', '\u{202a}', '\u{202b}',
            '\u{202c}', '\u{202d}', '\u{202e}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}',
            '\u{feff}',
        ];
        for c in escaped {
            let want = format!("txt\\u{{{:x}}}exe", u32::from(c));
            assert_eq!(escape_control(&format!("txt{c}exe")), want, "{c:?}");
        }
        for c in ['\u{200c}', '\u{200d}', '\u{202f}', '\u{2065}', '\u{61b}'] {
            let s = format!("a{c}b");
            assert!(matches!(escape_control(&s), Cow::Borrowed(_)), "{c:?}");
        }
    }

    #[test]
    fn bounded_caps_each_field_and_marks_the_cut() {
        let d = Diagnostic {
            hints: (0..20)
                .map(|i| Hint::new(HintKind::Other, format!("h{i}")))
                .collect(),
            snippet: Some((0..30).map(|i| format!("line {i}\n")).collect()),
            ..Diagnostic::new(Stage::Run, "é".repeat(MESSAGE_CAP))
        }
        .bounded();
        assert!(d.truncated);
        assert!(d.message.len() <= MESSAGE_CAP, "{}", d.message.len());
        assert!(d.message.ends_with('…'));
        assert_eq!(d.hints.len(), HINTS_CAP);
        assert_eq!(d.hints.last().map(|h| h.text.as_str()), Some("+13 more"));
        assert_eq!(
            d.snippet.as_deref().map(|s| s.lines().count()),
            Some(SNIPPET_LINES_CAP + 1)
        );
    }

    #[test]
    fn bounded_leaves_a_small_diagnostic_alone() {
        let d =
            Diagnostic::new(Stage::Eval, "bad").with_hint(Hint::new(HintKind::DidYouMean, "deps"));
        assert_eq!(d.clone().bounded(), d);
    }

    #[test]
    fn source_loc_refuses_absolute_dotdot_and_control() {
        assert!(SourceLoc::checked("p/BUILD".into(), 1, 1).is_ok());
        assert!(SourceLoc::checked("p/..x/BUILD".into(), 1, 1).is_ok());
        for bad in [
            "/etc/passwd",
            "../x",
            "p/../../x",
            "p\\..\\x",
            "",
            "p/\x1b[2J",
        ] {
            assert!(SourceLoc::checked(bad.into(), 1, 1).is_err(), "{bad:?}");
        }
    }

    /// Every stage's spelling, as the plugin wire and `--json` carry it:
    /// `as_str`, `Display` and `from_name` agree on one name each, and
    /// `from_name` reads back exactly the names `as_str` writes. The `match`
    /// makes a new stage fail to compile here until it has a row.
    #[test]
    fn every_stage_spelling() {
        const ROWS: [(Stage, &str); 8] = [
            (Stage::Eval, "eval"),
            (Stage::Spec, "spec"),
            (Stage::Parse, "parse"),
            (Stage::Link, "link"),
            (Stage::Run, "run"),
            (Stage::Cache, "cache"),
            (Stage::Config, "config"),
            (Stage::Cli, "cli"),
        ];
        for (stage, name) in ROWS {
            match stage {
                Stage::Eval
                | Stage::Spec
                | Stage::Parse
                | Stage::Link
                | Stage::Run
                | Stage::Cache
                | Stage::Config
                | Stage::Cli => {}
            }
            assert_eq!(stage.as_str(), name);
            assert_eq!(stage.to_string(), name);
            assert_eq!(Stage::from_name(name), Some(stage), "{name}");
        }
        for near in ["", "Eval", "EVAL", " eval", "eval ", "evaluate", "runs"] {
            assert_eq!(Stage::from_name(near), None, "{near:?}");
        }
    }

    /// Every hint kind's spelling, as [`every_stage_spelling`] does for
    /// stages; a name no kind writes (a newer peer's) reads as `Other`.
    #[test]
    fn every_hint_kind_spelling() {
        const ROWS: [(HintKind, &str); 5] = [
            (HintKind::DidYouMean, "did_you_mean"),
            (HintKind::ValidValues, "valid_values"),
            (HintKind::DeclareTool, "declare_tool"),
            (HintKind::SeeLog, "see_log"),
            (HintKind::Other, "other"),
        ];
        for (kind, name) in ROWS {
            match kind {
                HintKind::DidYouMean
                | HintKind::ValidValues
                | HintKind::DeclareTool
                | HintKind::SeeLog
                | HintKind::Other => {}
            }
            assert_eq!(kind.as_str(), name);
            assert_eq!(HintKind::from_name(name), kind, "{name}");
        }
        for unknown in [
            "",
            "DidYouMean",
            "did-you-mean",
            "see_logs",
            "from_the_future",
        ] {
            assert_eq!(HintKind::from_name(unknown), HintKind::Other, "{unknown:?}");
        }
    }

    #[test]
    fn display_is_location_then_message() {
        let d = Diagnostic::new(Stage::Eval, "unknown option `dep`")
            .with_location(SourceLoc::checked("p/BUILD".into(), 4, 5).expect("loc"));
        assert_eq!(d.to_string(), "p/BUILD:4:5: unknown option `dep`");
    }
}
