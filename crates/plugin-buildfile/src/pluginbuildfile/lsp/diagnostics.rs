//! Static validation of `target(...)` and `provider_state(...)` keyword arguments
//! against the engine's driver / provider-state schemas, surfaced as LSP
//! diagnostics (the editor's red squiggle) on the offending key or value.
//!
//! This runs from [`super::context::HephLspContext::parse_file_with_contents`]
//! whenever the buffer parses — it walks the AST, so it needs precise source
//! spans, but it does not need the buffer to *evaluate* (it works mid-edit, the
//! moment a bad key is typed, exactly like the textual completion does).
//!
//! Two checks per recognized call:
//! - **Wrong type** — a keyword argument whose value is a literal that doesn't
//!   match the field's declared [`ParamType`]. Non-literal values (variables,
//!   concatenations, calls) carry no statically-known type, so they're left alone.
//! - **Unknown key** — a keyword argument that names no known field. Only emitted
//!   when the *complete* set of valid keys is known: the driver / provider is
//!   resolved from a string-literal `driver=` / `provider=` argument and its schema
//!   is available. Otherwise the key might legitimately belong to an unresolved
//!   schema, so we stay silent rather than risk a false positive.

use crate::pluginbuildfile::run_file::target_base_fields;
use hcore::htvalue::Value;
use hcore::htvalue::signature::ParamType;
use hplugin::lsp::LspEngine;
use lsp_types::{Diagnostic, DiagnosticSeverity, Range};
use starlark::codemap::Span;
use starlark::syntax::AstModule;
use starlark::syntax::ast::{Argument, AstArgument, AstExpr, AstLiteral, Expr};
use std::collections::HashMap;

/// Which recognized builtin a call site is.
enum Callee {
    Target,
    ProviderState,
}

/// Validate every `target(...)` / `provider_state(...)` call in `ast`, returning a
/// diagnostic per invalid keyword argument (unknown key or type mismatch).
pub(crate) fn validate(ast: &AstModule, engine: &dyn LspEngine) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    // `Stmt::visit_expr` yields each top-level expression across all nested
    // statements; `walk_expr` then descends into sub-expressions, so nested calls
    // (e.g. inside a list or another call) are covered too.
    ast.statement().visit_expr(|top| {
        walk_expr(top, &mut |e| {
            if let Expr::Call(callee, args) = &e.node
                && let Expr::Identifier(id) = &callee.node
            {
                let callee = match id.node.ident.as_str() {
                    "target" => Callee::Target,
                    "provider_state" => Callee::ProviderState,
                    _ => return,
                };
                check_call(ast, engine, &args.args, callee, &mut out);
            }
        });
    });
    out
}

/// Visit `e` and every expression nested within it.
fn walk_expr<'a>(e: &'a AstExpr, f: &mut impl FnMut(&'a AstExpr)) {
    f(e);
    // `Expr::visit_expr` is one level deep; recurse to reach the whole subtree.
    e.visit_expr(|child| walk_expr(child, f));
}

fn check_call(
    ast: &AstModule,
    engine: &dyn LspEngine,
    args: &[AstArgument],
    callee: Callee,
    out: &mut Vec<Diagnostic>,
) {
    // The known fields (name → type) and whether that set is exhaustive.
    let mut known: Vec<(String, ParamType)> = Vec::new();
    let complete;
    let ctx;
    match callee {
        Callee::Target => {
            known.extend(target_base_fields().into_iter().map(|f| (f.name, f.ty)));
            // Driver-specific config fields are only known once the driver is
            // resolved from a string-literal `driver=`. Without it (or with an
            // unknown driver) any extra key might be a valid driver field.
            match named_str_literal(args, "driver")
                .and_then(|d| engine.driver_schema(&d).map(|s| (d, s)))
            {
                Some((driver, schema)) => {
                    known.extend(schema.fields.into_iter().map(|f| (f.name, f.ty)));
                    complete = true;
                    ctx = format!("`target` or the `{driver}` driver");
                }
                None => {
                    complete = false;
                    ctx = String::new();
                }
            }
        }
        Callee::ProviderState => {
            // `provider` is always a valid (string) key.
            known.push(("provider".to_string(), ParamType::String));
            match named_str_literal(args, "provider")
                .and_then(|p| engine.provider_state_schema(&p).map(|s| (p, s)))
            {
                Some((provider, schema)) => {
                    known.extend(schema.fields.into_iter().map(|f| (f.name, f.ty)));
                    complete = true;
                    ctx = format!("the `{provider}` provider");
                }
                None => {
                    complete = false;
                    ctx = String::new();
                }
            }
        }
    }

    for arg in args {
        let Argument::Named(name, value) = &arg.node else {
            continue;
        };
        let key = name.node.as_str();
        match known.iter().find(|(n, _)| n == key) {
            // A known field: flag a literal value whose type doesn't match.
            Some((_, ty)) => {
                if let Some(v) = literal_value(value)
                    && !value_matches(ty, &v)
                {
                    out.push(diag(
                        ast,
                        value.span,
                        format!("`{key}` expects {}, got {}", ty.render(), value_kind(&v)),
                    ));
                }
            }
            // An unknown key — only an error when the valid set is exhaustive.
            None if complete => {
                out.push(diag(
                    ast,
                    name.span,
                    format!("unknown field `{key}` for {ctx}"),
                ));
            }
            None => {}
        }
    }
}

