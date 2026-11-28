//! Which commands have a JSON form, across the whole CLI.
//!
//! A leaf command opts into `--json`/`--fields`/`--jq` by flattening
//! `output::JsonArgs` (or `output::DocArgs`), which puts the clap group
//! `output` on it. Every leaf that does not is on [`NO_JSON`], with the reason,
//! and the tests here walk the clap tree, hidden commands included, so a new
//! command is one or the other. Adding a command to the list is a reviewed
//! decision (the spec's "Accepted exemptions").

/// How a command without a JSON form refuses the output flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// It has none of the flags, so clap rejects each one with exit 2.
    NoFlags,
    /// Its trailing arguments are somebody else's (an applet's argv, a removed
    /// command's rest): an output flag is one of them, never parsed as heph's.
    Swallowed,
    /// It opts in for one mode only. In the other, a `requires` or
    /// `conflicts_with` on the `output` group refuses the flags with exit 2:
    /// these argv (after `heph`) must fail to parse.
    OtherMode(&'static [&'static str]),
    /// Dispatched by `main` before clap parses anything, so it is not in the
    /// clap tree at all.
    BeforeClap,
}

/// A command with no JSON form.
#[derive(Debug, Clone, Copy)]
pub struct Exempt {
    /// The command path after `heph`, space-separated.
    pub path: &'static str,
    /// How it refuses the output flags.
    pub refusal: Refusal,
    /// Why it has no JSON form, and what to use instead.
    pub reason: &'static str,
}

/// Every command without a JSON form.
pub const NO_JSON: &[Exempt] = &[
    Exempt {
        path: "log",
        refusal: Refusal::NoFlags,
        reason: "its output is the log's bytes; `heph inspect failure --json` has the log's path",
    },
    Exempt {
        path: "session attach",
        refusal: Refusal::NoFlags,
        reason: "a terminal UI; `heph session show --json` is the snapshot",
    },
    Exempt {
        path: "auth login",
        refusal: Refusal::NoFlags,
        reason: "interactive: runs sign-in commands that talk to the user",
    },
    Exempt {
        path: "inspect deps",
        refusal: Refusal::OtherMode(&["inspect", "deps", "//p:t", "-i", "--json"]),
        reason: "`-i` is a terminal UI; `heph inspect deps --json` without it",
    },
    Exempt {
        path: "tool completions",
        refusal: Refusal::NoFlags,
        reason: "its output is a shell script",
    },
    Exempt {
        path: "tool coreutils run",
        // After the applet's name the flags are the applet's arguments.
        refusal: Refusal::NoFlags,
        reason: "its output is the applet's own",
    },
    Exempt {
        path: "tool build-lsp",
        refusal: Refusal::OtherMode(&["tool", "build-lsp", "--json"]),
        reason: "the server speaks JSON-RPC on stdio already; `--explain` opts in",
    },
    Exempt {
        path: "gen-docs",
        refusal: Refusal::NoFlags,
        reason: "hidden; its output is markdown for the docs site",
    },
    Exempt {
        path: "tool gen-gitignore",
        refusal: Refusal::Swallowed,
        reason: "hidden and removed: it prints a removal error on stderr whatever it is given; \
                 the codegen commands replace it",
    },
    Exempt {
        path: "__auth-helper",
        refusal: Refusal::BeforeClap,
        reason: "hidden; its stdout is the AWS, GCP, Docker and git credential-helper protocols",
    },
];

/// Every leaf of `cmd`'s tree, hidden ones included, with its path after the
/// binary name.
pub fn leaves(cmd: &clap::Command) -> Vec<(String, clap::Command)> {
    fn walk(cmd: &clap::Command, path: &str, out: &mut Vec<(String, clap::Command)>) {
        for sub in cmd.get_subcommands() {
            // `help` is clap's, not heph's.
            if sub.get_name() == "help" {
                continue;
            }
            let sub_path = if path.is_empty() {
                sub.get_name().to_string()
            } else {
                format!("{path} {}", sub.get_name())
            };
            if sub.has_subcommands() {
                walk(sub, &sub_path, out);
            } else {
                out.push((sub_path, sub.clone()));
            }
        }
    }
    let mut out = Vec::new();
    walk(cmd, "", &mut out);
    out
}

