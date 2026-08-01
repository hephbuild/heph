//! Static validation of `target(...)` and `provider_state(...)` keyword arguments
//! against the engine's driver / provider-state schemas, surfaced as LSP
//! diagnostics (the editor's red squiggle) on the offending key or value.
//!
//! This runs from [`super::context::HephLspContext::parse_file_with_contents`]
//! whenever the buffer parses — it walks the AST, so it needs precise source
//! spans, but it does not need the buffer to *evaluate* (it works mid-edit, the
//! moment a bad key is typed, exactly like the textual completion does).
//!
//! Three checks per recognized call:
//! - **Wrong type** — a keyword argument whose value is a literal that doesn't
//!   match the field's declared [`ParamType`]. Non-literal values (variables,
//!   concatenations, calls) carry no statically-known type, so they're left alone.
//! - **Unknown key** — a keyword argument that names no known field. Only emitted
//!   when the *complete* set of valid keys is known: the driver / provider is
//!   resolved from a string-literal `driver=` / `provider=` argument and its schema
//!   is available. Otherwise the key might legitimately belong to an unresolved
//!   schema, so we stay silent rather than risk a false positive.
//! - **Missing required key** — a required field (a base field, or a driver/
//!   provider-schema field once resolved) with no keyword argument supplying
//!   it. Anchored on the call's callee token, since there's no key token to
//!   underline. Suppressed entirely when the call includes a `*args`/`**kwargs`
//!   splat — it could supply any key at runtime, so a "missing" field isn't
//!   necessarily missing.
//!
//! All three apply only to heph's *builtins*. A buffer that binds `target` or
//! `provider_state` itself — a `def`, an assignment, a `load(...)` import — is
//! calling its own function, whose signature is nothing the engine knows: a
//! `def target(name, **kwargs)` macro legitimately takes any key. Checking such
//! a call against the builtin's schema would underline valid code, so a
//! shadowed name is skipped entirely.

use crate::pluginbuildfile::run_file::target_base_fields;
use hcore::htvalue::Value;
use hcore::htvalue::signature::ParamType;
use hplugin::lsp::LspEngine;
use lsp_types::{Diagnostic, DiagnosticSeverity, Range};
use starlark::codemap::Span;
use starlark::syntax::AstModule;
use starlark::syntax::ast::{
    Argument, AssignTarget, AstArgument, AstAssignTarget, AstExpr, AstLiteral, AstStmt, Expr, Stmt,
};
use std::collections::{HashMap, HashSet};

/// Which recognized builtin a call site is.
enum Callee {
    Target,
    ProviderState,
}

