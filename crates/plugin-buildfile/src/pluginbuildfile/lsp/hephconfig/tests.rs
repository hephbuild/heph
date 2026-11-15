use super::*;

const ENC: Encoding = Encoding::Utf8;

/// `text` with `|` marking the cursor: the text without it, and the offset.
fn at(marked: &str) -> (String, usize) {
    let offset = marked.find('|').expect("cursor mark");
    (marked.replacen('|', "", 1), offset)
}

fn items(marked: &str) -> Vec<CompletionItem> {
    let (text, offset) = at(marked);
    completion(&text, offset, ENC)
}

fn labels(marked: &str) -> Vec<String> {
    items(marked).into_iter().map(|i| i.label).collect()
}

fn edit(item: &CompletionItem) -> &TextEdit {
    match &item.text_edit {
        Some(CompletionTextEdit::Edit(e)) => e,
        other => panic!("no plain edit: {other:?}"),
    }
}

fn find<'a>(items: &'a [CompletionItem], label: &str) -> &'a CompletionItem {
    items
        .iter()
        .find(|i| i.label == label)
        .unwrap_or_else(|| panic!("no `{label}` in {items:?}"))
}

fn key(k: &str) -> Seg {
    Seg::Key(k.to_string())
}

fn cursor_path(marked: &str) -> Option<(Vec<Seg>, String, bool)> {
    let (text, offset) = at(marked);
    match cursor(&text, offset)? {
        Cursor::Key {
            parent_path,
            prefix,
            ..
        } => Some((parent_path, prefix, true)),
        Cursor::Value { path, prefix, .. } => Some((path, prefix, false)),
    }
}

/// Where a cursor lands: the path, the prefix, and whether it is a key.
type Landing<'a> = Option<(Vec<Seg>, &'a str, bool)>;

/// The adversarial table: each row is a cursor, and where it must land.
#[test]
fn cursor_classification() {
    let rows: Vec<(&str, Landing<'_>)> = vec![
        ("|", Some((vec![], "", true))),
        ("ca|", Some((vec![], "ca", true))),
        ("cache:\n  sp|", Some((vec![key("cache")], "sp", true))),
        ("cache:\n|", Some((vec![], "", true))),
        ("cache:\n  |", Some((vec![key("cache")], "", true))),
        // A sibling at the parent's column closes it.
        ("cache:\n  a: 1\nlo|", Some((vec![], "lo", true))),
        (
            "lock:\n  backend: f|",
            Some((vec![key("lock"), key("backend")], "f", false)),
        ),
        (
            "lock:\n  backend: |",
            Some((vec![key("lock"), key("backend")], "", false)),
        ),
        (
            "lock:\n  backend: \"m|\"",
            Some((vec![key("lock"), key("backend")], "m", false)),
        ),
        // A sequence at the key's own column, and indented.
        (
            "plugins:\n- bu|",
            Some((vec![key("plugins"), Seg::Item], "bu", true)),
        ),
        (
            "plugins:\n  - bu|",
            Some((vec![key("plugins"), Seg::Item], "bu", true)),
        ),
        // Right after the dash, before any blank.
        (
            "plugins:\n  -|",
            Some((vec![key("plugins"), Seg::Item], "", true)),
        ),
        (
            "plugins:\n  - builtin: x\n    op|",
            Some((vec![key("plugins"), Seg::Item], "op", true)),
        ),
        // The second item is its own mapping.
        (
            "plugins:\n  - builtin: x\n  - |",
            Some((vec![key("plugins"), Seg::Item], "", true)),
        ),
        // Comments and blank lines change nothing.
        (
            "cache:\n  # note\n\n  sp|",
            Some((vec![key("cache")], "sp", true)),
        ),
        // A block scalar's text is not structure (`>` here: `|` is the
        // cursor mark).
        ("version: >\n  ca|", None),
        ("version: >-\n  text\n\n  ca|", None),
        ("version: >\n  text\nca|", Some((vec![], "ca", true))),
        // A blank line after one is text only deeper than its key.
        ("version: >\n  text\n|", Some((vec![], "", true))),
        ("version: >\n  text\n  |", None),
        // Flow collections and comments are not looked inside.
        ("fuse: {ena|", None),
        ("# ca|", None),
        // Nor are the lines a flow collection or a quoted scalar runs onto.
        ("lock: {\n  back|", None),
        ("lock: [a,\n  b]\nca|", Some((vec![], "ca", true))),
        ("scratch:\n  scope: \"a\n  b|", None),
        ("scratch:\n  scope: 'a\n  b|", None),
        (
            "scratch:\n  scope: it's\n  re|",
            Some((vec![key("scratch")], "re", true)),
        ),
        // A key the scanner cannot read hides what is beneath it.
        ("!!str cache:\n  sp|", Some((vec![Seg::Opaque], "sp", true))),
        // Tab indentation is the parser's error, not a position.
        ("lock:\n\tba|", None),
        // A quoted key.
        ("\"cache\":\n  sp|", Some((vec![key("cache")], "sp", true))),
        // CRLF line ends, and a byte-order mark.
        ("cache:\r\n  sp|", Some((vec![key("cache")], "sp", true))),
        (
            "\u{feff}cache:\n  sp|",
            Some((vec![key("cache")], "sp", true)),
        ),
        ("\u{feff}ca|", Some((vec![], "ca", true))),
        // A value containing `:` is not a second key, nor a value the
        // schema could complete.
        ("caches:\n  main:\n    uri: s3://b|", None),
        (
            "caches:\n  main:\n    uri: s3|",
            Some((vec![key("caches"), key("main"), key("uri")], "s3", false)),
        ),
        // `key:` with no blank after it is not yet a value position.
        ("lock:\n  backend:|", None),
        // Past a scalar list item.
        ("fs:\n  skip:\n    - a b|", None),
        // Multibyte keys and prefixes.
        ("é|", Some((vec![], "é", true))),
        (
            "caches:\n  é🎉:\n    re|",
            Some((vec![key("caches"), key("é🎉")], "re", true)),
        ),
        (
            "lock:\n  backend: é|",
            Some((vec![key("lock"), key("backend")], "é", false)),
        ),
    ];
    for (marked, want) in rows {
        let got = cursor_path(marked);
        let want = want.map(|(p, s, k)| (p, s.to_string(), k));
        assert_eq!(got, want, "{marked:?}");
    }
}

