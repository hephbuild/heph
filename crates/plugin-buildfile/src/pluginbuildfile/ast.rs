//! Syntax-tree walks shared by evaluation (`run_file`'s function-reference
//! check), the LSP and the BUILD-file checks: they read a parsed `AstModule`
//! and never evaluate it.

use starlark::syntax::AstModule;
use starlark::syntax::ast::{AssignTarget, AstAssignTarget, AstExpr, AstStmt, Stmt};
use std::collections::HashSet;

/// Visit `s` and every statement nested within it.
pub(crate) fn walk_stmt<'a>(s: &'a AstStmt, f: &mut impl FnMut(&'a AstStmt)) {
    f(s);
    // `Stmt::visit_stmt` is one level deep; recurse to reach the whole subtree.
    s.visit_stmt(|child| walk_stmt(child, f));
}

/// Visit `e` and every expression nested within it.
pub(crate) fn walk_expr<'a>(e: &'a AstExpr, f: &mut impl FnMut(&'a AstExpr)) {
    f(e);
    // `Expr::visit_expr` is one level deep; recurse to reach the whole subtree.
    e.visit_expr(|child| walk_expr(child, f));
}

/// Every name the module binds: `def`s and their parameters, assignment and loop
/// targets, and `load(...)` imports.
///
/// Deliberately flat — a name bound anywhere counts as bound everywhere, with no
/// scope tracking. Which way over-approximating errs depends on the caller: for
/// the LSP and the function-reference check it errs toward silence (a name the
/// buffer binds is not reported as heph's), while for `annotations::missing` it
/// errs toward a finding (a `True` rebound anywhere stops exempting a `= True`
/// parameter, although the typechecker still types it where the rebinding is
/// local to another def). The alternative (a real scope analysis) would buy
/// nothing: a buffer that defines its own `target` almost certainly means it at
/// every call site.
pub(crate) fn bound_names(ast: &AstModule) -> HashSet<&str> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let ast = AstModule::parse("BUILD", content.to_string(), &hxstarlark_fmt::DIALECT).unwrap();
        let names = bound_names(&ast);
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