/// Validate every `target(...)` / `provider_state(...)` call in `ast`, returning a
/// diagnostic per invalid keyword argument (unknown key or type mismatch).
pub(crate) fn validate(ast: &AstModule, engine: &dyn LspEngine) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let shadowed = bound_names(ast);
    // `Stmt::visit_expr` yields each top-level expression across all nested
    // statements; `walk_expr` then descends into sub-expressions, so nested calls
    // (e.g. inside a list or another call) are covered too.
    ast.statement().visit_expr(|top| {
        walk_expr(top, &mut |e| {
            if let Expr::Call(callee_expr, args) = &e.node
                && let Expr::Identifier(id) = &callee_expr.node
            {
                let name = id.node.ident.as_str();
                // The buffer's own `target`/`provider_state`, not heph's — its
                // signature is unknown here, so there is nothing to check against.
                if shadowed.contains(name) {
                    return;
                }
                let callee = match name {
                    "target" => Callee::Target,
                    "provider_state" => Callee::ProviderState,
                    _ => return,
                };
                check_call(ast, engine, callee_expr.span, &args.args, callee, &mut out);
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

/// Visit `s` and every statement nested within it.
fn walk_stmt<'a>(s: &'a AstStmt, f: &mut impl FnMut(&'a AstStmt)) {
    f(s);
    // `Stmt::visit_stmt` is one level deep; recurse to reach the whole subtree.
    s.visit_stmt(|child| walk_stmt(child, f));
}

/// Every name the module binds: `def`s and their parameters, assignment and loop
/// targets, and `load(...)` imports.
///
/// Deliberately flat — a name bound anywhere counts as bound everywhere, with no
/// scope tracking. Over-approximating errs toward silence, which is this
/// module's rule for anything it cannot resolve exactly, and the alternative
/// (a real scope analysis) would buy nothing: a buffer that defines its own
/// `target` almost certainly means it at every call site.
fn bound_names(ast: &AstModule) -> HashSet<&str> {
    let mut names = HashSet::new();
    walk_stmt(ast.statement(), &mut |s| match &s.node {
        Stmt::Def(def) => {
            names.insert(def.name.node.ident.as_str());
            names.extend(
                def.params
                    .iter()
                    .filter_map(|p| p.ident())
                    .map(|i| i.node.ident.as_str()),
            );
        }
        Stmt::Load(load) => {
            names.extend(load.args.iter().map(|a| a.local.node.ident.as_str()));
        }
        Stmt::Assign(assign) => collect_assign_target(&assign.lhs, &mut names),
        Stmt::AssignModify(target, _, _) => collect_assign_target(target, &mut names),
        Stmt::For(for_stmt) => collect_assign_target(&for_stmt.var, &mut names),
        _ => {}
    });
    names
}

/// Add the identifiers an assignment or loop target binds (`a`, `a, b`, `[a, b]`).
/// Index and attribute targets (`a[0] = …`, `a.b = …`) mutate an existing
/// binding rather than introducing one, so they bind nothing.
fn collect_assign_target<'a>(target: &'a AstAssignTarget, names: &mut HashSet<&'a str>) {
    match &target.node {
        AssignTarget::Identifier(id) => {
            names.insert(id.node.ident.as_str());
        }
        AssignTarget::Tuple(items) => {
            for item in items {
                collect_assign_target(item, names);
            }
        }
        AssignTarget::Index(_) | AssignTarget::Dot(_, _) => {}
    }
}

fn check_call(
    ast: &AstModule,
    engine: &dyn LspEngine,
    callee_span: Span,
    args: &[AstArgument],
    callee: Callee,
    out: &mut Vec<Diagnostic>,
) {
    // The known fields (name, type, required). Base fields (`target`'s
    // `name`/`driver`/…, or `provider_state`'s `provider`) are always pushed
    // first; `base_count` marks where they end and driver/provider-schema
    // fields begin, so a missing-required message can attribute a field to
    // the right source without formatting/cloning a label for every field up
    // front — only the (at most one) field that's actually missing needs it.
    let mut known: Vec<(String, ParamType, bool)> = Vec::new();
    let base_label: &str;
    let base_count: usize;
    let mut schema_label: Option<String> = None;
    let complete;
    let ctx;
    match callee {
        Callee::Target => {
            base_label = "`target`";
            known.extend(
                target_base_fields()
                    .into_iter()
                    .map(|f| (f.name, f.ty, f.required)),
            );
            base_count = known.len();
            // Driver-specific config fields are only known once the driver is
            // resolved from a string-literal `driver=`. Without it (or with an
            // unknown driver) any extra key might be a valid driver field, and
            // we can't tell whether the driver's required fields are missing.
            match named_str_literal(args, "driver")
                .and_then(|d| engine.driver_schema(&d).map(|s| (d, s)))
            {
                Some((driver, schema)) => {
                    known.extend(
                        schema
                            .fields
                            .into_iter()
                            .map(|f| (f.name, f.ty, f.required)),
                    );
                    complete = true;
                    ctx = format!("`target` or the `{driver}` driver");
                    schema_label = Some(format!("the `{driver}` driver"));
                }
                None => {
                    complete = false;
                    ctx = String::new();
                }
            }
        }
        Callee::ProviderState => {
            base_label = "`provider_state`";
            // `provider` is always a valid (string) key, and required.
            known.push(("provider".to_string(), ParamType::String, true));
            base_count = known.len();
            match named_str_literal(args, "provider")
                .and_then(|p| engine.provider_state_schema(&p).map(|s| (p, s)))
            {
                Some((provider, schema)) => {
                    known.extend(
                        schema
                            .fields
                            .into_iter()
                            .map(|f| (f.name, f.ty, f.required)),
                    );
                    complete = true;
                    ctx = format!("the `{provider}` provider");
                    schema_label = Some(format!("the `{provider}` provider"));
                }
                None => {
                    complete = false;
                    ctx = String::new();
                }
            }
        }
    }

    let mut provided: HashSet<&str> = HashSet::new();
    // A `*args`/`**kwargs` splat could supply any key at runtime, so a missing
    // key isn't necessarily missing — stay silent rather than risk a false
    // positive.
    let mut has_splat = false;

    for arg in args {
        match &arg.node {
            Argument::Named(name, value) => {
                let key = name.node.as_str();
                provided.insert(key);
                match known.iter().find(|(n, _, _)| n == key) {
                    // A known field: flag a literal value whose type doesn't match.
                    Some((_, ty, _)) => {
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
            Argument::Args(_) | Argument::KwArgs(_) => has_splat = true,
            Argument::Positional(_) => {}
        }
    }

    if !has_splat {
        // A schema field can share a name with a base field (unusual, but not
        // forbidden); on collision the base field wins, mirroring the
        // first-match `.find()` lookup used for type-checking above.
        let mut seen: HashSet<&str> = HashSet::new();
        for (i, (name, _, required)) in known.iter().enumerate() {
            if !seen.insert(name.as_str()) {
                continue;
            }
            if *required && !provided.contains(name.as_str()) {
                let label = if i < base_count {
                    base_label
                } else {
                    schema_label.as_deref().expect(
                        "schema fields are only appended to `known` once resolved, alongside `schema_label`",
                    )
                };
                out.push(diag(
                    ast,
                    callee_span,
                    format!("missing required field `{name}` for {label}"),
                ));
            }
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
            match name {
                "exec" => Some(DriverSchema {
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
                }),
                // A required field that collides with a base `target` field
                // name — exercises the dedup/first-wins path in the
                // missing-required check.
                "shadow" => Some(DriverSchema {
                    fields: vec![DriverField {
                        name: "name".to_string(),
                        ty: ParamType::Int,
                        doc: String::new(),
                        required: true,
                    }],
                }),
                _ => None,
            }
        }
        fn driver_names(&self) -> Vec<String> {
            vec!["exec".to_string(), "shadow".to_string()]
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
        // `cmd` is supplied so this doesn't also trip the missing-required check.
        let msgs =
            messages("target(name = PREFIX + \"_t\", driver = \"exec\", cmd = \"echo hi\")\n");
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
    fn missing_required_driver_field_is_flagged() {
        // `exec` requires `cmd`; the driver is selected but `cmd` is absent.
        let msgs = messages("target(name = \"t\", driver = \"exec\")\n");
        assert!(
            msgs.iter()
                .any(|m| m.contains("missing required field `cmd`") && m.contains("`exec` driver")),
            "expected missing-required error for cmd, got {msgs:?}"
        );
    }

    #[test]
    fn missing_required_driver_field_is_silent_when_driver_unresolved() {
        // No driver selected → the driver's required fields are unknown, so no
        // missing-required error for them (only the base `name` field is checked).
        let msgs = messages("target(name = \"t\")\n");
        assert!(
            msgs.is_empty(),
            "must not guess required driver fields without a driver, got {msgs:?}"
        );
    }

    #[test]
    fn missing_required_base_field_is_flagged() {
        // `name` is always required, driver or not.
        let msgs = messages("target(driver = \"exec\", cmd = \"echo hi\")\n");
        assert!(
            msgs.iter()
                .any(|m| m.contains("missing required field `name`")),
            "expected missing-required error for name, got {msgs:?}"
        );
    }

    #[test]
    fn missing_required_field_is_silent_with_kwargs_splat() {
        // A `**kwargs`-style splat could supply `cmd` at runtime — stay silent.
        let msgs = messages("target(name = \"t\", driver = \"exec\", **extra)\n");
        assert!(
            msgs.is_empty(),
            "a splat argument must suppress the missing-required check, got {msgs:?}"
        );
    }

    #[test]
    fn missing_required_field_is_silent_with_args_splat() {
        // A bare `*args` splat is the same shape as `**kwargs` here.
        let msgs = messages("target(name = \"t\", driver = \"exec\", *extra)\n");
        assert!(
            msgs.is_empty(),
            "a positional splat must also suppress the missing-required check, got {msgs:?}"
        );
    }

    #[test]
    fn missing_required_driver_field_is_silent_for_unknown_driver() {
        // An unresolvable driver name is just as unknowable as no driver at
        // all — the unknown-key check already stays silent here (see
        // `unknown_target_field_is_silent_when_driver_unresolved`); the
        // missing-required check must too.
        let msgs = messages("target(name = \"t\", driver = \"nope\")\n");
        assert!(
            msgs.is_empty(),
            "must not guess required fields for an unresolvable driver, got {msgs:?}"
        );
    }

    #[test]
    fn missing_required_field_on_shadowed_base_field_reports_once() {
        // `shadow`'s required `name` field collides with the base `name`
        // field; omitting `name` entirely must produce exactly one
        // missing-required diagnostic for it, not one per source.
        let msgs = messages("target(driver = \"shadow\")\n");
        let name_msgs: Vec<_> = msgs
            .iter()
            .filter(|m| m.contains("missing required field `name`"))
            .collect();
        assert_eq!(
            name_msgs.len(),
            1,
            "expected exactly one missing-required diagnostic for the shadowed field, got {msgs:?}"
        );
    }

    #[test]
    fn missing_required_field_on_multiline_call_anchors_on_callee_token() {
        // The diagnostic must land on the `target` token (line 0), regardless
        // of which line the call's arguments span.
        let content = "target(\n    name = \"t\",\n    driver = \"exec\",\n)\n";
        let ast = AstModule::parse("BUILD", content.to_string(), &Dialect::Extended).unwrap();
        let diags = validate(&ast, &FakeEngine);
        let d = diags
            .iter()
            .find(|d| d.message.contains("missing required field `cmd`"))
            .expect("missing-cmd diagnostic");
        assert_eq!(
            d.range.start.line, 0,
            "must anchor on the `target` token line"
        );
        assert_eq!(d.range.start.character, 0);
        assert_eq!(d.range.end.character, "target".len() as u32);
    }

    #[test]
    fn missing_required_field_fires_inside_list_comprehension() {
        // The AST walk covers comprehension bodies, not just top-level calls.
        let msgs = messages("y = [target(name = str(i), driver = \"exec\") for i in [1, 2]]\n");
        assert!(
            msgs.iter()
                .any(|m| m.contains("missing required field `cmd`")),
            "expected missing-required to fire inside a list comprehension, got {msgs:?}"
        );
    }

    #[test]
    fn provider_state_missing_provider_is_flagged() {
        let msgs = messages("provider_state(bogus = 1)\n");
        assert!(
            msgs.iter()
                .any(|m| m.contains("missing required field `provider`")),
            "expected missing-required error for provider, got {msgs:?}"
        );
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
        // be fully typed, so it must be skipped rather than mis-flagged. `cmd` is
        // supplied so this doesn't also trip the missing-required check.
        let msgs = messages(
            "target(name = \"t\", driver = \"exec\", cmd = \"echo hi\", labels = [\"a\", X])\n",
        );
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

    /// A call that the builtin checks would flag twice — `bogus` is no field of
    /// `target` or the `exec` driver, and the driver's required `cmd` is absent.
    /// Shared by the shadowing tests so each one is provably non-vacuous: with
    /// no shadowing binding it produces both diagnostics
    /// (`an_unrelated_binding_does_not_suppress_validation`).
    const FLAGGED_TARGET_CALL: &str = "target(name = \"t\", driver = \"exec\", bogus = 1)\n";

    #[test]
    fn locally_defined_target_macro_is_not_validated() {
        // The buffer defines its own `target`, taking `**kwargs`: `bogus` is a
        // legitimate key there and `cmd` is no requirement of it. Checking the
        // call against the builtin's schema would underline working code.
        let msgs = messages(&format!(
            "def target(name, **kwargs):\n    pass\n\n{FLAGGED_TARGET_CALL}"
        ));
        assert!(
            msgs.is_empty(),
            "a locally-defined `target` is not the builtin, got {msgs:?}"
        );
    }

    #[test]
    fn loaded_target_macro_is_not_validated() {
        // Same, via `load(...)` — including the aliased form, where the local
        // name is what the call site uses.
        let msgs = messages(&format!(
            "load(\"//lib\", \"target\")\n\n{FLAGGED_TARGET_CALL}"
        ));
        assert!(
            msgs.is_empty(),
            "a loaded `target` is not the builtin, got {msgs:?}"
        );

        let msgs = messages(&format!(
            "load(\"//lib\", target = \"my_target\")\n\n{FLAGGED_TARGET_CALL}"
        ));
        assert!(
            msgs.is_empty(),
            "an aliased loaded `target` is not the builtin, got {msgs:?}"
        );
    }

    #[test]
    fn assigned_provider_state_is_not_validated() {
        // A plain rebinding shadows the builtin just as a `def` does. Without
        // the binding this reports the builtin's missing required `provider`
        // (`provider_state_missing_provider_is_flagged`).
        let msgs = messages("provider_state = my_fn\n\nprovider_state(bogus = 1)\n");
        assert!(
            msgs.is_empty(),
            "a rebound `provider_state` is not the builtin, got {msgs:?}"
        );
    }

    #[test]
    fn an_unrelated_binding_does_not_suppress_validation() {
        // Shadowing is per-name: binding `foo` must not silence `target`. Also
        // pins that the shadowing tests above are not vacuous — the very same
        // call is flagged here.
        let msgs = messages(&format!(
            "def foo(a, **kwargs):\n    pass\n\n{FLAGGED_TARGET_CALL}"
        ));
        assert!(
            msgs.iter().any(|m| m.contains("unknown field `bogus`")),
            "unrelated bindings must not disable the unknown-key check, got {msgs:?}"
        );
        assert!(
            msgs.iter()
                .any(|m| m.contains("missing required field `cmd`")),
            "unrelated bindings must not disable the missing-required check, got {msgs:?}"
        );
    }

    #[test]
    fn bound_names_collects_every_binding_form() {
        let content = "\
load(\"//lib\", \"imported\", alias = \"exported\")
assigned = 1
tuple_a, tuple_b = 1, 2
augmented = 0
augmented += 1
for loop_var in [1]:
    pass

def fn_name(param, *args_param, **kwargs_param):
    pass
";
        let ast = AstModule::parse("BUILD", content.to_string(), &Dialect::Extended).unwrap();
        let names = super::bound_names(&ast);
        for expected in [
            "imported",
            "alias",
            "assigned",
            "tuple_a",
            "tuple_b",
            "augmented",
            "loop_var",
            "fn_name",
            "param",
            "args_param",
            "kwargs_param",
        ] {
            assert!(
                names.contains(expected),
                "missing binding {expected}: {names:?}"
            );
        }
        // `exported` is the name in the *other* module, not a local binding.
        assert!(!names.contains("exported"), "load alias source: {names:?}");
    }
}
