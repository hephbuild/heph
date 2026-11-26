//! Guards that no consumer counts cache hits from the tier events.
//!
//! `LocalCacheMiss` and `RemoteCacheMiss` fire once per cache *tier* consulted,
//! not once per target: a target served from the remote is also a local miss,
//! so a hit rate folded from them counts every remote hit as a miss — the bug
//! that had CI report a cold cache on a fully cached build. The per-target
//! numbers are `CacheOutcome` / `CacheHitRebuilt`, folded by
//! `hcore::events::CacheCounts`, which every consumer shares.
//!
//! A new consumer that matches on a tier miss compiles, passes its own tests,
//! and quietly brings the miscount back. This gate is what notices: a
//! non-test source file outside the allowlist that names either event fails.
//!
//! Scoped to `src/` of each workspace member and of the root package, minus
//! test code: each file's inline `#[cfg(test)] mod … { … }`, and every file
//! an out-of-line `#[cfg(test)] mod name;` declares. A test feeding a tier
//! event to prove a consumer *ignores* it is the point, not an offence. Cheap
//! on purpose: a walk and some string work, no build.

use std::path::{Path, PathBuf};

use super::execrunner_gate::{repo_root, rust_files, workspace_members};

/// The tier events no hit rate may be folded from.
const TIER_MISSES: &[&str] = &["LocalCacheMiss", "RemoteCacheMiss"];

/// Files allowed to name a tier miss outside their tests, and why.
const ALLOWED: &[(&str, &str)] = &[
    ("crates/core/src/events.rs", "defines the events"),
    ("crates/engine/src/engine/result.rs", "emits them"),
    (
        "crates/engine/src/engine/diag.rs",
        "touches the target's progress; counts nothing",
    ),
    (
        "crates/plugin-gh/src/tally.rs",
        "an explicit no-op match arm; never counts them",
    ),
    (
        "crates/telemetry/src/telemetry/collector.rs",
        "the deprecated tier counters, documented as tier events",
    ),
    (
        "crates/tui/src/tui/progress.rs",
        "an explicit no-op match arm; never counts them",
    ),
    (
        "crates/tui/src/tui/session/derive.rs",
        "records each tier probe as its own span event (`heph.cache.local_miss` …), \
         documented as a probe, not the outcome; the span's `heph.cache` attribute, \
         from `CacheOutcome`, is the outcome. Records, counts nothing",
    ),
    (
        "crates/tui/src/tui/session/recorder.rs",
        "an exhaustive match deciding which kinds survive the recording cap; counts nothing",
    ),
    (
        "crates/tui/src/tui/session/store.rs",
        "maps each event kind to its wire tag for the recording; counts nothing",
    ),
];

/// The part of `text` before its test module — the first `#[cfg(test)]` that
/// is directly followed by a top-level inline `mod name {`. A `#[cfg(test)]`
/// on a single item (a const, a helper fn) does not end the production code,
/// and neither does an out-of-line `mod name;`: the code after that
/// declaration is still production code (the declared file is excluded by
/// [`test_module_paths`] instead).
fn non_test(text: &str) -> &str {
    let mut offset = 0;
    let mut lines = text.split_inclusive('\n').peekable();
    while let Some(line) = lines.next() {
        if line.trim_end() == "#[cfg(test)]"
            && lines
                .peek()
                .is_some_and(|next| next.starts_with("mod ") && next.trim_end().ends_with('{'))
        {
            return text.get(..offset).unwrap_or(text);
        }
        offset += line.len();
    }
    text
}

/// The name an out-of-line module declaration declares: `mod name;`, with
/// any `pub` / `pub(crate)` visibility in front. `None` for anything else,
/// an inline `mod name {` included.
fn out_of_line_mod(line: &str) -> Option<&str> {
    let line = line.trim_end();
    let rest = line
        .strip_prefix("pub(crate) ")
        .or_else(|| line.strip_prefix("pub(super) "))
        .or_else(|| line.strip_prefix("pub "))
        .unwrap_or(line);
    let name = rest.strip_prefix("mod ")?.strip_suffix(';')?;
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
        .then_some(name)
}