/// The string value of the `key = "…"` keyword argument, when present with a
/// string-literal value; `None` otherwise (missing, or a non-literal value whose
/// driver/provider we can't statically resolve).
fn named_str_literal(args: &[AstArgument], key: &str) -> Option<String> {
    args.iter().find_map(|a| match &a.node {
        Argument::Named(name, value) if name.node == key => match &value.node {
            Expr::Literal(AstLiteral::String(s)) => Some(s.node.clone()),
            _ => None,
        },
        _ => None,
    })
}

/// The [`Value`] of a literal expression, or `None` when the expression isn't a
/// statically-evaluable literal (a variable, call, concatenation, f-string, …).
/// Numeric values are placeholders — only the *kind* matters for type-checking,
/// except a leading minus, which is tracked so a uint field rejects it.
/// Mirrors `run_file::starlark_to_rust` so the kinds line up with what the engine
/// would produce at eval time.
fn literal_value(expr: &AstExpr) -> Option<Value> {
    match &expr.node {
        Expr::Literal(AstLiteral::String(_)) => Some(Value::String(String::new())),
        Expr::Literal(AstLiteral::Int(_)) => Some(Value::Int(0)),
        Expr::Literal(AstLiteral::Float(_)) => Some(Value::Float(0.0)),
        // A negated literal: a non-negative int/float becomes negative.
        Expr::Minus(inner) => match &inner.node {
            Expr::Literal(AstLiteral::Int(_)) => Some(Value::Int(-1)),
            Expr::Literal(AstLiteral::Float(_)) => Some(Value::Float(-1.0)),
            _ => None,
        },
        // `True` / `False` / `None` are identifiers in Starlark, not literals.
        Expr::Identifier(id) => match id.node.ident.as_str() {
            "True" => Some(Value::Bool(true)),
            "False" => Some(Value::Bool(false)),
            "None" => Some(Value::Null()),
            _ => None,
        },
        // A list is a literal only if every element is — otherwise its element
        // type can't be checked, so treat the whole thing as non-literal.
        Expr::List(items) => items
            .iter()
            .map(literal_value)
            .collect::<Option<Vec<_>>>()
            .map(Value::List),
        // A dict literal: string-literal keys (heph maps are string-keyed) with
        // literal values.
        Expr::Dict(pairs) => {
            let mut m = HashMap::with_capacity(pairs.len());
            for (k, v) in pairs {
                let key = match &k.node {
                    Expr::Literal(AstLiteral::String(s)) => s.node.clone(),
                    _ => return None,
                };
                m.insert(key, literal_value(v)?);
            }
            Some(Value::Map(m))
        }
        _ => None,
    }
}

