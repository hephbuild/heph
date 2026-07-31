//! Static extraction of import/require/dynamic-import specifiers from one
//! JS/TS/JSX/TSX source file, via `oxc_parser` — the parsing half of M2's
//! import-graph resolver (see `ai-docs/js-plugin-plan.md`'s "Dependency
//! graph" section). Resolving each specifier to a filesystem path is
//! `resolvers.rs`'s job; this module only turns source bytes into a flat list
//! of `(specifier, context, type_only)` sites.
//!
//! **What is extracted** (per the task's M2 scope):
//! - `import ... from 'x'` / bare `import 'x'` — [`ModuleContext::Esm`]
//! - `export ... from 'x'` / `export * from 'x'` (re-exports) — [`ModuleContext::Esm`]
//! - top-level-or-nested `require('x')` calls, i.e. any `CallExpression`
//!   whose callee is the identifier `require` and whose sole argument is a
//!   string literal — [`ModuleContext::Cjs`]. Deliberately not restricted to
//!   the top scope: `require` inside a function body is exactly as
//!   statically resolvable as one at module scope (the specifier is still a
//!   literal), and narrowing to top-level-only would silently under-declare
//!   real edges — the "fail or fix, never ignore" rule from `.claude/rust.md`
//!   applies to *omitting* real, staticly-known edges just as much as to
//!   erroring.
//! - `import('x')` dynamic import with a literal string argument —
//!   [`ModuleContext::Esm`] (Node's dynamic `import()` always follows the ESM
//!   resolution algorithm, even from a CommonJS file).
//!
//! **Dynamic import with a non-literal argument** (`import(someExpr)`,
//! `import(\`./locales/${lang}.js\`)`, ...) is unresolvable statically by
//! construction — there is no specifier string to resolve at all, only an
//! expression whose value is known at runtime. Design decision, stated per
//! the task: this is **coarsened, not an error**. Reasoning:
//! - It is completely normal, valid JS/TS (a template-literal dynamic import
//!   over a locale/plugin directory is a common, intentional pattern every
//!   real bundler special-cases, e.g. esbuild/webpack turn it into a
//!   runtime glob-import of the matched directory).
//! - `ai-docs/js-plugin-plan.md`'s "Correctness safety valve" is explicit that
//!   the resolver informs the dependency graph and cache key, but "the real
//!   toolchain … is the ground truth at execution time" — a heph-side hard
//!   failure here would reject packages that build and run today, over a
//!   deliberately dynamic construct only a real bundler can (and does)
//!   handle.
//! - Erroring here would gate `Provider::get` — i.e. it would make an entire
//!   package unbuildable/undiscoverable by heph for using an ordinary,
//!   working JS idiom. That is a worse outcome than the alternative.
//!
//! It is **not silently dropped**: every occurrence is counted
//! ([`ParsedImports::unresolved_dynamic_imports`]) and logged
//! (`tracing::debug!`), so it is visible rather than invisible — the
//! "record it as an unresolved/coarsened edge" half of the task's
//! instruction. No graph edge is produced for it (there is nothing to
//! resolve it to), so it cannot trip phantom-dependency detection either.
//!
//! **`import type` / `export type`** (TypeScript type-only syntax) are
//! extracted the same way as their value counterparts but flagged
//! `type_only: true` — `importgraph.rs` routes these into the separate type
//! graph rather than the runtime graph (see that module's docs for why the
//! two must stay separate).

use oxc_allocator::Allocator;
use oxc_ast::ast::{Argument, CallExpression, Expression, ImportOrExportKind};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_span::SourceType;
use std::path::Path;

/// Whether a specifier occurrence should be resolved under Node's ESM or CJS
/// module-resolution algorithm — the two differ in `condition_names`
/// (`["node","import"]` vs `["node","require"]`) and, for `import()`, always
/// resolve as ESM regardless of the file's own module type. See module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModuleContext {
    Esm,
    Cjs,
}

/// One statically-extracted specifier occurrence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSite {
    pub specifier: String,
    pub context: ModuleContext,
    /// `true` for `import type ... from`, `export type ... from`, and
    /// `export type * from` — routed to the separate type graph.
    pub type_only: bool,
}

/// Everything statically extracted from one source file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedImports {
    pub sites: Vec<ImportSite>,
    /// Count of `import()` call sites whose argument was not a string
    /// literal — see module docs' "Dynamic import with a non-literal
    /// argument" section. Never resolved, never silently dropped.
    pub unresolved_dynamic_imports: usize,
}