#[test]
fn completes_top_level_keys_minus_present() {
    let got = labels("version: v1\n|");
    assert!(got.contains(&"cache".to_string()), "{got:?}");
    assert!(got.contains(&"plugins".to_string()), "{got:?}");
    assert!(!got.contains(&"version".to_string()), "{got:?}");
    // After a block scalar, at the scalar key's own column.
    let got = labels("version: >\n  text\n|");
    assert!(got.contains(&"cache".to_string()), "{got:?}");
}

#[test]
fn completes_nested_keys_and_inserts_a_colon() {
    let got = items("cache:\n  sp|");
    assert_eq!(
        got.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(),
        ["spillThresholdBytes"]
    );
    assert_eq!(edit(&got[0]).new_text, "spillThresholdBytes: ");
    // A key whose value nests takes the colon and no blank.
    let got = items("ca|");
    assert_eq!(edit(find(&got, "cache")).new_text, "cache:");
    assert_eq!(edit(find(&got, "caches")).new_text, "caches:");
}

/// With `key:` already on the line, completing replaces the whole name only.
#[test]
fn completing_a_written_key_replaces_its_name() {
    let (text, offset) = at("ca|che: 1\n");
    let got = completion(&text, offset, ENC);
    let e = edit(find(&got, "cache"));
    assert_eq!(e.new_text, "cache");
    assert_eq!(
        e.range,
        Range::new(
            lsp_types::Position::new(0, 0),
            lsp_types::Position::new(0, 5)
        )
    );
}

#[test]
fn a_key_right_after_a_dash_gets_a_blank() {
    let got = items("plugins:\n  -|");
    assert_eq!(edit(find(&got, "builtin")).new_text, " builtin: ");
    let got = items("plugins:\n  - |");
    assert_eq!(edit(find(&got, "builtin")).new_text, "builtin: ");
}

#[test]
fn item_keys_are_per_item() {
    let got = labels("plugins:\n  - builtin: a\n  - |");
    assert!(got.contains(&"builtin".to_string()), "{got:?}");
    let got = labels("plugins:\n  - builtin: a\n    |");
    assert!(!got.contains(&"builtin".to_string()), "{got:?}");
    assert!(got.contains(&"options".to_string()), "{got:?}");
}

