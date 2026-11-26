use super::*;
use crate::fixtures::{Row, args};
use clap::error::ErrorKind;

fn parse(flags: &[&str]) -> Result<JsonArgs<Row>, clap::Error> {
    JsonArgs::<Row>::try_parse_from(flags)
}

fn selected(flags: &[&str]) -> Vec<&'static str> {
    let selection = args(flags).selection();
    Row::FIELDS
        .iter()
        .enumerate()
        .filter(|(i, _)| selection.contains(*i))
        .map(|(_, f)| f.name)
        .collect()
}

#[test]
fn derived_command_is_valid() {
    use clap::Args as _;
    JsonArgs::<Row>::augment_args(clap::Command::new("t")).debug_assert();
}

#[test]
fn enabled_by_each_flag_alone() {
    assert!(!args(&[]).enabled());
    assert!(args(&["--json"]).enabled());
    assert!(args(&["--fields", "addr"]).enabled());
    assert!(args(&["--jq", "."]).enabled());
}

#[test]
fn fields_parser() {
    // Defaults: the free, non-opt-in fields.
    assert_eq!(selected(&["--json"]), ["addr", "labels"]);
    // Bare names replace; `+` adds; `all`; duplicates are one field.
    assert_eq!(selected(&["--fields", "hashin"]), ["hashin"]);
    assert_eq!(
        selected(&["--fields", "+driver"]),
        ["addr", "labels", "driver"]
    );
    assert_eq!(
        selected(&["--fields", "+driver,+deps"]),
        ["addr", "labels", "driver", "deps"]
    );
    assert_eq!(
        selected(&["--fields", "all"]),
        ["addr", "labels", "driver", "deps", "hashin"]
    );
    assert_eq!(selected(&["--fields", "addr,addr"]), ["addr"]);
    // The flag given twice: both count.
    assert_eq!(
        selected(&["--fields", "addr", "--fields", "deps"]),
        ["addr", "deps"]
    );
    // `+` repeated across flags adds every one.
    assert_eq!(
        selected(&["--fields", "+driver", "--fields", "+deps"]),
        ["addr", "labels", "driver", "deps"]
    );
    assert!(args(&["--fields", "deps"]).wants(Row::DEPS));
    assert!(!args(&["--fields", "deps"]).wants(Row::HASHIN));
    assert!(!args(&["--json"]).wants(Row::HASHIN));

    // An empty name (`a,,b`, a trailing comma, `--fields=`) is refused.
    for flags in [
        &["--fields", "addr,,labels"][..],
        &["--fields", "addr,"],
        &["--fields="],
    ] {
        let err = parse(flags)
            .err()
            .unwrap_or_else(|| panic!("{flags:?} parsed"));
        assert_eq!(err.exit_code(), 2, "{flags:?}");
        assert!(
            err.to_string().contains("a value is required"),
            "{flags:?}: {err}"
        );
    }
    // Each of these exits 2 with clap's message.
    for (flags, mention) in [
        (&["--fields", "Addr"][..], "addr"),
        (&["--fields", "dep"], "deps"),
        (&["--fields", "adr"], "addr"),
        (&["--fields", "ädr"], "ädr"),
        (&["--fields", "+nope"], "nope"),
    ] {
        let err = parse(flags)
            .err()
            .unwrap_or_else(|| panic!("{flags:?} parsed"));
        assert_eq!(err.kind(), ErrorKind::InvalidValue, "{flags:?}");
        assert_eq!(err.exit_code(), 2, "{flags:?}");
        let message = err.to_string();
        assert!(message.contains(mention), "{flags:?}: {message}");
    }
    // A typo is answered with the nearest name.
    let message = parse(&["--fields", "dep"]).err().unwrap().to_string();
    assert!(message.contains("similar"), "{message}");
}

