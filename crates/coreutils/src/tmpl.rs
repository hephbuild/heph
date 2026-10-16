//! `tmpl` — the template renderer as a command.
//!
//! The `template` rule is the surface to reach for: its inputs and outputs are
//! declared, and it needs no subprocess. This exists for the case the rule
//! cannot express — filling in a file *during* a recipe, from values the recipe
//! computed — which is otherwise done with `sed -i` or `envsubst`, neither of
//! which behaves the same on both hosts.
//!
//! Rendering and its safety properties come from `crates/template`, the same
//! code the rule uses, so the two cannot disagree about whether a template may
//! read `/etc/passwd`.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::PathBuf;

const OK: i32 = 0;
const ERROR: i32 = 1;

#[derive(Debug, Default)]
struct Options {
    input: Option<PathBuf>,
    output: Option<PathBuf>,
    vars: BTreeMap<String, String>,
    env: bool,
}

fn usage() -> &'static str {
    "usage: tmpl [-i IN] [-o OUT] [--set NAME=VALUE]... [--env]\n\
     \n\
     Renders a template, reading stdin and writing stdout by default.\n\
     --set supplies one variable; --env supplies every environment variable.\n\
     An undefined variable is an error naming it, never an empty string."
}

fn parse(argv: &[OsString]) -> Result<Options, String> {
    let mut o = Options::default();
    let mut it = argv.iter().skip(1);
    while let Some(raw) = it.next() {
        let arg = raw.to_string_lossy().into_owned();
        match arg.as_str() {
            "-h" | "--help" => return Err(usage().to_string()),
            "-i" | "--input" => match it.next() {
                Some(v) => o.input = Some(PathBuf::from(v)),
                None => return Err("-i needs a path".to_string()),
            },
            "-o" | "--output" => match it.next() {
                Some(v) => o.output = Some(PathBuf::from(v)),
                None => return Err("-o needs a path".to_string()),
            },
            "--env" => o.env = true,
            "--set" => match it.next() {
                Some(v) => {
                    let pair = v.to_string_lossy().into_owned();
                    match pair.split_once('=') {
                        Some((k, val)) => {
                            o.vars.insert(k.to_string(), val.to_string());
                        }
                        None => {
                            return Err(format!("--set needs NAME=VALUE, got {pair:?}"));
                        }
                    }
                }
                None => return Err("--set needs NAME=VALUE".to_string()),
            },
            other => return Err(format!("unexpected argument {other:?}\n{}", usage())),
        }
    }
    Ok(o)
}

/// Collect the variables, with `--set` winning over `--env`.
///
/// That order is the useful one: `--env` is the broad default and each `--set`
/// is a deliberate override of it.
fn collect_vars(o: &Options) -> BTreeMap<String, String> {
    let mut vars = BTreeMap::new();
    if o.env {
        for (k, v) in std::env::vars() {
            vars.insert(k, v);
        }
    }
    for (k, v) in &o.vars {
        vars.insert(k.clone(), v.clone());
    }
    vars
}

pub fn main(argv: Vec<OsString>) -> i32 {
    let o = match parse(&argv) {
        Ok(o) => o,
        Err(msg) => {
            if msg == usage() {
                println!("{msg}");
                return OK;
            }
            eprintln!("tmpl: {msg}");
            return ERROR;
        }
    };

    let res = (|| -> anyhow::Result<()> {
        let mut template = String::new();
        match &o.input {
            Some(path) => template = std::fs::read_to_string(path)?,
            None => {
                std::io::stdin().read_to_string(&mut template)?;
            }
        }
        let rendered = htemplate::render(&template, &collect_vars(&o))?;
        match &o.output {
            Some(path) => std::fs::write(path, rendered)?,
            None => {
                let stdout = std::io::stdout();
                let mut w = stdout.lock();
                w.write_all(rendered.as_bytes())?;
                w.flush()?;
            }
        }
        Ok(())
    })();

    match res {
        Ok(()) => OK,
        Err(e) => {
            eprintln!("tmpl: {e:#}");
            ERROR
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    #[test]
    fn set_parses_name_equals_value() {
        let o = parse(&argv(&["tmpl", "--set", "a=1", "--set", "b=2"])).expect("parse");
        assert_eq!(o.vars.get("a").map(String::as_str), Some("1"));
        assert_eq!(o.vars.get("b").map(String::as_str), Some("2"));
    }

    #[test]
    fn a_value_may_contain_equals_signs() {
        // `--set url=a=b` is a value with an `=` in it, not a malformed pair.
        let o = parse(&argv(&["tmpl", "--set", "url=a=b"])).expect("parse");
        assert_eq!(o.vars.get("url").map(String::as_str), Some("a=b"));
    }

    #[test]
    fn set_without_a_pair_is_refused() {
        let err = parse(&argv(&["tmpl", "--set", "nope"])).expect_err("must refuse");
        assert!(err.contains("NAME=VALUE"), "{err}");
    }

    #[test]
    fn set_overrides_env() {
        // `--env` is the broad default; each `--set` is a deliberate override.
        let o = Options {
            env: true,
            vars: BTreeMap::from([("PATH".to_string(), "overridden".to_string())]),
            ..Options::default()
        };
        assert_eq!(
            collect_vars(&o).get("PATH").map(String::as_str),
            Some("overridden")
        );
    }

    #[test]
    fn rendering_goes_through_the_shared_renderer() {
        // The point of the shared crate: the applet and the rule cannot
        // disagree about undefined variables.
        let vars = BTreeMap::from([("a".to_string(), "1".to_string())]);
        assert_eq!(htemplate::render("{{ a }}", &vars).expect("render"), "1");
        htemplate::render("{{ b }}", &vars).expect_err("undefined must fail");
    }
}