/// A plugin entry sets one source: once it has one, the others are gone.
#[test]
fn a_plugin_entry_offers_one_source() {
    let got = labels("plugins:\n  - path: a\n    |");
    for gone in ["builtin", "path", "url"] {
        assert!(!got.contains(&gone.to_string()), "{gone}: {got:?}");
    }
    assert!(got.contains(&"options".to_string()), "{got:?}");
    let got = labels("plugins:\n  - options: {}\n    |");
    for offered in ["builtin", "path", "url"] {
        assert!(got.contains(&offered.to_string()), "{offered}: {got:?}");
    }
}

#[test]
fn completes_enum_and_boolean_values() {
    assert_eq!(labels("lock:\n  backend: |"), ["fs", "mem"]);
    assert_eq!(labels("lock:\n  backend: m|"), ["mem"]);
    assert_eq!(labels("telemetry:\n  enabled: |"), ["true", "false"]);
    let fuse = labels("fuse:\n  enabled: |");
    assert!(fuse.contains(&"auto".to_string()), "{fuse:?}");
    assert!(fuse.contains(&"true".to_string()), "{fuse:?}");
    assert_eq!(
        labels("resources:\n  gpu:\n    unit: |"),
        ["count", "cpu", "bytes"]
    );
    // `Option<bool>` is `"type": ["boolean", "null"]`.
    assert_eq!(labels("caches:\n  main:\n    read: |"), ["true", "false"]);
    assert_eq!(labels("scratch:\n  seedOnFork: |"), ["true", "false"]);
    // The prefix filter is case-sensitive.
    assert!(labels("lock:\n  backend: F|").is_empty());
}

#[test]
fn map_values_complete_their_entry_schema() {
    let got = labels("caches:\n  main:\n    |");
    assert!(got.contains(&"uri".to_string()), "{got:?}");
    assert!(got.contains(&"region".to_string()), "{got:?}");
    let got = labels("resources:\n  |");
    assert!(got.contains(&"cpu".to_string()), "{got:?}");
    assert!(got.contains(&"memory".to_string()), "{got:?}");
}

fn hover_md(marked: &str) -> Option<String> {
    let (text, offset) = at(marked);
    match hover(&text, offset, ENC)?.contents {
        HoverContents::Markup(m) => Some(m.value),
        other => panic!("not markup: {other:?}"),
    }
}

#[test]
fn hover_shows_the_field_doc() {
    let md = hover_md("cache:\n  spill|ThresholdBytes: 10\n").expect("hover");
    assert!(
        md.starts_with("`cache.spillThresholdBytes`: integer"),
        "{md}"
    );
    assert!(md.contains("spill to plain files"), "{md}");
    let md = hover_md("lock:\n  back|end: fs\n").expect("hover");
    assert!(md.contains("One of `fs`, `mem`."), "{md}");
    let md = hover_md("plugins:\n  - buil|tin: exec\n").expect("hover");
    assert!(md.starts_with("`plugins[].builtin`: string"), "{md}");
    assert!(md.contains("compiled-in plugin"), "{md}");
    let md = hover_md("scratch:\n  seed|OnFork: false\n").expect("hover");
    assert!(md.starts_with("`scratch.seedOnFork`: boolean"), "{md}");
}

#[test]
fn hover_is_none_off_a_schema_key() {
    assert_eq!(hover_md("# ca|che: 1\n"), None);
    assert_eq!(hover_md("cache:\n  # sp|ill\n"), None);
    // A plugin's options are the plugin's own.
    assert_eq!(
        hover_md("plugins:\n  - builtin: go\n    options:\n      got|ool: host\n"),
        None
    );
}

#[test]
fn hover_cleans_intra_doc_links() {
    assert_eq!(clean_doc("See [`ScratchConfig`]."), "See `ScratchConfig`.");
    assert_eq!(clean_doc("a [`x`](Self::x) b"), "a `x` b");
    assert_eq!(clean_doc("a [text](crate::X) b"), "a text b");
    assert_eq!(
        clean_doc("[docs](https://example.com)"),
        "[docs](https://example.com)"
    );
    assert_eq!(clean_doc("unclosed [a"), "unclosed [a");
    // Inline code and prose brackets are not links.
    assert_eq!(clean_doc("e.g. `[\"master\"]`."), "e.g. `[\"master\"]`.");
    assert_eq!(clean_doc("`[`X`]` stays"), "`[`X`]` stays");
    assert_eq!(clean_doc("a[0] and [x] stay"), "a[0] and [x] stay");
    assert_eq!(clean_doc("[`a`, `b`] stays"), "[`a`, `b`] stays");
    assert_eq!(clean_doc("unclosed `code [`X`]"), "unclosed `code [`X`]");
}