#[test]
fn fields_mixing_bare_and_plus_names_is_refused() {
    // In one value, and across repeated flags: the union of every `--fields`
    // must be all bare or all `+`.
    for flags in [
        &["--fields", "deps,+driver"][..],
        &["--fields", "+driver,deps"],
        &["--fields", "deps", "--fields", "+driver"],
        &["--fields", "+driver", "--fields", "deps"],
    ] {
        let err = parse(flags)
            .err()
            .unwrap_or_else(|| panic!("{flags:?} parsed"));
        assert_eq!(err.kind(), ErrorKind::ArgumentConflict, "{flags:?}");
        assert_eq!(err.exit_code(), 2, "{flags:?}");
        let message = err.to_string();
        assert!(
            message.contains("mixes replacing (`deps`) and adding (`+driver`) names"),
            "{flags:?}: {message}"
        );
    }
    let message = parse(&["--fields", "deps,+driver,deps"])
        .err()
        .unwrap()
        .to_string();
    assert!(
        message.contains(
            "write `--fields deps,driver` for exactly those fields, \
             or `--fields +deps,+driver` to add them to the default set"
        ),
        "{message}"
    );
    // `all` and `help` are neither: they combine with either form.
    for flags in [
        &["--fields", "all,+driver"][..],
        &["--fields", "deps,all"],
        &["--fields", "help,+driver"],
    ] {
        if let Err(err) = parse(flags) {
            panic!("{flags:?}: {err}");
        }
    }
}

#[test]
fn fields_help_prints_the_table() {
    use clap::{Args as _, FromArgMatches as _};
    let cmd = clap::Command::new("heph")
        .subcommand(JsonArgs::<Row>::augment_args(clap::Command::new("query")));
    let matches = cmd
        .clone()
        .try_get_matches_from(["heph", "query", "--fields", "help"])
        .unwrap();
    let table = fields_help(&matches).unwrap();
    assert!(table.contains("heph query --json"), "{table}");
    for f in Row::FIELDS {
        assert!(table.contains(f.name), "{table}");
    }
    assert!(table.contains("* addr"), "{table}");
    assert!(table.contains("  hashin  built"), "{table}");
    // Not asked: nothing.
    let matches = cmd
        .try_get_matches_from(["heph", "query", "--fields", "addr"])
        .unwrap();
    assert!(fields_help(&matches).is_none());
    let (_, sub) = matches.subcommand().unwrap();
    JsonArgs::<Row>::from_arg_matches(sub).unwrap();
}

#[test]
fn help_lists_fields_with_tiers() {
    use clap::Args as _;
    let mut cmd = JsonArgs::<Row>::augment_args(clap::Command::new("t"));
    let help = cmd.render_long_help().to_string();
    assert!(help.contains("Output:"), "{help}");
    assert!(help.contains("[built] Its input hash."), "{help}");
    assert!(
        help.contains("[free, default] The target's address."),
        "{help}"
    );
}

#[test]
fn bad_jq_is_a_parse_error() {
    let err = parse(&["--jq", ".foo | bar"]).err().unwrap();
    assert_eq!(err.exit_code(), 2);
    let message = err.to_string();
    assert!(message.contains("unknown function `bar/0`"), "{message}");
    assert!(message.contains("column 8"), "{message}");
    assert_eq!(parse(&["--jq", "env"]).err().unwrap().exit_code(), 2);
}

#[test]
fn output_group_conflicts() {
    #[derive(clap::Parser, Debug)]
    struct Cmd {
        #[arg(long, conflicts_with = "output")]
        shell: bool,
        #[command(flatten)]
        out: JsonArgs<Row>,
    }
    use clap::Parser as _;
    for flag in [&["--json"][..], &["--fields", "addr"], &["--jq", "."]] {
        let mut argv = vec!["t", "--shell"];
        argv.extend_from_slice(flag);
        let err = Cmd::try_parse_from(argv).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ArgumentConflict, "{flag:?}");
    }
    let ok = Cmd::try_parse_from(["t", "--json", "--fields", "addr", "--jq", "."]).unwrap();
    assert!(ok.out.enabled() && !ok.shell);
}
