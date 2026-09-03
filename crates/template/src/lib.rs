//! The template renderer, shared by the `template` driver and the `tmpl` applet.
//!
//! Small enough to be tempting to copy, and exactly the wrong thing to copy:
//! the two safety properties below are *configuration*, and configuration that
//! exists in two places drifts. A loader enabled on one side and not the other
//! would be a sandbox hole nobody notices.
//!
//! * **A template cannot read an undeclared file.** The environment is built
//!   with no loader, so `{% include %}` and `{% import %}` have nothing to
//!   resolve against and fail rather than reaching into the filesystem.
//! * **An undefined variable is an error that names itself.** Undefined
//!   behaviour is strict, and the referenced variables are checked against what
//!   was supplied *before* rendering, because minijinja's own message is
//!   `undefined value (in template:1)` — it says something is missing without
//!   saying what, which is the least useful thing an error about a typo can do.

use anyhow::Context as _;
use std::collections::BTreeMap;

/// Bumped when rendering changes — a minijinja upgrade that alters output, or a
/// change to the environment configured here.
///
/// Consumers fold this into their cache key: the rendered bytes are a function
/// of the renderer as well as of the template, so a renderer that moved without
/// moving the key would keep serving the old rendering forever.
pub const TEMPLATE_FORMAT_VERSION: u32 = 2;

/// Render `template` with `vars`.
///
/// `vars` is a `BTreeMap` rather than a `HashMap` on purpose: callers hash it
/// into a cache key, and a `HashMap`'s iteration order is randomized per
/// process.
pub fn render(template: &str, vars: &BTreeMap<String, String>) -> anyhow::Result<String> {
    let mut env = minijinja::Environment::new();
    env.set_undefined_behavior(minijinja::UndefinedBehavior::Strict);
    // minijinja drops a template's final newline by default. These render
    // *files*, and a config file that has lost its trailing newline is wrong —
    // POSIX text files end with one, and diffs against them get noisy.
    env.set_keep_trailing_newline(true);
    env.add_template("template", template)
        .context("compile template")?;
    let tmpl = env.get_template("template").context("load template")?;

    let missing = missing_variables(&tmpl, vars);
    if !missing.is_empty() {
        let supplied: Vec<&str> = vars.keys().map(String::as_str).collect();
        anyhow::bail!(
            "template uses {} that `vars` does not supply: {}. Supplied: {}",
            if missing.len() == 1 {
                "a variable"
            } else {
                "variables"
            },
            missing.join(", "),
            if supplied.is_empty() {
                "nothing".to_string()
            } else {
                supplied.join(", ")
            },
        );
    }

    tmpl.render(minijinja::Value::from_serialize(vars))
        .context("render template")
}

/// The variables `tmpl` references that `vars` does not supply.
///
/// Compared on the *root* of a dotted path: minijinja reports
/// `{{ cfg.port }}` as the undeclared name `cfg.port`, so comparing the whole
/// path would reject every template that reads a field or calls a method.
/// Loop-bound names are not reported by minijinja at all.
fn missing_variables(
    tmpl: &minijinja::Template<'_, '_>,
    vars: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut m: Vec<String> = tmpl
        .undeclared_variables(true)
        .into_iter()
        .map(|name| name.split('.').next().unwrap_or(&name).to_string())
        .filter(|root| !vars.contains_key(root))
        .collect();
    m.sort_unstable();
    m.dedup();
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn the_trailing_newline_survives() {
        // These render files. A config file that lost its final newline is
        // wrong, and every diff against it is noisy.
        assert_eq!(render("a\n", &vars(&[])).expect("render"), "a\n");
    }

    #[test]
    fn renders_declared_variables() {
        assert_eq!(
            render(
                "hello {{ name }} x{{ n }}",
                &vars(&[("name", "world"), ("n", "3")])
            )
            .expect("render"),
            "hello world x3"
        );
    }

    #[test]
    fn an_undefined_variable_is_an_error_that_names_itself() {
        let err = render("port = {{ prot }}", &vars(&[("port", "80")])).expect_err("must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("prot"),
            "the error must name the variable: {msg}"
        );
        assert!(msg.contains("port"), "and say what was supplied: {msg}");
    }

    #[test]
    fn a_template_cannot_reach_the_filesystem() {
        // No loader is configured, so `include` has nothing to resolve against.
        let err = render("{% include '/etc/passwd' %}", &vars(&[])).expect_err("must fail");
        assert!(
            !format!("{err:#}").contains("root:"),
            "a template must never read a host file"
        );
    }

    #[test]
    fn attribute_access_checks_the_root_variable() {
        let err = render("{{ cfg.port }}", &vars(&[("cfg", "x")])).expect_err("strict mode");
        assert!(
            !format!("{err:#}").contains("does not supply"),
            "the root is supplied, so this is not a missing variable"
        );
    }

    #[test]
    fn a_loop_variable_is_not_reported_as_missing() {
        assert_eq!(
            render(
                "{% for item in items %}[{{ item }}]{% endfor %}",
                &vars(&[("items", "ab")])
            )
            .expect("render"),
            "[a][b]"
        );
    }
}