/// What `--explain` prints for a config document.
#[test]
fn explain_classifies_the_cursor() {
    let (text, offset) = at("plugins:\n  - builtin: e|\n");
    assert_eq!(
        explain(&text, Some(offset)),
        serde_json::json!({
            "document": "hephconfig", "kind": "value", "path": "plugins[].builtin",
            "in_schema": true, "prefix": "e",
        })
    );
    let (text, offset) = at("plugins:\n  - bu|\n");
    assert_eq!(explain(&text, Some(offset))["path"], "plugins[]");
    assert_eq!(explain(&text, Some(offset))["kind"], "key");
    // Beneath a line the scanner cannot read: a key position the schema
    // has nothing for.
    let (text, offset) = at("!!str cache:\n  sp|\n");
    assert_eq!(explain(&text, Some(offset))["path"], "?");
    assert_eq!(explain(&text, Some(offset))["in_schema"], false);
    let none = serde_json::json!({
        "document": "hephconfig", "kind": "none", "path": null,
        "in_schema": false, "prefix": null,
    });
    let (text, offset) = at("# |\n");
    assert_eq!(explain(&text, Some(offset)), none);
    assert_eq!(explain(&text, None), none);
}

fn messages(text: &str) -> Vec<String> {
    diagnostics(text, ENC)
        .into_iter()
        .map(|d| {
            format!(
                "{}:{} {}",
                d.range.start.line, d.range.start.character, d.message
            )
        })
        .collect()
}

