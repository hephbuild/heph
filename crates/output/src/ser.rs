//! `serialize_with` helpers for record fields.
//!
//! serde serializes a non-UTF-8 `Path` as an error that does not name it.
//! These fail naming the path (lossily, in the message only), so the command
//! fails instead of printing a lossy path a script would then act on.
//!
//! ```
//! # use std::path::PathBuf;
//! #[derive(serde::Serialize, output::Record)]
//! struct Row {
//!     #[serde(serialize_with = "output::ser::path")]
//!     log: PathBuf,
//!     #[serde(serialize_with = "output::ser::paths")]
//!     outputs: Vec<PathBuf>,
//! }
//! ```

use serde::Serializer;
use serde::ser::{Error as _, SerializeSeq as _};
use std::path::Path;

/// A path as a JSON string; an error naming it when it is not UTF-8.
pub fn path<P: AsRef<Path>, S: Serializer>(path: &P, s: S) -> Result<S::Ok, S::Error> {
    let path = path.as_ref();
    match path.to_str() {
        Some(text) => s.serialize_str(text),
        None => Err(S::Error::custom(format_args!(
            "the path {} is not valid UTF-8, so it cannot be printed as JSON",
            path.display()
        ))),
    }
}

/// An optional path: `null`, or [`path`].
pub fn opt_path<P: AsRef<Path>, S: Serializer>(p: &Option<P>, s: S) -> Result<S::Ok, S::Error> {
    match p {
        Some(p) => path(p, s),
        None => s.serialize_none(),
    }
}

/// A list of paths, each as [`path`].
pub fn paths<I, S>(ps: I, s: S) -> Result<S::Ok, S::Error>
where
    I: IntoIterator,
    I::Item: AsRef<Path>,
    S: Serializer,
{
    let mut seq = s.serialize_seq(None)?;
    for p in ps {
        seq.serialize_element(&Utf8Path(p.as_ref()))?;
    }
    seq.end()
}

struct Utf8Path<'a>(&'a Path);

impl serde::Serialize for Utf8Path<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        path(&self.0, s)
    }
}

#[cfg(test)]
mod tests {
    use crate::Selection;
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::PathBuf;

    #[derive(serde::Serialize, crate::Record)]
    struct Row {
        #[serde(serialize_with = "crate::ser::path")]
        log_path: PathBuf,
        #[serde(serialize_with = "crate::ser::opt_path")]
        out_path: Option<PathBuf>,
        #[serde(serialize_with = "crate::ser::paths")]
        all_paths: Vec<PathBuf>,
    }

    fn print(row: &Row) -> anyhow::Result<String> {
        let args = crate::JsonArgs::<Row>::default();
        let mut out = Vec::new();
        args.printer_to(&mut out, crate::Style::Compact)
            .record(row, &crate::never_cancelled)?;
        Ok(String::from_utf8(out)?)
    }

    #[test]
    fn utf8_paths_print_as_strings() {
        let row = Row {
            log_path: "/a/log.txt".into(),
            out_path: None,
            all_paths: vec!["x".into(), "y/z".into()],
        };
        assert_eq!(
            print(&row).unwrap(),
            "{\"log_path\":\"/a/log.txt\",\"out_path\":null,\"all_paths\":[\"x\",\"y/z\"]}\n"
        );
        assert_eq!(
            Selection::defaults(<Row as crate::Record>::FIELDS),
            Selection::all(<Row as crate::Record>::FIELDS)
        );
    }

    #[test]
    fn non_utf8_path_is_an_error() {
        let bad = PathBuf::from(std::ffi::OsStr::from_bytes(b"/out/caf\xe9"));
        for row in [
            Row {
                log_path: bad.clone(),
                out_path: None,
                all_paths: vec![],
            },
            Row {
                log_path: "ok".into(),
                out_path: Some(bad.clone()),
                all_paths: vec![],
            },
            Row {
                log_path: "ok".into(),
                out_path: None,
                all_paths: vec!["fine".into(), bad.clone()],
            },
        ] {
            let err = print(&row).unwrap_err();
            let message = format!("{err:#}");
            assert!(message.contains("/out/caf"), "names the path: {message}");
            assert!(message.contains("not valid UTF-8"), "{message}");
        }
    }
}