/// Whether `leaf` carries the `output` group, i.e. opted in.
pub fn opted_in(leaf: &clap::Command) -> bool {
    leaf.get_groups().any(|g| g.get_id() == "output")
}

/// The field table of an opted-in leaf, read from its own `--fields` parser:
/// `None` for a leaf with no `--fields` (D11's always-JSON documents).
pub fn field_table(leaf: &clap::Command) -> Option<&'static [output::Field]> {
    let fields = leaf.get_arguments().find(|a| a.get_id() == "fields")?;
    // Parse `--fields help` with that one argument, so none of the leaf's
    // own requirements (a positional, `--explain`) is in the way.
    let cmd = clap::Command::new("leaf")
        .group(clap::ArgGroup::new("output").multiple(true))
        .arg(fields.clone());
    let matches = cmd
        .try_get_matches_from(["leaf", "--fields", "help"])
        .ok()?;
    matches
        .try_get_many::<output::FieldSel>("fields")
        .ok()
        .flatten()?
        .find_map(|sel| match sel {
            output::FieldSel::Help(fields) => Some(*fields),
            _ => None,
        })
}

/// Whether clap's rendering of an argument (`--fields <FIELDS>`) is one of
/// the output flags.
fn is_output_flag(rendered: &str) -> bool {
    ["--json", "--fields", "--jq"].iter().any(|flag| {
        rendered
            .strip_prefix(flag)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '=']))
    })
}

/// `argv` without the output flags (and their values), up to a `--`; `None`
/// when it has none.
fn without_output_flags(argv: &[std::ffi::OsString]) -> Option<Vec<std::ffi::OsString>> {
    let mut kept = Vec::with_capacity(argv.len());
    let mut words = argv.iter();
    let mut removed = false;
    while let Some(word) = words.next() {
        match word.to_str() {
            Some("--") => {
                kept.push(word.clone());
                kept.extend(words.cloned());
                break;
            }
            Some("--json") => removed = true,
            Some("--fields" | "--jq") => {
                removed = true;
                let _value = words.next();
            }
            Some(w) if w.starts_with("--fields=") || w.starts_with("--jq=") => removed = true,
            _ => kept.push(word.clone()),
        }
    }
    removed.then_some(kept)
}

