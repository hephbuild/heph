//! `record_derive`: what `#[derive(Record)]` accepts. What it refuses is in
//! `src/derive_refusals.rs`, as `compile_fail` doctests.

use output::{Record, Tier};

#[derive(serde::Serialize, Record)]
#[serde(rename_all = "camelCase")]
struct Renamed {
    /// Kept as serde names it.
    exit_code: i32,
    #[serde(rename = "out")]
    #[record(built)]
    outputs: Vec<String>,
    #[serde(skip)]
    #[expect(dead_code, reason = "skipped by serde: not a field")]
    hidden: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[record(opt_in)]
    note: Option<String>,
}

#[test]
fn rename_and_rename_all_are_honoured() {
    let names: Vec<_> = Renamed::FIELDS.iter().map(|f| f.name).collect();
    assert_eq!(names, ["exitCode", "out", "note"]);
    assert_eq!(Renamed::FIELDS[0].doc, "Kept as serde names it.");
    assert_eq!(Renamed::FIELDS[0].kind, "number");
    assert_eq!(Renamed::FIELDS[1].kind, "[string]");
    assert_eq!(Renamed::FIELDS[2].kind, "string?");
    assert_eq!(
        Renamed::__OUTPUT_SHAPE,
        [
            (".exitCode", "number"),
            (".out", "[string]"),
            (".note", "string?")
        ]
    );
    assert_eq!(Renamed::FIELDS[1].tier, Tier::Built);
    assert!(!Renamed::FIELDS[1].default);
    assert_eq!(Renamed::FIELDS[2].tier, Tier::Free);
    assert!(!Renamed::FIELDS[2].default);
    assert_eq!(Renamed::OUTPUTS.index(), 1);
    let value = Renamed {
        exit_code: 1,
        outputs: vec![],
        hidden: 0,
        note: None,
    };
    output::check_keys(&value).unwrap();
}

/// `derive(Shape)` lists what serde writes for each representation the field
/// snapshot meets: a struct, a unit-only enum, an externally tagged enum with
/// data, an internally tagged enum; borrowed and wrapped types read as their
/// JSON kind.
#[test]
fn shape_lists_the_serialized_paths() {
    #![expect(dead_code, reason = "only the derived constants are read")]

    #[derive(serde::Serialize, output::Shape)]
    struct Step<'a> {
        #[serde(rename = "where")]
        at: &'a std::path::Path,
        count: Option<Box<u64>>,
        names: &'a [std::borrow::Cow<'a, str>],
        nested: Vec<Unit>,
        #[serde(skip)]
        hidden: u8,
    }
    #[derive(serde::Serialize, output::Shape)]
    #[serde(rename_all = "snake_case")]
    enum Unit {
        UpToDate,
        Stale,
    }
    #[derive(serde::Serialize, output::Shape)]
    #[serde(rename_all = "snake_case")]
    enum External {
        Done,
        Moved(String),
        Failed { code: i32 },
    }
    #[derive(serde::Serialize, output::Shape)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    enum Internal {
        Idle,
        Lock { path: String },
    }

    assert_eq!(
        Step::__OUTPUT_SHAPE,
        [
            (".where", "string"),
            (".count", "number?"),
            (".names", "[string]"),
            (".nested", "[@Unit]"),
        ]
    );
    assert_eq!(
        Unit::__OUTPUT_SHAPE,
        [("", "\"up_to_date\""), ("", "\"stale\"")]
    );
    assert_eq!(
        External::__OUTPUT_SHAPE,
        [
            ("", "\"done\""),
            (".moved", "string"),
            (".failed.code", "number")
        ]
    );
    assert_eq!(
        Internal::__OUTPUT_SHAPE,
        [
            (".kind", "\"idle\""),
            (".kind", "\"lock\""),
            ("{kind=lock}.path", "string")
        ]
    );
}
