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
//! each file's `#[cfg(test)] mod`: a test feeding a tier event to prove a
//! consumer *ignores* it is the point, not an offence. Cheap on purpose: a
//! walk and some string work, no build.

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
        "crates/plugin-gha/src/tally.rs",
        "tells an old host that consulted the cache from one that never did; never counts them",
    ),
    (
        "crates/tui/src/tui/progress.rs",
        "an explicit no-op match arm",
    ),
    (
        "crates/telemetry/src/telemetry/collector.rs",
        "the deprecated tier counters, documented as tier events",
    ),
];

/// The part of `text` before its test module — the first `#[cfg(test)]` that
/// is directly followed by a top-level `mod`. A `#[cfg(test)]` on a single
/// item (a const, a helper fn) does not end the production code.
fn non_test(text: &str) -> &str {
    let mut offset = 0;
    let mut lines = text.split_inclusive('\n').peekable();
    while let Some(line) = lines.next() {
        if line.trim_end() == "#[cfg(test)]"
            && lines.peek().is_some_and(|next| next.starts_with("mod "))
        {
            return text.get(..offset).unwrap_or(text);
        }
        offset += line.len();
    }
    text
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

/// Every non-test source file in the workspace, relative to the repo root.
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
    files
        .into_iter()
        .filter_map(|file| {
            let text = std::fs::read_to_string(&file).ok()?;
            let rel = file
                .strip_prefix(&root)
                .unwrap_or(&file)
                .display()
                .to_string();
            Some((rel, text))
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