/// Every config file in the repository, by path and text.
fn repo_configs() -> Vec<(std::path::PathBuf, String)> {
    fn walk(dir: &std::path::Path, out: &mut Vec<(std::path::PathBuf, String)>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .expect("read_dir")
            .map(|e| e.expect("entry").path())
            .collect();
        entries.sort();
        for path in entries {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if path.is_dir() {
                if !name.starts_with(".heph") && name != "target" {
                    walk(&path, out);
                }
            } else if hconfig::is_config_file_name(name) {
                let text = std::fs::read_to_string(&path).expect("read config");
                out.push((path, text));
            }
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&root).expect("read root") {
        let path = entry.expect("entry").path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if path.is_file() && hconfig::is_config_file_name(name) {
            let text = std::fs::read_to_string(&path).expect("read config");
            out.push((path, text));
        }
    }
    walk(&root.join("example"), &mut out);
    out
}

/// Text the loader accepts gets no diagnostic: every shape the scanner
/// cannot read is left to the parser, which has nothing to say.
#[test]
fn what_loads_has_no_diagnostics() {
    let corpus = [
        "",
        "\n",
        "# c\n",
        "---\n",
        "   \n  \n",
        "version: v1.2.3\n",
        "plugins:\n- builtin: exec\n  options:\n    anything: 1\n",
        "plugins:\n  -\n    builtin: x\n",
        "caches:\n  main:\n    uri: s3://b/p\n    read: true\n",
        // A profile patching one field.
        "caches:\n  main:\n    read: false\n",
        "resources:\n  cpu: 50%\n  gpu:\n    unit: count\n    value: 2\n",
        "scratch:\n  restoreScopes:\n    - master\n",
        "scratch:\n  scope: it's # a comment\n  seedOnFork: false\n",
        // Block-scalar text that looks like a key is text.
        "version: |\n  notakey: x\n",
        "# only a comment\nversion: v1 # trailing\n",
        // Flow collections and quoted scalars over several lines.
        "lock: {\nbackend: fs}\n",
        "plugins:\n- builtin: exec\n  options: {a: 1,\n  b: 2}\n",
        "plugins:\n- {builtin: exec,\noptions: {a: 1}}\n",
        "scratch:\n  scope: \"a\n  b: c\"\n",
        "scratch:\n  scope: 'a\n  b: c'\n",
        "scratch:\n  scope: \"a \\\" {\n  b: c\"\n",
        // Keys behind a tag, an anchor or `?`.
        "fuse:\n  enabled: auto\n!!str cache:\n  spillThresholdBytes: 1\n",
        "&k cache:\n  spillThresholdBytes: 1\n",
        "? cache\n: {spillThresholdBytes: 1}\n",
        "\u{feff}lock:\n  backend: fs\n",
    ];
    for text in corpus {
        assert!(
            serde_yaml::from_str::<hconfig::ConfigYaml>(text).is_ok() || text.is_empty(),
            "corpus entry does not load: {text:?}"
        );
        assert_eq!(messages(text), Vec::<String>::new(), "{text:?}");
    }
    let files = repo_configs();
    assert!(!files.is_empty(), "no config file found in the repository");
    for (path, text) in files {
        if serde_yaml::from_str::<hconfig::ConfigYaml>(&text).is_ok() {
            assert_eq!(messages(&text), Vec::<String>::new(), "{}", path.display());
        }
    }
}

#[test]
fn every_unknown_key_is_flagged_with_a_suggestion() {
    let got = messages("cahce:\n  x: 1\nlock:\n  backnd: fs\n");
    assert_eq!(
        got,
        [
            "0:0 unknown key `cahce` at the top level: did you mean `cache`?",
            "3:2 unknown key `backnd` in `lock`: did you mean `backend`?",
        ]
    );
}

/// The parser's error is dropped only when it is an unknown key already
/// flagged: one complaint per key, whichever line the parser puts it on.
#[test]
fn one_diagnostic_per_problem() {
    // The plugin entry's and a pool's own errors sit on their mapping, not
    // on the key.
    let got = messages("plugins:\n  - builtin: x\n    bogus: 1\n");
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(
        got[0].starts_with("2:4 unknown key `bogus` in `plugins[]`"),
        "{got:?}"
    );
    let got = messages("resources:\n  gpu:\n    units: count\n");
    assert_eq!(
        got,
        ["2:4 unknown key `units` in `resources.gpu`: did you mean `unit`?"]
    );
    // A key out of line with its siblings is the parser's indentation error
    // alone: the scanner does not guess which mapping it was meant for.
    let got = messages("cache:\n    spillThresholdBytes: 1\n  lock: x\n");
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(
        got[0].starts_with("2:2 did not find expected key"),
        "{got:?}"
    );
    // Tab indentation: the parser's error, not a key named `\tbackend`.
    let got = messages("lock:\n\tbackend: fs\n");
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(
        got[0].starts_with("1:0 found character that cannot start"),
        "{got:?}"
    );
    // Errors no key check knows about.
    let got = messages("version: a\nversion: b\n");
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].contains("duplicate field `version`"), "{got:?}");
    let got = messages("memCache:\n  capacityBytes: 0\n");
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].contains("missing field `perEntryBytes`"), "{got:?}");
}

#[test]
fn parser_errors_land_on_their_location() {
    let got = messages("lock:\n  backend: disk\n");
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].starts_with("1:11 "), "{got:?}");
    assert!(got[0].contains("disk"), "{got:?}");
    assert!(!got[0].contains(" at line "), "{got:?}");
    let got = messages("resources:\n  Bad: 1\n");
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].starts_with("1:2 "), "{got:?}");
    assert!(got[0].contains("not a valid pool name"), "{got:?}");
    // The plugin entry's own error sits on its item.
    let got = messages("plugins:\n- builtin: a\n  path: b\n");
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].starts_with("1:0 "), "{got:?}");
    assert!(got[0].contains("exactly one"), "{got:?}");
    // A column counts characters, and starts after a byte-order mark.
    let got = messages("\u{feff}caches:\n  é🎉: {read: maybe}\n");
    assert_eq!(got.len(), 1, "{got:?}");
    let d = &diagnostics("\u{feff}caches:\n  é🎉: {read: maybe}\n", ENC)[0];
    let text = "\u{feff}caches:\n  é🎉: {read: maybe}\n";
    let start = position::to_offset(text, d.range.start, ENC).expect("offset");
    assert!(
        text.get(start..).is_some_and(|t| t.starts_with("maybe")),
        "{got:?}"
    );
    let got = messages("\u{feff}lock:\n  backend: disk\n");
    assert!(got[0].starts_with("1:11 "), "{got:?}");
}

#[test]
fn syntax_errors_are_reported() {
    let got = messages("cache: [\n");
    assert_eq!(got.len(), 1, "{got:?}");
}
