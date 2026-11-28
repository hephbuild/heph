//! "Did you mean" for a config name nothing knows (error-reporting D4): a
//! top-level or nested `.hephconfig` key, a builtin plugin name. The nearest
//! known name comes from [`hcore::suggest`]; with none near enough the
//! error says only what it said before.

use hcore::suggest::{did_you_mean_text, listed, suggest};

/// How many builtin plugin names an unknown-builtin error lists.
const BUILTINS_LISTED: usize = 10;

/// The error for a `builtin:` entry naming no builtin plugin: the nearest
/// names, then the builtin plugins (at most [`BUILTINS_LISTED`], sorted).
/// `known` is every builtin plugin name, in any order.
pub fn unknown_builtin<'a>(name: &str, known: impl IntoIterator<Item = &'a str>) -> anyhow::Error {
    let mut known: Vec<&str> = known.into_iter().collect();
    known.sort_unstable();
    known.dedup();
    let mut msg = format!(
        "unknown builtin plugin `{}`",
        hcore::diagnostic::escape_control(name)
    );
    if let Some(dym) = did_you_mean_text(&suggest(name, known.iter().copied())) {
        msg.push_str("; ");
        msg.push_str(&dym);
    }
    if !known.is_empty() {
        msg.push_str(" (builtin plugins: ");
        msg.push_str(&listed(known.iter().copied(), BUILTINS_LISTED));
        msg.push(')');
    }
    anyhow::anyhow!(msg)
}

/// serde's unknown-field (or unknown-variant) message, `msg`, with the
/// nearest expected name added after the unknown one:
/// "unknown field `plugns`, did you mean `plugins`? (expected one of …) at
/// line 8 column 1". `None` when `msg` is no such message or no expected
/// name is near enough, and the caller keeps serde's error.
///
/// serde writes the expected names as "`a`", "`a` or `b`" or "one of `a`,
/// `b`, `c`" (`serde::de::OneOf`); serde_yaml adds a key path before and a
/// position after, both kept.
pub fn unknown_name_suggestion(msg: &str) -> Option<String> {
    let (before, what, rest) = ["unknown field `", "unknown variant `"]
        .into_iter()
        .find_map(|marker| {
            let (before, rest) = msg.split_once(marker)?;
            Some((before, marker, rest))
        })?;
    let (name, rest) = rest.split_once('`')?;
    let rest = rest.strip_prefix(", expected ")?;
    let list = rest.strip_prefix("one of ").unwrap_or(rest);
    let (expected, tail) = expected_names(list)?;
    let near = suggest(name, expected.iter().copied());
    let dym = did_you_mean_text(&near)?;
    let shown = rest.strip_suffix(tail).unwrap_or(rest);
    Some(format!(
        "{before}{what}{name}`, {dym} (expected {shown}){tail}"
    ))
}

/// The backticked names at the start of `list` ("`a`, `b`", "`a` or `b`"),
/// and the text after the last one. `None` for no name.
fn expected_names(list: &str) -> Option<(Vec<&str>, &str)> {
    let mut names = Vec::new();
    let mut rest = list;
    while let Some(after_tick) = rest.strip_prefix('`') {
        let Some((name, after)) = after_tick.split_once('`') else {
            break;
        };
        names.push(name);
        rest = after;
        match [", ", " or "]
            .into_iter()
            .find_map(|sep| rest.strip_prefix(sep))
            .filter(|next| next.starts_with('`'))
        {
            Some(next) => rest = next,
            None => break,
        }
    }
    (!names.is_empty()).then_some((names, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CLI-04: an unknown builtin plugin names the nearest builtin and
    /// lists them; nothing near, the list alone.
    #[test]
    fn unknown_builtin_plugin_suggests() {
        let known = ["exec", "buildfile", "fs", "group", "http"];
        assert_eq!(
            unknown_builtin("bildfile", known).to_string(),
            "unknown builtin plugin `bildfile`; did you mean `buildfile`? \
             (builtin plugins: buildfile, exec, fs, group, http)"
        );
        assert_eq!(
            unknown_builtin("frobnicate", known).to_string(),
            "unknown builtin plugin `frobnicate` \
             (builtin plugins: buildfile, exec, fs, group, http)"
        );
        let many: Vec<String> = (0..12).map(|i| format!("plugin{i:02}")).collect();
        let msg = unknown_builtin("zzz", many.iter().map(String::as_str)).to_string();
        assert!(msg.ends_with(", … (+2 more))"), "{msg}");
        assert_eq!(
            unknown_builtin("a\x1bb", ["ab"]).to_string(),
            "unknown builtin plugin `a\\x1bb`; did you mean `ab`? (builtin plugins: ab)",
            "a control byte in the name is shown escaped"
        );
    }

    /// CLI-02: a typo'd top-level key through the real loader names the
    /// nearest key and keeps serde's list and position.
    #[test]
    fn unknown_config_key_suggests() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(".hephconfig");
        std::fs::write(&path, "homeDir: .heph\nplugns: []\n").expect("write");
        let err = crate::load(&path).expect_err("unknown key");
        let msg = format!("{err:#}");
        assert!(
            msg.contains(
                "unknown field `plugns`, did you mean `plugins`? (expected one of `version`"
            ),
            "{msg}"
        );
        assert!(msg.ends_with(" at line 2 column 1"), "{msg}");

        // A nested key: the path before and the position after are kept.
        std::fs::write(&path, "memCache:\n  perEntryByte: 1\n").expect("write");
        let msg = format!("{:#}", crate::load(&path).expect_err("unknown key"));
        assert!(
            msg.contains("memCache: unknown field `perEntryByte`, did you mean `perEntryBytes`?"),
            "{msg}"
        );

        // Nothing near: serde's message unchanged.
        std::fs::write(&path, "zzzzzz: 1\n").expect("write");
        let msg = format!("{:#}", crate::load(&path).expect_err("unknown key"));
        assert!(
            msg.contains("unknown field `zzzzzz`, expected one of `version`"),
            "{msg}"
        );
        assert!(!msg.contains("did you mean"), "{msg}");
    }

    #[test]
    fn unknown_name_suggestion_reads_each_serde_shape() {
        for (msg, want) in [
            (
                "unknown field `buidl`, expected `build`",
                Some("unknown field `buidl`, did you mean `build`? (expected `build`)"),
            ),
            (
                "unknown field `bulid`, expected `build` or `test`",
                Some("unknown field `bulid`, did you mean `build`? (expected `build` or `test`)"),
            ),
            (
                "x: unknown variant `bulid`, expected one of `build`, `test`, `run` at line 1 column 4",
                Some(
                    "x: unknown variant `bulid`, did you mean `build`? \
                     (expected one of `build`, `test`, `run`) at line 1 column 4",
                ),
            ),
            ("unknown field `zzzz`, expected `build`", None),
            ("unknown field `a`, there are no fields", None),
            ("invalid type: integer `5`, expected a map", None),
            ("unknown field `unterminated", None),
            ("", None),
        ] {
            assert_eq!(unknown_name_suggestion(msg).as_deref(), want, "{msg}");
        }
    }
}