/// Where the test modules `file` declares out of line live: for each
/// `#[cfg(test)]` directly followed by `mod name;`, the path `name.rs` or
/// `name/mod.rs` is reached by, minus its extension. Such a file carries no
/// `#[cfg(test)]` of its own — the attribute is on its declaration, in the
/// parent — so [`non_test`] alone reads all of it as production code.
fn test_module_paths(file: &Path, text: &str) -> Vec<PathBuf> {
    let Some(dir) = file.parent() else {
        return Vec::new();
    };
    // `mod.rs`, `lib.rs` and `main.rs` own their directory; `foo.rs` owns
    // `foo/`.
    let base = match file.file_stem().and_then(|s| s.to_str()) {
        Some("mod" | "lib" | "main") | None => dir.to_path_buf(),
        Some(stem) => dir.join(stem),
    };
    let mut out = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        if line.trim_end() != "#[cfg(test)]" {
            continue;
        }
        if let Some(name) = lines.peek().and_then(|next| out_of_line_mod(next)) {
            out.push(base.join(name));
        }
    }
    out
}

/// Whether `file` is (inside) one of `test_mods`: `<m>.rs`, or anything
/// under `<m>/`.
fn is_test_module(file: &Path, test_mods: &[PathBuf]) -> bool {
    test_mods
        .iter()
        .any(|m| file.with_extension("") == *m || file.starts_with(m))
}

/// `line` up to the `//` that starts a line comment, if any. A `//` inside a
/// string literal — an addr such as `"//a:x"` — starts nothing, so the code
/// after it is still code. Escapes and the `'"'` char literal are honoured;
/// that is all a line-oriented scan needs.
fn strip_line_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut in_str = false;
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        match (in_str, b) {
            (true, b'\\') => i += 1,
            (_, b'"') => in_str = !in_str,
            (false, b'\'') if bytes.get(i + 1..i + 3) == Some(b"\"'") => i += 2,
            (false, b'/') if bytes.get(i + 1) == Some(&b'/') => {
                return line.get(..i).unwrap_or(line);
            }
            _ => {}
        }
        i += 1;
    }
    line
}

/// The 1-based line number of each code line in `code` naming a tier miss. A
/// line that only mentions one in a comment is not a use.
fn tier_mentions(code: &str) -> impl Iterator<Item = usize> + '_ {
    code.lines().enumerate().filter_map(|(n, line)| {
        let code = strip_line_comment(line);
        TIER_MISSES
            .iter()
            .any(|ev| code.contains(ev))
            .then_some(n + 1)
    })
}

/// Every non-test source file in the workspace, relative to the repo root. A
/// file an out-of-line `#[cfg(test)] mod` declares is test code and left out.
fn source_files() -> Vec<(String, String)> {
    let root = repo_root();
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("read root Cargo.toml");
    let members = workspace_members(&manifest);
    assert!(members.len() > 10, "parsed only {} members", members.len());

    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    for member in &members {
        rust_files(&root.join(member).join("src"), &mut files);
    }
    let files: Vec<(PathBuf, String)> = files
        .into_iter()
        .filter_map(|file| {
            let text = std::fs::read_to_string(&file).ok()?;
            Some((file, text))
        })
        .collect();
    let test_mods: Vec<PathBuf> = files
        .iter()
        .flat_map(|(file, text)| test_module_paths(file, text))
        .collect();
    files
        .into_iter()
        .filter(|(file, _)| !is_test_module(file, &test_mods))
        .map(|(file, text)| {
            let rel = file
                .strip_prefix(&root)
                .unwrap_or(&file)
                .display()
                .to_string();
            (rel, text)
        })
        .collect()
}

