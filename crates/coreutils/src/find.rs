//! `find` and `xargs`, from [uutils/findutils].
//!
//! Both are pure adapters. The one thing they have to decide is what to do
//! about encoding: findutils' entry points take `&[&str]`, while an applet is
//! handed `OsString`s because a filename is not required to be UTF-8. Rather
//! than lose the bytes silently, a non-UTF-8 argument is refused by name.
//!
//! [uutils/findutils]: https://github.com/uutils/findutils

use std::ffi::OsString;

/// Exit code for a usage error, which is what both of these report on bad input.
const USAGE_ERROR: i32 = 1;

/// Borrow `argv` as UTF-8, or explain which argument could not be.
///
/// GNU `find` accepts arbitrary bytes in a path; this cannot, because the
/// upstream entry point does not. Saying so with the offending argument beats
/// a lossy conversion that would search the wrong path.
fn as_strs(argv: &[OsString]) -> Result<Vec<&str>, i32> {
    let mut out = Vec::with_capacity(argv.len());
    for arg in argv {
        match arg.to_str() {
            Some(s) => out.push(s),
            None => {
                eprintln!(
                    "{}: argument is not valid UTF-8: {:?} — heph's builtin cannot take \
                     arbitrary bytes here, unlike GNU's",
                    argv.first().and_then(|a| a.to_str()).unwrap_or("find"),
                    arg
                );
                return Err(USAGE_ERROR);
            }
        }
    }
    Ok(out)
}

pub fn find(argv: Vec<OsString>) -> i32 {
    let args = match as_strs(&argv) {
        Ok(a) => a,
        Err(code) => return code,
    };
    let deps = findutils::find::StandardDependencies::new();
    findutils::find::find_main(&args, &deps)
}

pub fn xargs(argv: Vec<OsString>) -> i32 {
    let args = match as_strs(&argv) {
        Ok(a) => a,
        Err(code) => return code,
    };
    findutils::xargs::xargs_main(&args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_arguments_pass_through() {
        let argv = vec![OsString::from("find"), OsString::from(".")];
        assert_eq!(as_strs(&argv).expect("utf-8"), vec!["find", "."]);
    }

    #[test]
    fn a_non_utf8_argument_is_refused_rather_than_mangled() {
        // Losing the bytes here would search a *different* path than the one
        // asked for, which is worse than refusing.
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt as _;
            let argv = vec![
                OsString::from("find"),
                OsString::from_vec(vec![0x66, 0x80, 0x6f]),
            ];
            assert_eq!(as_strs(&argv).expect_err("must refuse"), USAGE_ERROR);
        }
    }
}