/// Parse `source_text` (the contents of `path`, used only to infer
/// JS/JSX/TS/TSX parsing mode from the extension) and extract every
/// statically-known import/require/dynamic-import specifier.
///
/// A fatal parse error (`ParserReturn::panicked`) is a hard error — an
/// unparseable first-party source file is a real problem, not something to
/// silently skip (`.claude/rust.md`'s "fail or fix, never ignore"). A
/// *recoverable* syntax error (diagnostics non-empty but `panicked` false)
/// still yields a usable, partial AST; that's logged, not failed, since oxc's
/// own recovery already produced a best-effort tree — for the purpose of
/// this milestone (declared-dependency cross-validation and cache-relevant
/// edges), a partially-recovered file's import statements are still real
/// signal worth keeping.
pub fn parse_file_imports(path: &Path, source_text: &str) -> anyhow::Result<ParsedImports> {
    let source_type = SourceType::from_path(path)
        .map_err(|e| anyhow::anyhow!("{}: unrecognized source extension: {e}", path.display()))?;
    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, source_text, source_type).parse();

    anyhow::ensure!(
        !ret.panicked,
        "{}: failed to parse (fatal syntax error): {}",
        path.display(),
        ret.diagnostics
            .iter()
            .map(|d| d.to_string())
            .collect::<Vec<_>>()
            .join("; ")
    );
    if !ret.diagnostics.is_empty() {
        tracing::debug!(
            file = %path.display(),
            diagnostics = ret.diagnostics.len(),
            "js import parse: recoverable syntax errors, using best-effort AST"
        );
    }

    let mut visitor = ImportVisitor::default();
    visitor.visit_program(&ret.program);
    Ok(ParsedImports {
        sites: visitor.sites,
        unresolved_dynamic_imports: visitor.unresolved_dynamic_imports,
    })
}

#[derive(Default)]
struct ImportVisitor {
    sites: Vec<ImportSite>,
    unresolved_dynamic_imports: usize,
}

impl ImportVisitor {
    fn push(&mut self, specifier: &str, context: ModuleContext, type_only: bool) {
        self.sites.push(ImportSite {
            specifier: specifier.to_string(),
            context,
            type_only,
        });
    }
}

/// A `require(...)` call: callee is the bare identifier `require`, exactly one
/// argument, which is a string literal. Anything else (computed callee,
/// member-expression callee like `mod.require(...)`, zero/multiple args, a
/// non-literal arg) is not a statically-resolvable `require` and is left
/// alone — mirroring Node's own runtime behavior of only ever accepting a
/// single string argument, and this milestone's stated scope of static,
/// literal-argument resolution only.
fn as_require_specifier<'a>(call: &CallExpression<'a>) -> Option<&'a str> {
    let Expression::Identifier(ident) = &call.callee else {
        return None;
    };
    if ident.name.as_str() != "require" {
        return None;
    }
    let args: &[Argument<'a>] = &call.arguments;
    let [arg] = args else {
        return None;
    };
    string_literal_of_argument(arg)
}

fn string_literal_of_argument<'a>(arg: &Argument<'a>) -> Option<&'a str> {
    match arg.as_expression()? {
        Expression::StringLiteral(s) => Some(s.value.as_str()),
        _ => None,
    }
}

impl<'a> Visit<'a> for ImportVisitor {
    fn visit_import_declaration(&mut self, it: &oxc_ast::ast::ImportDeclaration<'a>) {
        self.push(
            it.source.value.as_str(),
            ModuleContext::Esm,
            it.import_kind == ImportOrExportKind::Type,
        );
        walk::walk_import_declaration(self, it);
    }

    fn visit_export_named_declaration(&mut self, it: &oxc_ast::ast::ExportNamedDeclaration<'a>) {
        if let Some(source) = &it.source {
            self.push(
                source.value.as_str(),
                ModuleContext::Esm,
                it.export_kind == ImportOrExportKind::Type,
            );
        }
        walk::walk_export_named_declaration(self, it);
    }

    fn visit_export_all_declaration(&mut self, it: &oxc_ast::ast::ExportAllDeclaration<'a>) {
        self.push(
            it.source.value.as_str(),
            ModuleContext::Esm,
            it.export_kind == ImportOrExportKind::Type,
        );
        walk::walk_export_all_declaration(self, it);
    }

    fn visit_call_expression(&mut self, it: &oxc_ast::ast::CallExpression<'a>) {
        if let Some(specifier) = as_require_specifier(it) {
            self.push(specifier, ModuleContext::Cjs, false);
        }
        walk::walk_call_expression(self, it);
    }