/// Whether `v` satisfies `ty`. Like [`ParamType::matches`] but lenient on numeric
/// literals: an int literal satisfies a `uint` (when non-negative) or `float`
/// field, since Starlark has a single integer literal kind.
fn value_matches(ty: &ParamType, v: &Value) -> bool {
    match (ty, v) {
        (ParamType::Union(types), _) => types.iter().any(|t| value_matches(t, v)),
        (ParamType::Uint, Value::Int(i)) => *i >= 0,
        (ParamType::Float, Value::Int(_)) => true,
        (ParamType::List(inner), Value::List(items)) => {
            items.iter().all(|e| value_matches(inner, e))
        }
        (ParamType::Map(value), Value::Map(m)) => m.values().all(|e| value_matches(value, e)),
        (ParamType::Struct(fields), Value::Map(m)) => m.iter().all(|(k, v)| {
            fields
                .iter()
                .find(|f| &f.name == k)
                .is_some_and(|f| value_matches(&f.ty, v))
        }),
        _ => ty.matches(v),
    }
}

/// The value-kind name of `v`, for the "got …" half of a type-mismatch message.
fn value_kind(v: &Value) -> &'static str {
    match v {
        Value::String(_) => "string",
        Value::Bool(_) => "bool",
        Value::Int(_) => "int",
        Value::Uint(_) => "uint",
        Value::Float(_) => "float",
        Value::Null() => "null",
        Value::Map(_) => "map",
        Value::List(_) => "list",
    }
}