#[test]
fn no_consumer_counts_cache_tier_events() {
    let mut offenders = Vec::new();
    for (rel, text) in source_files() {
        if ALLOWED.iter().any(|(path, _)| *path == rel) {
            continue;
        }
        offenders.extend(tier_mentions(non_test(&text)).map(|n| format!("{rel}:{n}")));
    }
    assert!(
        offenders.is_empty(),
        "these name `LocalCacheMiss`/`RemoteCacheMiss` outside a test:\n  {}\n\n\
         Those are tier events: they fire per cache tier consulted, so a remote \
         hit is also a local miss, and a hit rate folded from them counts every \
         remote-served target as a miss. Count targets with `CacheOutcome` and \
         `CacheHitRebuilt`, folded by `hcore::events::CacheCounts`. If the file \
         genuinely needs the tier event for something that is not counting \
         (a progress touch, a no-op arm), add it to ALLOWED with the reason.",
        offenders.join("\n  ")
    );
}

/// The allowlist must not rot: an entry that stopped naming a tier miss in
/// its production code should leave the list.
#[test]
fn the_cache_tier_allowlist_is_minimal() {
    let root = repo_root();
    for (path, why) in ALLOWED {
        let text = std::fs::read_to_string(root.join(path))
            .unwrap_or_else(|e| panic!("read allowlisted {path}: {e}"));
        assert!(
            tier_mentions(non_test(&text)).next().is_some(),
            "`{path}` is allowlisted ({why}) but no longer names a tier miss \
             outside its tests — drop it from ALLOWED"
        );
    }
}

/// A `//` in a string is not a comment: an addr before the event name must
/// not hide it. A real comment still does.
#[test]
fn tier_mentions_see_past_a_string_containing_slashes() {
    let code = "let e = Ev { addr: \"//a:x\".into(), RemoteCacheMiss };\n\
                // RemoteCacheMiss in a comment\n\
                let x = 1; // LocalCacheMiss trailing\n\
                let q = '\"'; RemoteCacheMiss\n\
                let s = \"a \\\" // b\"; LocalCacheMiss\n";
    assert_eq!(tier_mentions(code).collect::<Vec<_>>(), vec![1, 4, 5]);
}

#[test]
fn non_test_stops_at_the_test_module_only() {
    let text = "a\n#[cfg(test)]\nconst X: u8 = 1;\nb\n#[cfg(test)]\nmod tests {\nc\n}\n";
    assert_eq!(non_test(text), "a\n#[cfg(test)]\nconst X: u8 = 1;\nb\n");
    assert_eq!(non_test("no tests\n"), "no tests\n");
}

/// An out-of-line `#[cfg(test)] mod name;` near the top of a file declares
/// a test file; the production code after it is still scanned. Stopping
/// there once hid every line of `progress.rs` below its first such
/// declaration.
#[test]
fn non_test_reads_past_an_out_of_line_test_module() {
    let text = "a\n#[cfg(test)]\nmod approval_tests;\nb\n#[cfg(test)]\nmod tests {\nc\n}\n";
    assert_eq!(non_test(text), "a\n#[cfg(test)]\nmod approval_tests;\nb\n");
}

/// The file an out-of-line test module lives in is test code, whichever
/// layout reaches it; a sibling whose name merely starts the same is not.
#[test]
fn out_of_line_test_modules_are_test_files() {
    let text = "mod derive;\n#[cfg(test)]\nmod store_tests;\n#[cfg(test)]\n\
                pub(crate) mod test_events;\n#[cfg(test)]\nmod inline {\n}\n";
    let in_mod = test_module_paths(Path::new("/c/src/session/mod.rs"), text);
    assert_eq!(
        in_mod,
        vec![
            PathBuf::from("/c/src/session/store_tests"),
            PathBuf::from("/c/src/session/test_events"),
        ]
    );
    let in_file = test_module_paths(Path::new("/c/src/progress.rs"), text);
    assert_eq!(
        in_file.first(),
        Some(&PathBuf::from("/c/src/progress/store_tests"))
    );

    let test = |p: &str| is_test_module(Path::new(p), &in_mod);
    assert!(test("/c/src/session/store_tests.rs"));
    assert!(test("/c/src/session/store_tests/mod.rs"));
    assert!(test("/c/src/session/test_events/more.rs"));
    assert!(!test("/c/src/session/derive.rs"));
    assert!(!test("/c/src/session/store_tests_extra.rs"));
    assert!(!test("/c/src/session/store.rs"));
}