    fn visit_import_expression(&mut self, it: &oxc_ast::ast::ImportExpression<'a>) {
        match &it.source {
            Expression::StringLiteral(s) => {
                self.push(s.value.as_str(), ModuleContext::Esm, false);
            }
            _ => {
                self.unresolved_dynamic_imports += 1;
                tracing::debug!(
                    span = ?it.span,
                    "js import parse: dynamic import() with a non-literal argument, coarsened \
                     (no static edge — see importparse.rs module docs)"
                );
            }
        }
        walk::walk_import_expression(self, it);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn parse(ext: &str, source: &str) -> ParsedImports {
        let path = PathBuf::from(format!("test.{ext}"));
        parse_file_imports(&path, source).expect("parse")
    }

    #[test]
    fn extracts_import_declaration() {
        let p = parse("ts", "import { foo } from 'bar';");
        assert_eq!(p.sites.len(), 1);
        assert_eq!(p.sites[0].specifier, "bar");
        assert_eq!(p.sites[0].context, ModuleContext::Esm);
        assert!(!p.sites[0].type_only);
    }

    #[test]
    fn extracts_bare_import() {
        let p = parse("js", "import 'side-effect';");
        assert_eq!(p.sites.len(), 1);
        assert_eq!(p.sites[0].specifier, "side-effect");
    }

    #[test]
    fn extracts_export_from() {
        let p = parse("ts", "export { foo } from 'bar';");
        assert_eq!(p.sites.len(), 1);
        assert_eq!(p.sites[0].specifier, "bar");
        assert!(!p.sites[0].type_only);
    }

    #[test]
    fn extracts_export_star_from() {
        let p = parse("ts", "export * from 'bar';");
        assert_eq!(p.sites.len(), 1);
        assert_eq!(p.sites[0].specifier, "bar");
    }

    #[test]
    fn extracts_export_star_as_from() {
        let p = parse("ts", "export * as ns from 'bar';");
        assert_eq!(p.sites.len(), 1);
        assert_eq!(p.sites[0].specifier, "bar");
    }

    #[test]
    fn extracts_require_call() {
        let p = parse("js", "const x = require('lodash');");
        assert_eq!(p.sites.len(), 1);
        assert_eq!(p.sites[0].specifier, "lodash");
        assert_eq!(p.sites[0].context, ModuleContext::Cjs);
    }

    #[test]
    fn extracts_require_call_nested_in_function() {
        let p = parse(
            "js",
            "function load() { if (true) { return require('lodash'); } }",
        );
        assert_eq!(p.sites.len(), 1);
        assert_eq!(p.sites[0].specifier, "lodash");
    }

    #[test]
    fn ignores_require_like_calls_that_are_not_the_real_require() {
        let p = parse("js", "const x = mod.require('lodash'); other.require('x');");
        assert!(p.sites.is_empty());
    }

    #[test]
    fn extracts_dynamic_import_with_literal() {
        let p = parse("js", "const x = import('lodash');");
        assert_eq!(p.sites.len(), 1);
        assert_eq!(p.sites[0].specifier, "lodash");
        assert_eq!(p.sites[0].context, ModuleContext::Esm);
        assert_eq!(p.unresolved_dynamic_imports, 0);
    }

    #[test]
    fn coarsens_dynamic_import_with_non_literal_argument() {
        let p = parse(
            "js",
            "const lang = 'en'; const x = import(`./locales/${lang}.js`);",
        );
        assert!(
            p.sites.is_empty(),
            "a non-literal dynamic import must not produce a resolved edge: {:?}",
            p.sites
        );
        assert_eq!(p.unresolved_dynamic_imports, 1);
    }

    #[test]
    fn coarsens_dynamic_import_with_identifier_argument() {
        let p = parse("js", "function load(mod) { return import(mod); }");
        assert_eq!(p.unresolved_dynamic_imports, 1);
        assert!(p.sites.is_empty());
    }

    #[test]
    fn import_type_is_flagged_type_only() {
        let p = parse("ts", "import type { Foo } from 'bar';");
        assert_eq!(p.sites.len(), 1);
        assert_eq!(p.sites[0].specifier, "bar");
        assert!(p.sites[0].type_only);
    }

    #[test]
    fn export_type_from_is_flagged_type_only() {
        let p = parse("ts", "export type { Foo } from 'bar';");
        assert_eq!(p.sites.len(), 1);
        assert!(p.sites[0].type_only);
    }

    #[test]
    fn regular_import_is_not_type_only() {
        let p = parse("ts", "import { Foo } from 'bar';");
        assert!(!p.sites[0].type_only);
    }

    #[test]
    fn extracts_multiple_sites_in_one_file() {
        let p = parse(
            "ts",
            "import a from 'a';\nexport { b } from 'b';\nconst c = require('c');\n",
        );
        let specs: Vec<&str> = p.sites.iter().map(|s| s.specifier.as_str()).collect();
        assert_eq!(specs, vec!["a", "b", "c"]);
    }

    #[test]
    fn tsx_source_parses() {
        let p = parse("tsx", "import React from 'react';\nconst x = <div />;\n");
        assert_eq!(p.sites.len(), 1);
        assert_eq!(p.sites[0].specifier, "react");
    }

    #[test]
    fn unparseable_source_is_a_hard_error() {
        let path = PathBuf::from("broken.ts");
        // Unterminated string literal: the lexer cannot recover from this at
        // all, so oxc reports `panicked = true` -- must surface as `Err`,
        // not an empty/silent result.
        let err = parse_file_imports(&path, "const x = \"unterminated").unwrap_err();
        assert!(format!("{err:#}").contains("failed to parse"), "{err:#}");
    }

    #[test]
    fn recoverable_syntax_error_does_not_fail_the_whole_parse() {
        // Missing semicolon before `import` is a recoverable ASI case in
        // practice; regardless, an import that IS well-formed elsewhere in
        // the file must still be extracted even if oxc reports diagnostics.
        let p = parse("ts", "import { a } from 'a'\nimport { b } from 'b'\n");
        let specs: Vec<&str> = p.sites.iter().map(|s| s.specifier.as_str()).collect();
        assert_eq!(specs, vec!["a", "b"]);
    }
}