/// An error diagnostic over `span` with heph as the source (so the editor groups
/// it apart from the stock Starlark diagnostics).
fn diag(ast: &AstModule, span: Span, message: String) -> Diagnostic {
    let range: Range = ast.file_span(span).resolve_span().into();
    Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        source: Some("heph".to_string()),
        message,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::htvalue::signature::ParamType;
    use hplugin::driver::{DriverField, DriverSchema};
    use hplugin::provider::{StateField, StateSchema};
    use starlark::syntax::{AstModule, Dialect};
    use std::path::Path;

    /// An engine exposing one driver (`exec` with a required string `cmd` and an
    /// optional bool `verbose`) and one provider state schema (`go` with a bool
    /// `go_codegen_root`).
    struct FakeEngine;

    impl hplugin::lsp::LspEngine for FakeEngine {
        fn root(&self) -> &Path {
            Path::new("/ws")
        }
        fn provider_function_registry(
            &self,
        ) -> std::sync::Arc<hplugin::provider::ProviderFunctionRegistry> {
            std::sync::Arc::new(hplugin::provider::ProviderFunctionRegistry::default())
        }
        fn driver_schema(&self, name: &str) -> Option<DriverSchema> {
            (name == "exec").then(|| DriverSchema {
                fields: vec![
                    DriverField {
                        name: "cmd".to_string(),
                        ty: ParamType::String,
                        doc: String::new(),
                        required: true,
                    },
                    DriverField {
                        name: "verbose".to_string(),
                        ty: ParamType::Bool,
                        doc: String::new(),
                        required: false,
                    },
                ],
            })
        }
        fn driver_names(&self) -> Vec<String> {
            vec!["exec".to_string()]
        }
        fn provider_state_schema(&self, name: &str) -> Option<StateSchema> {
            (name == "go").then(|| StateSchema {
                fields: vec![StateField {
                    name: "go_codegen_root".to_string(),
                    ty: ParamType::Bool,
                    doc: String::new(),
                    required: false,
                }],
            })
        }
        fn provider_options(&self, _name: &str) -> hplugin::config::Options {
            Default::default()
        }
    }

    fn messages(content: &str) -> Vec<String> {
        let ast = AstModule::parse("BUILD", content.to_string(), &Dialect::Extended).unwrap();
        validate(&ast, &FakeEngine)
            .into_iter()
            .map(|d| d.message)
            .collect()
    }

    #[test]
    fn unknown_target_field_is_flagged_when_driver_resolves() {
        let msgs = messages("target(name = \"t\", driver = \"exec\", bogus = 1)\n");
        assert!(
            msgs.iter().any(|m| m.contains("unknown field `bogus`")),
            "expected unknown-field error, got {msgs:?}"
        );
    }

    #[test]
    fn known_target_and_driver_fields_are_not_flagged() {
        // `name`/`driver` are base fields; `cmd`/`verbose` are exec driver fields.
        let msgs = messages(
            "target(name = \"t\", driver = \"exec\", cmd = \"echo hi\", verbose = True)\n",
        );
        assert!(msgs.is_empty(), "expected no diagnostics, got {msgs:?}");
    }

    #[test]
    fn unknown_target_field_is_silent_when_driver_unresolved() {
        // No driver → the extra key could be a valid driver field, so stay silent.
        let msgs = messages("target(name = \"t\", maybe_driver_field = 1)\n");
        assert!(
            msgs.is_empty(),
            "must not guess unknown fields without a driver, got {msgs:?}"
        );
        // An unknown driver is equally unresolvable.
        let msgs = messages("target(name = \"t\", driver = \"nope\", x = 1)\n");
        assert!(msgs.is_empty(), "unknown driver → silent, got {msgs:?}");
    }

    #[test]
    fn wrong_type_on_driver_field_is_flagged() {
        // `cmd` is a string; an int literal is a type mismatch.
        let msgs = messages("target(name = \"t\", driver = \"exec\", cmd = 5)\n");
        assert!(
            msgs.iter()
                .any(|m| m.contains("`cmd` expects string, got int")),
            "expected type mismatch on cmd, got {msgs:?}"
        );
    }

    #[test]
    fn wrong_type_on_base_field_is_flagged_without_driver() {
        // `name` is a base field (string) — checkable even with no driver resolved.
        let msgs = messages("target(name = 1)\n");
        assert!(
            msgs.iter()
                .any(|m| m.contains("`name` expects string, got int")),
            "expected type mismatch on name, got {msgs:?}"
        );
    }

    #[test]
    fn non_literal_value_is_not_type_checked() {
        // A concatenation has no statically-known type → no false positive.
        let msgs = messages("target(name = PREFIX + \"_t\", driver = \"exec\")\n");
        assert!(
            msgs.is_empty(),
            "non-literal value must be skipped, got {msgs:?}"
        );
    }

    #[test]
    fn provider_state_unknown_field_is_flagged() {
        let msgs = messages("provider_state(provider = \"go\", bogus = True)\n");
        assert!(
            msgs.iter()
                .any(|m| m.contains("unknown field `bogus`") && m.contains("`go` provider")),
            "expected unknown provider-state field, got {msgs:?}"
        );
    }

    #[test]
    fn provider_state_wrong_type_is_flagged() {
        // `go_codegen_root` is a bool; a string literal is a mismatch.
        let msgs = messages("provider_state(provider = \"go\", go_codegen_root = \"yes\")\n");
        assert!(
            msgs.iter()
                .any(|m| m.contains("`go_codegen_root` expects bool, got string")),
            "expected type mismatch, got {msgs:?}"
        );
    }

    #[test]
    fn provider_state_valid_call_is_clean() {
        let msgs = messages("provider_state(provider = \"go\", go_codegen_root = True)\n");
        assert!(msgs.is_empty(), "expected no diagnostics, got {msgs:?}");
    }

    #[test]
    fn diagnostic_range_targets_the_offending_token() {
        // The unknown-key squiggle must land on the key, not the whole call.
        let content = "target(name = \"t\", driver = \"exec\", bogus = 1)\n";
        let ast = AstModule::parse("BUILD", content.to_string(), &Dialect::Extended).unwrap();
        let diags = validate(&ast, &FakeEngine);
        let d = diags
            .iter()
            .find(|d| d.message.contains("bogus"))
            .expect("bogus diagnostic");
        let start = content.find("bogus").unwrap() as u32;
        assert_eq!(d.range.start.line, 0);
        assert_eq!(d.range.start.character, start);
        assert_eq!(d.range.end.character, start + "bogus".len() as u32);
        assert_eq!(d.severity, Some(DiagnosticSeverity::ERROR));
    }

    #[test]
    fn list_with_non_literal_element_is_not_type_checked() {
        // labels accepts string | list[string]; a list containing a variable can't
        // be fully typed, so it must be skipped rather than mis-flagged.
        let msgs = messages("target(name = \"t\", driver = \"exec\", labels = [\"a\", X])\n");
        assert!(
            msgs.is_empty(),
            "partial list must be skipped, got {msgs:?}"
        );
    }

    #[test]
    fn wrong_type_list_field_is_flagged() {
        // labels is string | list[string]; an int literal matches neither.
        let msgs = messages("target(name = \"t\", driver = \"exec\", labels = 3)\n");
        assert!(
            msgs.iter().any(|m| m.contains("`labels` expects")),
            "expected labels type mismatch, got {msgs:?}"
        );
    }
}