/// `err` with a tip naming what to use instead, when it refuses an output
/// flag on a command of [`NO_JSON`]: `heph log --json` says why `log` has no
/// JSON form, not only that `--json` is unexpected. Any other error comes
/// back as it was; the kind, and so the exit code (2), never changes.
///
/// The flags are the refused part when clap names one as the unexpected
/// argument, or when `argv` without them parses (`tool build-lsp --json`
/// fails for a missing `--explain`, and parses without `--json`). The leaf
/// is found by following `argv`'s words down `cmd`'s subcommands, the way
/// clap dispatched them.
pub fn with_no_json_hint(
    mut err: clap::Error,
    cmd: &clap::Command,
    argv: impl IntoIterator<Item = std::ffi::OsString>,
) -> clap::Error {
    use clap::error::{ContextKind, ContextValue, ErrorKind};
    let argv: Vec<std::ffi::OsString> = argv.into_iter().collect();
    let Some(stripped) = without_output_flags(&argv) else {
        return err;
    };
    let named = err.kind() == ErrorKind::UnknownArgument
        && matches!(err.get(ContextKind::InvalidArg), Some(ContextValue::String(s)) if is_output_flag(s));
    if !named && cmd.clone().try_get_matches_from(stripped).is_err() {
        return err;
    }
    let mut cur = cmd;
    let mut path: Vec<&str> = Vec::new();
    for word in argv.iter().skip(1) {
        let Some(word) = word.to_str() else {
            continue;
        };
        let Some(sub) = cur
            .get_subcommands()
            .find(|s| s.get_name() == word || s.get_all_aliases().any(|a| a == word))
        else {
            continue;
        };
        path.push(sub.get_name());
        cur = sub;
        if !cur.has_subcommands() {
            break;
        }
    }
    let path = path.join(" ");
    let Some(e) = NO_JSON.iter().find(|e| e.path == path) else {
        return err;
    };
    let tip = match e.refusal {
        Refusal::OtherMode(_) => format!(
            "`heph {}` has no JSON output in this mode: {}",
            e.path, e.reason
        ),
        _ => format!("`heph {}` has no JSON output: {}", e.path, e.reason),
    };
    let mut tips = match err.get(ContextKind::Suggested) {
        Some(ContextValue::StyledStrs(tips)) => tips.clone(),
        _ => Vec::new(),
    };
    tips.push(tip.into());
    err.insert(ContextKind::Suggested, ContextValue::StyledStrs(tips));
    err
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::gendocs::cli_command;
    use std::collections::BTreeSet;

    fn exempt(path: &str) -> Option<&'static Exempt> {
        NO_JSON.iter().find(|e| e.path == path)
    }

    #[test]
    fn every_leaf_command_opts_in_or_is_listed() {
        let leaves = leaves(&cli_command());
        assert!(
            leaves.len() > 40,
            "the walk found the tree: {}",
            leaves.len()
        );
        let mut problems = Vec::new();
        for (path, leaf) in &leaves {
            match (opted_in(leaf), exempt(path).map(|e| e.refusal)) {
                (true, None) | (false, Some(Refusal::NoFlags | Refusal::Swallowed)) => {}
                // Opted in for one mode, refused in the other: checked below.
                (true, Some(Refusal::OtherMode(_))) => {}
                (false, None) => problems.push(format!(
                    "`heph {path}` neither opts into the output flags (flatten \
                     output::JsonArgs) nor is on NO_JSON with a reason"
                )),
                (true, Some(_)) => problems.push(format!(
                    "`heph {path}` opts in but is on NO_JSON: drop its entry"
                )),
                (false, Some(r)) => problems.push(format!(
                    "`heph {path}` has no output group, but NO_JSON says {r:?}"
                )),
            }
        }
        // No stale entry: each names a leaf, except the one clap never sees.
        let paths: BTreeSet<&str> = leaves.iter().map(|(p, _)| p.as_str()).collect();
        for e in NO_JSON {
            assert!(!e.reason.is_empty(), "{} has no reason", e.path);
            match e.refusal {
                Refusal::BeforeClap => {
                    assert!(!paths.contains(e.path), "{} is in clap after all", e.path);
                    // Still dispatched by `main`: the entry is not stale.
                    assert_eq!(e.path, crate::engine::credential::AUTH_HELPER_SUBCOMMAND);
                    let argv = ["heph", e.path, "docker", "--pin", "/p", "get"]
                        .map(std::ffi::OsString::from);
                    assert!(
                        crate::commands::auth_helper::parse(argv).is_some(),
                        "`heph {}` is no longer dispatched before clap",
                        e.path
                    );
                }
                _ => {
                    if !paths.contains(e.path) {
                        problems.push(format!("NO_JSON names `heph {}`, not a leaf", e.path));
                    }
                }
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    /// I2: on a command that has not opted in, each output flag fails to
    /// parse, and clap's error exits 2 (`main` calls `e.exit()`).
    #[test]
    fn output_flags_rejected_where_not_opted_in() {
        let cli = cli_command();
        let mut checked = 0;
        for e in NO_JSON {
            let argvs: Vec<Vec<&str>> = match e.refusal {
                Refusal::NoFlags => [&["--json"][..], &["--fields", "addr"], &["--jq", "."]]
                    .iter()
                    .map(|flags| {
                        let mut argv = vec!["heph"];
                        argv.extend(e.path.split(' '));
                        argv.extend(*flags);
                        argv
                    })
                    .collect(),
                Refusal::OtherMode(argv) => {
                    vec![
                        std::iter::once("heph")
                            .chain(argv.iter().copied())
                            .collect(),
                    ]
                }
                Refusal::Swallowed | Refusal::BeforeClap => continue,
            };
            for argv in argvs {
                let err = match cli.clone().try_get_matches_from(&argv) {
                    Ok(_) => panic!("{argv:?} parsed"),
                    Err(err) => err,
                };
                // What `main` prints: clap's refusal, plus why the command
                // has no JSON form and what to use instead.
                let kind = err.kind();
                let err = with_no_json_hint(err, &cli, argv.iter().map(Into::into));
                assert_eq!(err.kind(), kind, "{argv:?}: the hint changed the error");
                let rendered = err.render().to_string();
                assert!(
                    rendered.contains(&format!("tip: `heph {}` has no JSON output", e.path))
                        && rendered.contains(e.reason),
                    "{argv:?}: no hint naming what to use instead:\n{rendered}"
                );
                assert_eq!(err.exit_code(), 2, "{argv:?}: {err}");
                assert!(
                    matches!(
                        err.kind(),
                        clap::error::ErrorKind::UnknownArgument
                            | clap::error::ErrorKind::ArgumentConflict
                            | clap::error::ErrorKind::MissingRequiredArgument
                    ),
                    "{argv:?} failed for another reason: {err}"
                );
                checked += 1;
            }
        }
        assert!(checked >= 15, "{checked}");
    }

    /// The hint is only for an output flag a [`NO_JSON`] command refuses:
    /// another unknown flag there, a near-miss flag name, or another mistake
    /// beside an output flag is clap's error alone.
    #[test]
    fn no_json_hint_only_for_output_flags() {
        let cli = cli_command();
        for argv in [
            &["heph", "log", "//p:t", "--bogus"][..],
            &["heph", "log", "//p:t", "--jsonl"],
            &["heph", "log", "//p:t", "--fieldsx", "a"],
            &["heph", "tool", "completions", "--jqq", "."],
            // `inspect deps` has JSON; what is wrong here is the missing addr.
            &["heph", "inspect", "deps", "--json"],
        ] {
            let err = cli
                .clone()
                .try_get_matches_from(argv)
                .expect_err("an unknown flag");
            let err = with_no_json_hint(err, &cli, argv.iter().map(Into::into));
            let rendered = err.render().to_string();
            assert!(
                !rendered.contains("has no JSON output"),
                "{argv:?}:\n{rendered}"
            );
        }
        assert!(is_output_flag("--fields <FIELDS>") && is_output_flag("--jq=."));
    }

    /// The flag is swallowed as an argument, so it is never parsed as
    /// heph's: the commands' own handling (an unknown applet, the removal
    /// error) sees it.
    #[test]
    fn swallowed_output_flags_reach_the_trailing_args() {
        for e in NO_JSON.iter().filter(|e| e.refusal == Refusal::Swallowed) {
            let mut argv = vec!["heph"];
            argv.extend(e.path.split(' '));
            argv.push("--json");
            let m = cli_command()
                .try_get_matches_from(&argv)
                .unwrap_or_else(|err| panic!("{argv:?}: {err}"));
            let mut leaf = &m;
            while let Some((_, sub)) = leaf.subcommand() {
                leaf = sub;
            }
            assert!(
                !leaf.ids().any(|id| id == "json"),
                "{argv:?}: --json parsed as heph's"
            );
        }
    }

    /// How the snapshot expands a nested type a field names (`@Name` in its
    /// kind).
    enum Nested {
        /// Its `__OUTPUT_SHAPE`, from `derive(Record)` or `derive(Shape)`:
        /// the keys serde writes, read from the same attributes.
        Derived(&'static [(&'static str, &'static str)]),
        /// A hand-written `Serialize`: the shape is written here, and nothing
        /// checks it against the impl. Keep the two in step.
        Hand(&'static [(&'static str, &'static str)]),
    }

    /// Every type nested in a `--json` record, by its Rust name (a field's
    /// kind names it as `@Name`). A name the walk meets that is not here
    /// fails `field_tables_snapshot`, so a new nested type cannot go
    /// unpinned.
    fn nested(name: &str) -> Option<Nested> {
        use crate::commands::{auth, inspect, session, tool};
        use Nested::{Derived, Hand};
        Some(match name {
            // `Serialize` as one string.
            "Addr" | "TargetAddr" => Hand(&[("", "string")]),
            "ExplainStep" => Derived(auth::ExplainStep::__OUTPUT_SHAPE),
            "State" => Derived(auth::State::__OUTPUT_SHAPE),
            "StateRow" => Derived(inspect::states::StateRow::__OUTPUT_SHAPE),
            "BlockedOn" => Derived(session::BlockedOn::__OUTPUT_SHAPE),
            "SessionState" => Derived(session::SessionState::__OUTPUT_SHAPE),
            "Progress" => Derived(session::list::Progress::__OUTPUT_SHAPE),
            "SessionEnd" => Derived(htui::tui::session::format::SessionEnd::__OUTPUT_SHAPE),
            "RunningTarget" => Derived(htui::tui::session::format::RunningTarget::__OUTPUT_SHAPE),
            "LockWaitEntry" => Derived(htui::tui::session::format::LockWaitEntry::__OUTPUT_SHAPE),
            "AdmissionWaitEntry" => {
                Derived(htui::tui::session::format::AdmissionWaitEntry::__OUTPUT_SHAPE)
            }
            "SnapshotCounts" => Derived(htui::tui::session::format::SnapshotCounts::__OUTPUT_SHAPE),
            "ListTotals" => Derived(htui::tui::session::format::ListTotals::__OUTPUT_SHAPE),
            "ApprovalStanding" => {
                Derived(htui::tui::session::format::ApprovalStanding::__OUTPUT_SHAPE)
            }
            "ListTotal" => Derived(htui::tui::session::format::ListTotal::__OUTPUT_SHAPE),
            "PathResult" => Derived(tool::codegen::keep::PathResult::__OUTPUT_SHAPE),
            "PathResultKind" => Derived(tool::codegen::keep::PathResultKind::__OUTPUT_SHAPE),
            "WhyItem" => Derived(tool::codegen::why::WhyItem::__OUTPUT_SHAPE),
            "WhyRow" => Derived(tool::codegen::why::WhyRow::__OUTPUT_SHAPE),
            "WhyLine" => Derived(tool::codegen::why::WhyLine::__OUTPUT_SHAPE),
            "Released" => Derived(tool::codegen::gitignore::Released::__OUTPUT_SHAPE),
            // `impl Serialize for StatusItem`: an unresolved package has no
            // path, addr or output, but a package and an error.
            "StatusItem" => Hand(&[
                (".path", "string?"),
                (".kind", "string"),
                (".addr", "string?"),
                (".output", "string?"),
                (".package", "string?"),
                (".error", "string?"),
                (".fix", "string?"),
            ]),
            // `impl Serialize for LineChange`.
            "LineChange" => Hand(&[
                (".op", "\"add\""),
                (".op", "\"remove\""),
                (".pattern", "string"),
                (".addr", "string"),
            ]),
            // Each written through a `serialize_with` that changes its kind:
            // `reason_strs` (gitignore.rs), `removed_lines` (keep.rs).
            "StaleReason" => Hand(&[
                ("", "\"lines\""),
                ("", "\"malformed\""),
                ("", "\"old_marker\""),
            ]),
            "GitignoreEntry" => Hand(&[
                (".op", "\"remove\""),
                (".pattern", "string"),
                (".addr", "string"),
            ]),
            "Next" => Derived(session::show::Next::__OUTPUT_SHAPE),
            "NextKind" => Derived(session::show::NextKind::__OUTPUT_SHAPE),
            "DisabledBy" => Derived(htelemetry::telemetry::DisabledBy::__OUTPUT_SHAPE),
            "FlushMode" => Derived(htelemetry::telemetry::FlushMode::__OUTPUT_SHAPE),
            "RestoresFrom" => Derived(tool::scratch::RestoresFrom::__OUTPUT_SHAPE),
            "RemoteSkipped" => Derived(tool::scratch::RemoteSkipped::__OUTPUT_SHAPE),
            "LocalView" => Derived(tool::scratch::LocalView::__OUTPUT_SHAPE),
            "RemoteView" => Derived(tool::scratch::RemoteView::__OUTPUT_SHAPE),
            "PushOutcome" => Derived(tool::scratch::PushOutcome::__OUTPUT_SHAPE),
            "PullOutcome" => Derived(tool::scratch::PullOutcome::__OUTPUT_SHAPE),
            "Outcome" => Derived(crate::commands::run_json::Outcome::__OUTPUT_SHAPE),
            "CacheTier" => Derived(crate::commands::run_json::CacheTier::__OUTPUT_SHAPE),
            "RunFailure" => Derived(crate::commands::run_json::RunFailure::__OUTPUT_SHAPE),
            // `hcore::diagnostic`'s, which cannot derive `output::Shape`
            // (`hcore` does not depend on `output`): each `serde` shape.
            "Stage" => Hand(&[
                ("", "\"eval\""),
                ("", "\"spec\""),
                ("", "\"parse\""),
                ("", "\"link\""),
                ("", "\"run\""),
                ("", "\"cache\""),
                ("", "\"config\""),
                ("", "\"cli\""),
            ]),
            "SourceLoc" => Hand(&[(".file", "string"), (".line", "number"), (".col", "number")]),
            "Hint" => Hand(&[(".kind", "@HintKind"), (".text", "string")]),
            "HintKind" => Hand(&[
                ("", "\"did_you_mean\""),
                ("", "\"valid_values\""),
                ("", "\"declare_tool\""),
                ("", "\"see_log\""),
                ("", "\"other\""),
            ]),
            _ => return None,
        })
    }

    /// Expand one value at `path` of `kind` into snapshot `(path, kind)`
    /// pairs, descending into each nested type it names.
    fn expand(
        path: &str,
        kind: &str,
        depth: usize,
        out: &mut Vec<(String, String)>,
        unknown: &mut BTreeSet<String>,
    ) {
        out.push((path.to_string(), display_kind(kind, unknown)));
        if depth >= 4 {
            return;
        }
        // Each `@Name`, with the brackets around it: `[` is an array level
        // (`[]` in the path), `{` a map level (`.*`).
        let mut suffix: Vec<&str> = Vec::new();
        for tok in kind_tokens(kind) {
            match tok {
                KindTok::Char('[') => suffix.push("[]"),
                KindTok::Char('{') => suffix.push(".*"),
                KindTok::Char(']' | '}') => {
                    suffix.pop();
                }
                KindTok::Char(_) => {}
                KindTok::Name(name) => {
                    let Some(Nested::Derived(shape) | Nested::Hand(shape)) = nested(&name) else {
                        continue;
                    };
                    let base = format!("{path}{}", suffix.concat());
                    for (sub, k) in shape {
                        if !sub.is_empty() || k.contains('@') {
                            expand(&format!("{base}{sub}"), k, depth + 1, out, unknown);
                        } else if k.starts_with('"') {
                            // One value of a string enum.
                            out.push((base.clone(), (*k).to_string()));
                        }
                    }
                }
            }
        }
    }

    enum KindTok {
        Char(char),
        /// `@Name`.
        Name(String),
    }

    fn kind_tokens(kind: &str) -> Vec<KindTok> {
        let mut out = Vec::new();
        let mut chars = kind.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '@' {
                out.push(KindTok::Char(c));
                continue;
            }
            let mut name = String::new();
            while let Some(&c) = chars.peek() {
                if !(c.is_alphanumeric() || c == '_') {
                    break;
                }
                name.push(c);
                chars.next();
            }
            out.push(KindTok::Name(name));
        }
        out
    }

    /// `kind` with each `@Name` replaced by what it is in JSON: `string` for
    /// a unit-only enum (its values are lines of their own), `object`, or
    /// both.
    fn display_kind(kind: &str, unknown: &mut BTreeSet<String>) -> String {
        let mut out = String::new();
        for tok in kind_tokens(kind) {
            match tok {
                KindTok::Char(c) => out.push(c),
                KindTok::Name(name) => match nested(&name) {
                    Some(Nested::Derived(shape) | Nested::Hand(shape)) => {
                        let values = shape.iter().filter(|(sub, _)| sub.is_empty()).count();
                        let all_literal = shape.iter().all(|(_, k)| k.starts_with('"'));
                        out.push_str(match shape.first() {
                            Some((_, k)) if values == shape.len() && !all_literal => k,
                            _ if values == shape.len() => "string",
                            _ if values == 0 => "object",
                            _ => "string|object",
                        });
                    }
                    None => {
                        out.push('@');
                        out.push_str(&name);
                        unknown.insert(name);
                    }
                },
            }
        }
        out
    }

    /// The snapshot line of each JSON path of each opted-in leaf: its own
    /// fields, then the keys of the types nested in them.
    fn field_lines() -> (BTreeSet<String>, BTreeSet<String>) {
        let mut lines = BTreeSet::new();
        let mut unknown = BTreeSet::new();
        for (path, leaf) in leaves(&cli_command()) {
            let Some(fields) = field_table(&leaf) else {
                continue;
            };
            for f in fields {
                let mut paths = Vec::new();
                expand(f.name, f.kind, 0, &mut paths, &mut unknown);
                for (json_path, kind) in paths {
                    lines.insert(format!(
                        "{path}\t{json_path}\t{kind}\t{}\t{}",
                        f.tier.as_str(),
                        if f.default { "default" } else { "opt-in" }
                    ));
                }
            }
        }
        (lines, unknown)
    }

    const SNAPSHOT: &str = "src/commands/json_fields.snap";

    /// The snapshot's `# contract N` header.
    fn snapshot_contract(text: &str) -> Option<u32> {
        text.lines()
            .find_map(|l| l.strip_prefix("# contract "))
            .and_then(|n| n.trim().parse().ok())
    }

    /// I3: every JSON path ever shipped is still there with its kind, tier
    /// and default membership, and every path there is in the snapshot. A
    /// path is a field, or a key of a type nested in one (`sources[].addr`),
    /// and each value of a string enum is a line of its own. Run with
    /// `HEPH_UPDATE_FIELD_SNAPSHOT=1` to add new paths: the update only adds
    /// lines, so a removed or changed path keeps failing until a human
    /// deletes its line, in review. The header pins `output::OUTPUT_CONTRACT`:
    /// only a contract bump may drop lines, and the update then rewrites the
    /// file from scratch. `inspect spec`, `inspect def` and `tool
    /// config-schema` have no `--fields` and are not in it (D11).
    #[test]
    fn field_tables_snapshot() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(SNAPSHOT);
        let (current, unknown) = field_lines();
        assert!(
            unknown.is_empty(),
            "fields name types the snapshot cannot expand; derive `output::Shape` on each and \
             add it to `nested` in {}: {unknown:?}",
            file!()
        );
        assert!(current.len() > 50, "found the tables: {}", current.len());
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let mut contract = snapshot_contract(&text);
        let mut shipped: BTreeSet<String> = text
            .lines()
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(str::to_string)
            .collect();
        if std::env::var_os("HEPH_UPDATE_FIELD_SNAPSHOT").is_some() {
            if contract != Some(output::OUTPUT_CONTRACT) {
                shipped.clear();
                contract = Some(output::OUTPUT_CONTRACT);
            }
            shipped.extend(current.iter().cloned());
            let mut out = format!(
                "# contract {}\n\
                 # Every JSON path of every `--json` record: command, path, kind, tier, default.\n\
                 # A line is never removed or changed (I3); `HEPH_UPDATE_FIELD_SNAPSHOT=1` only adds.\n",
                output::OUTPUT_CONTRACT
            );
            for l in &shipped {
                out.push_str(l);
                out.push('\n');
            }
            std::fs::write(&path, out).expect("write the snapshot");
        }
        assert_eq!(
            contract,
            Some(output::OUTPUT_CONTRACT),
            "{SNAPSHOT} pins another output contract: a bump to `output::OUTPUT_CONTRACT` \
             starts a new snapshot (HEPH_UPDATE_FIELD_SNAPSHOT=1 rewrites it)"
        );
        let gone: Vec<&String> = shipped.difference(&current).collect();
        assert!(
            gone.is_empty(),
            "shipped fields renamed, removed, retyped or re-tiered (I3; add a new \
             field instead):\n{}",
            gone.iter()
                .map(|l| l.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        );
        let new: Vec<&String> = current.difference(&shipped).collect();
        assert!(
            new.is_empty(),
            "fields not in {SNAPSHOT}; run with HEPH_UPDATE_FIELD_SNAPSHOT=1:\n{}",
            new.iter()
                .map(|l| l.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    /// Contract 2 (error reporting, D5): `run` lost `blocked_by` and the
    /// `"blocked"` outcome, gained `"skipped"`, and the fields a culprit
    /// carries are nullable where a record may lack them (a BUILD file that
    /// did not evaluate has no `addr`; a target with no requested root above
    /// it has no `root`, `more` or `path`; `root` and `root_file` are each
    /// null when the culprit is the other kind). `inspect failure` describes a slot
    /// that may hold no record of its target. Checked over the committed
    /// snapshot and over the tables it is generated from, so neither side can
    /// drift back to contract 1 alone.
    #[test]
    fn json_contract_2_snapshot() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(SNAPSHOT);
        let text = std::fs::read_to_string(&path).expect("read the snapshot");
        assert_eq!(snapshot_contract(&text), Some(2), "{SNAPSHOT} header");
        assert_eq!(output::OUTPUT_CONTRACT, 2, "output::OUTPUT_CONTRACT");

        let shipped: BTreeSet<&str> = text
            .lines()
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        let (current, _) = field_lines();
        let sources: [(&str, BTreeSet<&str>); 2] = [
            ("snapshot", shipped),
            ("field tables", current.iter().map(String::as_str).collect()),
        ];
        // `(command, path)` → the kinds it has, from a `command\tpath\tkind\t…` line.
        let kinds = |lines: &BTreeSet<&str>, cmd: &str, field: &str| -> Vec<String> {
            lines
                .iter()
                .filter_map(|l| {
                    let mut cols = l.split('\t');
                    (cols.next() == Some(cmd) && cols.next() == Some(field))
                        .then(|| cols.next().unwrap_or_default().to_string())
                })
                .collect()
        };
        for (source, lines) in &sources {
            let run: Vec<&&str> = lines.iter().filter(|l| l.starts_with("run\t")).collect();
            assert!(!run.is_empty(), "{source}: no `run` lines");
            assert!(
                run.iter().all(|l| !l.contains("blocked_by")),
                "{source}: `run` still has `blocked_by`"
            );
            assert!(
                run.iter().all(|l| !l.contains("\"blocked\"")),
                "{source}: `run` still has the `\"blocked\"` outcome"
            );
            assert!(
                kinds(lines, "run", "outcome")
                    .iter()
                    .any(|k| k == "\"skipped\""),
                "{source}: `run outcome \"skipped\"` missing"
            );
            for (cmd, field) in [
                ("run", "addr"),
                ("run", "file"),
                ("run", "root"),
                ("run", "root_file"),
                ("run", "more"),
                ("run", "path"),
                ("run", "exit_code"),
                ("inspect failure", "addr"),
                ("inspect failure", "hashin"),
                ("inspect failure", "failed_at"),
            ] {
                let ks = kinds(lines, cmd, field);
                assert!(!ks.is_empty(), "{source}: `{cmd} {field}` missing");
                assert!(
                    ks.iter().all(|k| k.ends_with('?')),
                    "{source}: `{cmd} {field}` is not nullable: {ks:?}"
                );
            }
            // A failure's facts (D5, PR 4), from the record its box is drawn
            // from: null, empty or false on every other record.
            for (field, kind) in [
                ("stage", "string?"),
                ("stage", "\"eval\""),
                ("location", "object?"),
                ("location.file", "string"),
                ("location.line", "number"),
                ("location.col", "number"),
                ("hints", "[object]"),
                ("hints[].kind", "\"did_you_mean\""),
                ("hints[].text", "string"),
                ("truncated", "bool"),
                ("log_replaced", "bool"),
                ("streamed", "bool"),
            ] {
                assert!(
                    kinds(lines, "run", field).iter().any(|k| k == kind),
                    "{source}: `run {field}` is not {kind}"
                );
            }
        }
    }

    /// I4: the default set adds no resolved or built work.
    #[test]
    fn default_fields_are_free() {
        for (path, leaf) in leaves(&cli_command()) {
            for f in field_table(&leaf).unwrap_or_default() {
                assert!(
                    !f.default || f.tier == output::Tier::Free,
                    "`heph {path}`: default field `{}` is {}",
                    f.name,
                    f.tier.as_str()
                );
            }
        }
    }

    /// Every opted-in leaf with a `--fields` yields its table to the walk, so
    /// the snapshot cannot silently miss one.
    #[test]
    fn every_fields_flag_yields_its_table() {
        for (path, leaf) in leaves(&cli_command()) {
            if leaf.get_arguments().any(|a| a.get_id() == "fields") {
                assert!(field_table(&leaf).is_some(), "`heph {path}`");
            }
        }
    }
}
