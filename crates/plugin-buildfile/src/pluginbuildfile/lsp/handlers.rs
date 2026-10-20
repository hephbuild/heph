//! The site-kind table: for each [`Site`] kind, what completion offers, what
//! hover shows, where definition jumps, and what happens to the stock
//! analysis's items — under one merge policy.
//!
//! **Merge policy.** Wherever heph controls or knows the answer for sure
//! (keyword names, driver and provider names, plugin namespaces and members,
//! addresses, load paths and symbols) heph is authoritative and replaces the
//! stock answer, for completion, hover and definition alike; the stock answer
//! passes through only where heph cannot know (free expressions, locals). Every
//! heph completion item carries a text edit over the site's token, a
//! `filterText` matching what the editor compares against, one kind per site
//! kind, Markdown documentation and a `sortText` of `0_<name>`; heph filters by
//! prefix itself, dedupes by label, and the response is always an array marked
//! complete.
//!
//! The renderers and item builders here moved from the old response proxy;
//! they gained the resolver's [`Site`] as input instead of re-scanning text.

use super::context::HephLspContext;
use super::index::SharedState;
use super::lex::{Kind, Tokens};
use super::position::{self, Encoding};
use super::site::{Builtin, Call, Quote, Site, Str};
use crate::pluginbuildfile::run_file::{resolve_load_target, target_base_fields};
use crate::pluginbuildfile::{DEFAULT_INDENT, build_file_indent_from_options};
use hplugin::driver::DriverSchema;
use hplugin::lsp::LspEngine;
use hplugin::provider::StateSchema;
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionTextEdit, Documentation, Hover, HoverContents,
    LocationLink, MarkupContent, MarkupKind, Position, Range, TextEdit,
};
use starlark::syntax::{AstModule, Dialect};
use std::collections::{HashMap, HashSet};
use std::ops::Range as Bytes;
use std::path::{Path, PathBuf};
use vendored_starlark_lsp::server::LspUri;
use vendored_starlark_lsp::uri::file_path_to_uri;

/// The drivers and providers, read once per server start.
#[derive(Debug, Default)]
pub(crate) struct Registry {
    pub drivers: Vec<String>,
    pub driver_schemas: HashMap<String, DriverSchema>,
    pub providers: Vec<String>,
    pub provider_schemas: HashMap<String, StateSchema>,
}

impl Registry {
    pub(crate) fn snapshot(engine: &dyn LspEngine) -> Registry {
        let drivers = engine.driver_names();
        let driver_schemas = drivers
            .iter()
            .filter_map(|d| engine.driver_schema(d).map(|s| (d.clone(), s)))
            .collect();
        let providers = engine.provider_names();
        let provider_schemas = providers
            .iter()
            .filter_map(|p| engine.provider_state_schema(p).map(|s| (p.clone(), s)))
            .collect();
        Registry {
            drivers,
            driver_schemas,
            providers,
            provider_schemas,
        }
    }
}

/// What happens to the stock analysis's answer at a site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Merge {
    /// heph's answer is the whole answer.
    Replace,
    /// The stock answer, untouched.
    PassThrough,
}

/// The stock column of the site table.
pub(crate) fn merge_for(site: &Site) -> Merge {
    match site {
        Site::None | Site::Callee { .. } => Merge::PassThrough,
        _ => Merge::Replace,
    }
}

/// Everything a handler reads.
pub(crate) struct Ctx<'a> {
    pub text: &'a str,
    pub tokens: &'a Tokens,
    pub uri: &'a LspUri,
    pub encoding: Encoding,
    pub registry: &'a Registry,
    pub shared: &'a SharedState,
    pub listing: &'a HephLspContext,
}

impl Ctx<'_> {
    fn range(&self, r: &Bytes<usize>) -> Range {
        Range::new(
            position::to_position(self.text, r.start, self.encoding),
            position::to_position(self.text, r.end, self.encoding),
        )
    }

    fn path(&self) -> Option<&Path> {
        match self.uri {
            LspUri::File(p) => Some(p),
            _ => None,
        }
    }

    fn package(&self) -> String {
        self.path()
            .map(|p| pkg_of(&self.shared.root, p))
            .unwrap_or_default()
    }

    /// The selected driver's schema, when the call names one that resolves.
    fn driver(&self, call: &Call) -> Option<(&str, &DriverSchema)> {
        let name = call.selected.as_deref()?;
        self.registry
            .driver_schemas
            .get_key_value(name)
            .map(|(k, v)| (k.as_str(), v))
    }

    fn provider(&self, call: &Call) -> Option<(&str, &StateSchema)> {
        let name = call.selected.as_deref()?;
        self.registry
            .provider_schemas
            .get_key_value(name)
            .map(|(k, v)| (k.as_str(), v))
    }
}

// ---------------------------------------------------------------------------
// Completion.
// ---------------------------------------------------------------------------

/// One heph candidate before the conventions are applied.
struct Candidate {
    label: String,
    detail: Option<String>,
    doc: String,
    /// What replaces the edit range (before quoting).
    insert: String,
}

fn markdown(value: String) -> Documentation {
    Documentation::MarkupContent(MarkupContent {
        kind: MarkupKind::Markdown,
        value,
    })
}

/// The kind every item of a site carries.
fn item_kind(site: &Site) -> CompletionItemKind {
    match site {
        Site::TargetKey { .. } | Site::StateKey { .. } => CompletionItemKind::FIELD,
        Site::DriverValue { .. } | Site::ProviderValue { .. } => CompletionItemKind::ENUM_MEMBER,
        Site::PluginNamespace { .. } => CompletionItemKind::MODULE,
        Site::PluginMember { .. } | Site::LoadSymbol { .. } => CompletionItemKind::FUNCTION,
        Site::Address { .. } => CompletionItemKind::REFERENCE,
        Site::LoadPath { .. } => CompletionItemKind::FILE,
        Site::None | Site::Callee { .. } => CompletionItemKind::TEXT,
    }
}

/// The heph completion items for a site, conventions applied.
pub(crate) fn items(site: &Site, ctx: &Ctx<'_>) -> Vec<CompletionItem> {
    let (candidates, edit, value): (Vec<Candidate>, Bytes<usize>, Option<&Str>) = match site {
        Site::None | Site::Callee { .. } => return Vec::new(),
        Site::TargetKey { call, edit, .. } => (target_keys(call, ctx), edit.clone(), None),
        Site::StateKey { call, edit, .. } => (state_keys(call, ctx), edit.clone(), None),
        Site::DriverValue { value, .. } => (
            ctx.registry
                .drivers
                .iter()
                .map(|d| Candidate {
                    label: d.clone(),
                    detail: Some("driver".to_string()),
                    doc: ctx
                        .registry
                        .driver_schemas
                        .get(d)
                        .map(|s| driver_doc(d, s))
                        .unwrap_or_default(),
                    insert: d.clone(),
                })
                .collect(),
            value.edit.clone(),
            Some(value),
        ),
        Site::ProviderValue { value, .. } => (
            ctx.registry
                .providers
                .iter()
                .map(|p| Candidate {
                    label: p.clone(),
                    detail: Some("provider".to_string()),
                    doc: ctx
                        .registry
                        .provider_schemas
                        .get(p)
                        .map(|s| provider_doc(p, s))
                        .unwrap_or_default(),
                    insert: p.clone(),
                })
                .collect(),
            value.edit.clone(),
            Some(value),
        ),
        Site::PluginNamespace { edit, .. } => (namespaces(ctx), edit.clone(), None),
        Site::PluginMember { plugin, edit, .. } => (members(plugin, ctx), edit.clone(), None),
        Site::Address { value } => (
            addresses(&value.prefix, ctx),
            value.edit.clone(),
            Some(value),
        ),
        Site::LoadPath { value } => (
            load_paths(&value.prefix, ctx),
            value.edit.clone(),
            Some(value),
        ),
        Site::LoadSymbol { path, value } => {
            (load_symbols(path, ctx), value.edit.clone(), Some(value))
        }
    };
    let prefix = site.prefix().unwrap_or("");
    // What the editor compares `filterText` against: the edit range's text up
    // to the cursor, which for an unterminated string starts at its quote.
    let lead = value
        .filter(|v| v.quote == Quote::Open)
        .and_then(|v| {
            let quote_end = v.token.start + ctx.text.get(v.token.clone())?.find(['"', '\''])? + 1;
            ctx.text.get(v.edit.start..quote_end)
        })
        .unwrap_or("");
    let quote = value
        .and_then(|v| ctx.text.get(v.token.clone()))
        .and_then(|t| t.chars().find(|c| *c == '"' || *c == '\''))
        .unwrap_or('"');
    let range = ctx.range(&edit);
    let kind = item_kind(site);
    let mut seen = HashSet::new();
    let mut out: Vec<CompletionItem> = candidates
        .into_iter()
        .filter(|c| c.label.starts_with(prefix))
        .filter(|c| seen.insert(c.label.clone()))
        .map(|c| {
            let new_text = match value.map(|v| v.quote) {
                Some(Quote::Open) | Some(Quote::Absent) if needs_quotes(site) => {
                    format!("{quote}{}{quote}", c.insert)
                }
                _ => c.insert,
            };
            CompletionItem {
                filter_text: Some(format!("{lead}{}", c.label)),
                sort_text: Some(format!("0_{}", c.label)),
                kind: Some(kind),
                detail: c.detail,
                documentation: (!c.doc.is_empty()).then(|| markdown(c.doc)),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit { range, new_text })),
                label: c.label,
                ..Default::default()
            }
        })
        .collect();
    out.sort_by(|a, b| a.sort_text.cmp(&b.sort_text));
    out
}

/// Whether an unquoted or half-quoted value is inserted with its quotes.
fn needs_quotes(site: &Site) -> bool {
    matches!(
        site,
        Site::DriverValue { .. }
            | Site::ProviderValue { .. }
            | Site::Address { .. }
            | Site::LoadPath { .. }
            | Site::LoadSymbol { .. }
    )
}

fn field_doc(name: &str, ty: &str, doc: &str, required: bool) -> String {
    let mut md = format!("```python\n{name}: {ty}\n```");
    if required {
        md.push_str("\n\nRequired.");
    }
    if !doc.is_empty() {
        md.push_str("\n\n");
        md.push_str(doc);
    }
    md
}

fn field_candidate(name: &str, ty: &str, doc: &str, ctx_name: &str, required: bool) -> Candidate {
    let detail = if required {
        format!("{ctx_name}: {ty} (required)")
    } else {
        format!("{ctx_name}: {ty}")
    };
    Candidate {
        label: name.to_string(),
        detail: Some(detail),
        doc: field_doc(name, ty, doc, required),
        insert: format!("{name} = "),
    }
}

/// `target(` keys: the base fields plus the selected driver's, minus the keys
/// already present.
fn target_keys(call: &Call, ctx: &Ctx<'_>) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = target_base_fields()
        .iter()
        .map(|f| field_candidate(&f.name, &f.ty.render(), &f.doc, "target", f.required))
        .collect();
    if let Some((driver, schema)) = ctx.driver(call) {
        out.extend(
            schema
                .fields
                .iter()
                .map(|f| field_candidate(&f.name, &f.ty.render(), &f.doc, driver, f.required)),
        );
    }
    out.retain(|c| !call.keys.contains(&c.label));
    out
}

/// `provider_state(` keys: the base `provider` key, then the selected
/// provider's schema, minus the keys already present.
fn state_keys(call: &Call, ctx: &Ctx<'_>) -> Vec<Candidate> {
    let mut out = vec![field_candidate(
        "provider",
        "string",
        "Provider whose state this sets.",
        "provider_state",
        true,
    )];
    if let Some((provider, schema)) = ctx.provider(call) {
        out.extend(
            schema
                .fields
                .iter()
                .map(|f| field_candidate(&f.name, &f.ty.render(), &f.doc, provider, f.required)),
        );
    }
    out.retain(|c| !call.keys.contains(&c.label));
    out
}

fn driver_doc(name: &str, schema: &DriverSchema) -> String {
    let mut md = format!("**`{name}`** driver");
    if schema.fields.is_empty() {
        md.push_str("\n\nAccepts no config.");
    }
    for f in &schema.fields {
        let req = if f.required { " (required)" } else { "" };
        md.push_str(&format!("\n- `{}`: {}{req}", f.name, f.ty.render()));
        if !f.doc.is_empty() {
            md.push_str(&format!(" — {}", f.doc));
        }
    }
    md
}

fn provider_doc(name: &str, schema: &StateSchema) -> String {
    let mut md = format!("**`{name}`** provider state");
    for f in &schema.fields {
        let req = if f.required { " (required)" } else { "" };
        md.push_str(&format!("\n- `{}`: {}{req}", f.name, f.ty.render()));
        if !f.doc.is_empty() {
            md.push_str(&format!(" — {}", f.doc));
        }
    }
    md
}

/// `heph.` → the plugin namespaces plus `core`.
fn namespaces(ctx: &Ctx<'_>) -> Vec<Candidate> {
    let registry = ctx.shared.engine.function_registry();
    let mut names: Vec<String> = registry.plugins().map(|(p, _)| p.to_string()).collect();
    names.push("core".to_string());
    names.sort();
    names.dedup();
    names
        .into_iter()
        .map(|name| Candidate {
            doc: format!("The `heph.{name}` namespace."),
            detail: Some("heph namespace".to_string()),
            insert: name.clone(),
            label: name,
        })
        .collect()
}

/// `heph.<plugin>.` → that plugin's functions (`heph.core.` → the builtins).
fn members(plugin: &str, ctx: &Ctx<'_>) -> Vec<Candidate> {
    if plugin == "core" {
        return ctx
            .shared
            .core_members
            .iter()
            .map(|m| Candidate {
                label: m.name.clone(),
                detail: (!m.detail.is_empty()).then(|| m.detail.clone()),
                doc: m.doc.clone(),
                insert: m.name.clone(),
            })
            .collect();
    }
    let registry = ctx.shared.engine.function_registry();
    registry
        .plugins()
        .find(|(p, _)| *p == plugin)
        .map(|(_, fns)| {
            fns.iter()
                .map(|(name, rf)| Candidate {
                    label: name.clone(),
                    detail: Some(rf.signature.render(name)),
                    doc: function_doc(name, rf),
                    insert: name.clone(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Target names declared in the open document: `name = "…"` keyword arguments.
fn names_in(text: &str, tokens: &Tokens) -> Vec<String> {
    let toks = &tokens.toks;
    toks.windows(3)
        .filter(|w| {
            matches!(
                w,
                [a, b, c] if a.kind == Kind::Ident
                    && text.get(a.range()) == Some("name")
                    && b.kind == Kind::Equal
                    && c.kind == Kind::Str
            )
        })
        .filter_map(|w| w.get(2))
        .filter_map(|c| text.get(c.start as usize + 1..(c.end as usize).saturating_sub(1)))
        .map(str::to_string)
        .collect()
}

/// `//…` → packages, then `//pkg:…` targets; `:…` → this package's targets.
fn addresses(prefix: &str, ctx: &Ctx<'_>) -> Vec<Candidate> {
    if let Some(rel) = prefix.strip_prefix(':') {
        let mut names = ctx.listing.list_targets(&ctx.package());
        names.extend(names_in(ctx.text, ctx.tokens));
        names.sort();
        names.dedup();
        return names
            .into_iter()
            .filter(|n| n.starts_with(rel))
            .map(|n| Candidate {
                doc: format!("Target `:{n}` in this package."),
                detail: Some("target".to_string()),
                insert: format!(":{n}"),
                label: format!(":{n}"),
            })
            .collect();
    }
    let Some(rest) = prefix.strip_prefix("//") else {
        return Vec::new();
    };
    match rest.split_once(':') {
        Some((pkg, _)) => ctx
            .listing
            .list_targets(pkg)
            .into_iter()
            .map(|n| Candidate {
                doc: format!("Target `//{pkg}:{n}`."),
                detail: Some("target".to_string()),
                insert: format!("//{pkg}:{n}"),
                label: format!("//{pkg}:{n}"),
            })
            .collect(),
        None => ctx
            .listing
            .list_packages(rest)
            .into_iter()
            .map(|p| Candidate {
                doc: format!("Package `//{p}`."),
                detail: Some("package".to_string()),
                insert: format!("//{p}"),
                label: format!("//{p}"),
            })
            .collect(),
    }
}

/// `load("//…` → package paths.
fn load_paths(prefix: &str, ctx: &Ctx<'_>) -> Vec<Candidate> {
    let rest = prefix.strip_prefix("//").unwrap_or("");
    ctx.listing
        .list_packages(rest)
        .into_iter()
        .filter(|p| !p.is_empty())
        .map(|p| Candidate {
            doc: format!("Load from package `//{p}`."),
            detail: Some("package".to_string()),
            insert: format!("//{p}"),
            label: format!("//{p}"),
        })
        .collect()
}

/// The files a load path names: every BUILD file of a package directory, or
/// the one file.
fn load_files(path: &str, ctx: &Ctx<'_>) -> Vec<PathBuf> {
    let resolve = |p: &str| resolve_load_target(&ctx.shared.root, &ctx.package(), p).ok();
    // `//pkg:file.star` is how a load path is rendered for an auto-`load`
    // edit; on disk it is `//pkg/file.star`.
    let Some(resolved) = resolve(path).filter(|r| r.exists()).or_else(|| {
        let (pkg, file) = path.rsplit_once(':')?;
        resolve(&format!("{pkg}/{file}"))
    }) else {
        return Vec::new();
    };
    if resolved.is_dir() {
        package_build_files(&resolved, &ctx.shared.patterns)
    } else {
        vec![resolved]
    }
}

/// The top-level `def`s of a parsed file, with their name spans' lines.
fn defs_in(ast: &AstModule) -> Vec<(String, u32)> {
    use starlark::syntax::ast::Stmt;
    let mut out = Vec::new();
    ast.statement().visit_stmt(|s| {
        if let Stmt::Def(def) = &s.node {
            let line = ast.file_span(def.name.span).resolve_span().begin.line as u32;
            out.push((def.name.node.ident.clone(), line));
        }
    });
    if let Stmt::Def(def) = &ast.statement().node {
        let line = ast.file_span(def.name.span).resolve_span().begin.line as u32;
        out.push((def.name.node.ident.clone(), line));
    }
    out
}

fn parse_file(path: &Path) -> Option<AstModule> {
    let content = std::fs::read_to_string(path).ok()?;
    AstModule::parse(&path.to_string_lossy(), content, &Dialect::Extended).ok()
}

/// `load("…", "…` → the loaded file's exported `def`s. Read from that file's
/// own AST; the current file's is not consulted.
fn load_symbols(path: &str, ctx: &Ctx<'_>) -> Vec<Candidate> {
    load_files(path, ctx)
        .iter()
        .filter_map(|f| parse_file(f))
        .flat_map(|ast| defs_in(&ast))
        .filter(|(name, _)| !name.starts_with('_'))
        .map(|(name, _)| Candidate {
            doc: format!("`{name}`, loaded from `{path}`."),
            detail: Some(format!("Load from {path}")),
            insert: name.clone(),
            label: name,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Hover.
// ---------------------------------------------------------------------------

fn hover_markdown(value: String, range: Option<Range>) -> Hover {
    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value,
        }),
        range,
    }
}

/// heph's hover for a site, or `None` when it has nothing to say there.
pub(crate) fn hover(site: &Site, ctx: &Ctx<'_>) -> Option<Hover> {
    let (md, range) = match site {
        Site::None | Site::PluginNamespace { .. } | Site::Address { .. } => return None,
        Site::LoadPath { .. } | Site::LoadSymbol { .. } => return None,
        Site::Callee { call } => {
            let md = match call.callee {
                Builtin::ProviderState => {
                    ctx.shared.builtin_hovers.provider_state(ctx.provider(call))
                }
                _ => ctx.shared.builtin_hovers.target(ctx.driver(call)),
            };
            (md, call.callee_span.clone())
        }
        Site::DriverValue { value, .. } => {
            let name = value.content.as_str();
            let schema = ctx.registry.driver_schemas.get(name)?;
            (driver_doc(name, schema), value.token.clone())
        }
        Site::ProviderValue { value, .. } => {
            let name = value.content.as_str();
            let schema = ctx.registry.provider_schemas.get(name)?;
            (provider_doc(name, schema), value.token.clone())
        }
        Site::TargetKey { call, edit, .. } => {
            let name = ctx.text.get(edit.clone())?;
            let base = target_base_fields();
            let field = base
                .iter()
                .find(|f| f.name == name)
                .map(|f| field_doc(&f.name, &f.ty.render(), &f.doc, f.required))
                .or_else(|| {
                    let (_, schema) = ctx.driver(call)?;
                    let f = schema.fields.iter().find(|f| f.name == name)?;
                    Some(field_doc(&f.name, &f.ty.render(), &f.doc, f.required))
                })?;
            (field, edit.clone())
        }
        Site::StateKey { call, edit, .. } => {
            let name = ctx.text.get(edit.clone())?;
            let md = if name == "provider" {
                field_doc(
                    "provider",
                    "string",
                    "Provider whose state this sets.",
                    true,
                )
            } else {
                let (_, schema) = ctx.provider(call)?;
                let f = schema.fields.iter().find(|f| f.name == name)?;
                field_doc(&f.name, &f.ty.render(), &f.doc, f.required)
            };
            (md, edit.clone())
        }
        Site::PluginMember { plugin, edit, .. } => {
            let name = ctx.text.get(edit.clone())?;
            let md = if plugin == "core" {
                ctx.shared
                    .core_members
                    .iter()
                    .find(|m| m.name == name)
                    .map(|m| m.doc.clone())?
            } else {
                let registry = ctx.shared.engine.function_registry();
                let rf = registry.get(plugin, name)?;
                function_doc(name, rf)
            };
            (md, edit.clone())
        }
    };
    Some(hover_markdown(md, Some(ctx.range(&range))))
}

/// The provenance block the evaluation contributes to a stock hover: the
/// targets the call site under the cursor produced. Enriches, never gates.
pub(crate) fn provenance_block(ctx: &Ctx<'_>, offset: usize) -> Option<String> {
    let index = ctx.shared.index(ctx.uri)?;
    let at = position::newline_position(ctx.text, offset);
    let targets = index.targets_at(at.line + 1, at.character + 1)?;
    if targets.is_empty() {
        return None;
    }
    let mut md = format!("**Targets ({})**\n\n```\n", targets.len());
    for a in targets {
        md.push_str(a);
        md.push('\n');
    }
    md.push_str("```\n");
    Some(md)
}

/// The signature of an undocumented local `def`, which the stock hover skips.
pub(crate) fn def_fallback(ctx: &Ctx<'_>, offset: usize) -> Option<String> {
    let index = ctx.shared.index(ctx.uri)?;
    let at = position::newline_position(ctx.text, offset);
    index
        .def_hover_at(at.line + 1, at.character + 1)
        .map(str::to_string)
}

/// A plugin function's hover and completion doc: its signature rendered as a
/// Starlark `def` prototype (the stock renderer, so it reads like a local
/// `def`), then its doc.
fn function_doc(name: &str, rf: &hplugin::function::RegisteredFn) -> String {
    use hcore::htvalue::signature::Param;
    use starlark::docs::markdown::render_doc_item_no_link;
    use starlark::docs::{
        DocFunction, DocItem, DocMember, DocParam, DocParams, DocReturn, DocString, DocStringKind,
    };
    let sig = &rf.signature;
    let ty = |p: &Param| crate::pluginbuildfile::run_file::param_type_to_ty(&p.ty);
    let to_param = |p: &Param| DocParam {
        name: p.name.to_string(),
        docs: None,
        typ: ty(p),
        default_value: p.default.as_ref().map(default_repr),
    };
    let params = DocParams {
        pos_only: Vec::new(),
        pos_or_named: sig.positional.iter().map(to_param).collect(),
        args: sig.variadic.as_ref().map(|p| DocParam {
            name: p.name.to_string(),
            docs: None,
            typ: ty(p),
            default_value: None,
        }),
        named_only: sig.named.iter().map(to_param).collect(),
        kwargs: None,
    };
    let item = DocItem::Member(DocMember::Function(DocFunction {
        docs: DocString::from_docstring(DocStringKind::Starlark, &rf.doc),
        params,
        ret: DocReturn {
            docs: None,
            typ: crate::pluginbuildfile::run_file::param_type_to_ty(&sig.returns),
        },
    }));
    render_doc_item_no_link(name, &item)
}

/// An htvalue default as a Starlark literal for a hover prototype.
fn default_repr(v: &hcore::htvalue::Value) -> String {
    use hcore::htvalue::Value;
    match v {
        Value::Null() => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Int(i) => i.to_string(),
        Value::Uint(u) => u.to_string(),
        Value::Float(f) => f.to_string(),
        Value::String(s) => format!("{s:?}"),
        Value::List(xs) if xs.is_empty() => "[]".to_string(),
        Value::List(_) => "[...]".to_string(),
        Value::Map(m) if m.is_empty() => "{}".to_string(),
        Value::Map(_) => "{...}".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Definition.
// ---------------------------------------------------------------------------

fn link(origin: Range, path: &Path, line: u32) -> Option<LocationLink> {
    let target = Range::new(Position::new(line, 0), Position::new(line, 0));
    Some(LocationLink {
        origin_selection_range: Some(origin),
        target_uri: file_path_to_uri(path)?,
        target_range: target,
        target_selection_range: target,
    })
}

/// heph's definition for a site. `None` passes the stock answer through;
/// `Some(empty)` is heph saying there is nothing to jump to.
pub(crate) fn definition(site: &Site, ctx: &Ctx<'_>) -> Option<Vec<LocationLink>> {
    match site {
        Site::None => None,
        Site::Address { value } => Some(address_definition(value, ctx).into_iter().collect()),
        Site::LoadPath { value } => {
            let origin = ctx.range(&value.token);
            Some(
                load_files(&value.content, ctx)
                    .first()
                    .and_then(|f| link(origin, f, 0))
                    .into_iter()
                    .collect(),
            )
        }
        Site::LoadSymbol { path, value } => {
            let origin = ctx.range(&value.token);
            Some(
                symbol_definition(path, &value.content, origin, ctx)
                    .into_iter()
                    .collect(),
            )
        }
        _ => Some(Vec::new()),
    }
}

/// `//pkg:name` → the line of the target's `name = "name"` in the package's
/// BUILD files; `//pkg` → the BUILD file; `:name` → this package.
fn address_definition(value: &Str, ctx: &Ctx<'_>) -> Option<LocationLink> {
    let origin = ctx.range(&value.token);
    let literal = value.content.as_str();
    let (pkg, name) = if let Some(rest) = literal.strip_prefix("//") {
        match rest.split_once(':') {
            Some((p, n)) => (p.to_string(), Some(n)),
            None => (rest.to_string(), None),
        }
    } else {
        (ctx.package(), Some(literal.strip_prefix(':')?))
    };
    let dir = ctx.shared.root.join(&pkg);
    let files = package_build_files(&dir, &ctx.shared.patterns);
    if let Some(name) = name {
        for file in &files {
            // The open document's text wins over the file on disk.
            let text = match (ctx.path(), file) {
                (Some(open), f) if open == f.as_path() => Some(ctx.text.to_string()),
                _ => std::fs::read_to_string(file).ok(),
            };
            if let Some(line) = text.as_deref().and_then(|t| target_line(t, name)) {
                return link(origin, file, line);
            }
        }
    }
    files.first().and_then(|f| link(origin, f, 0))
}

/// The line of `name = "<name>"` in `text`, read from its tokens (so a file
/// that does not parse still answers).
fn target_line(text: &str, name: &str) -> Option<u32> {
    let tokens = super::lex::lex(text);
    tokens
        .toks
        .windows(3)
        .find(|w| {
            matches!(
                w,
                [a, b, c] if a.kind == Kind::Ident
                    && text.get(a.range()) == Some("name")
                    && b.kind == Kind::Equal
                    && c.kind == Kind::Str
                    && text.get(c.start as usize + 1..(c.end as usize).saturating_sub(1)) == Some(name)
            )
        })
        .and_then(|w| w.first())
        .map(|a| position::newline_position(text, a.start as usize).line)
}

/// A loaded symbol's definition: its `def` line, from the AST of each file the
/// load path names.
fn symbol_definition(path: &str, name: &str, origin: Range, ctx: &Ctx<'_>) -> Option<LocationLink> {
    load_files(path, ctx).iter().find_map(|f| {
        let ast = parse_file(f)?;
        let line = defs_in(&ast).into_iter().find(|(n, _)| n == name)?.1;
        link(origin, f, line)
    })
}

/// Definition of a loaded name used outside its `load` statement: the `def`
/// line in whichever of the package's files defines it. The stock analysis
/// reads one file per package; heph merges them all.
pub(crate) fn loaded_definition(
    ctx: &Ctx<'_>,
    word: Bytes<usize>,
    loaded: &HashMap<String, (String, String)>,
) -> Option<LocationLink> {
    let local = ctx.text.get(word.clone())?;
    let (path, exported) = loaded.get(local)?;
    symbol_definition(path, exported, ctx.range(&word), ctx)
}

// ---------------------------------------------------------------------------
// Formatting (unchanged by the redesign).
// ---------------------------------------------------------------------------

/// The `textDocument/formatting` result: one whole-document edit, an empty list
/// when already formatted, or `null` when the formatter declines (parse error,
/// `heph:fmt skip-file`).
pub(crate) fn format(name: &str, source: &str, engine: &dyn LspEngine) -> serde_json::Value {
    let indent = build_file_indent_from_options(&engine.provider_options("buildfile"))
        .unwrap_or(DEFAULT_INDENT);
    let Ok(formatted) = hxstarlark_fmt::format(
        name,
        source,
        hxstarlark_fmt::Config {
            indent_size: indent,
        },
    ) else {
        return serde_json::Value::Null;
    };
    if formatted == source {
        return serde_json::json!([]);
    }
    serde_json::to_value(vec![TextEdit {
        range: whole_document_range(source),
        new_text: formatted,
    }])
    .unwrap_or(serde_json::Value::Null)
}

/// The range covering the whole document, in UTF-16 code units.
fn whole_document_range(source: &str) -> Range {
    let last_line = source.matches('\n').count() as u32;
    let last_line_text = source.rsplit('\n').next().unwrap_or("");
    let last_col = last_line_text.encode_utf16().count() as u32;
    Range {
        start: Position::new(0, 0),
        end: Position::new(last_line, last_col),
    }
}

// ---------------------------------------------------------------------------
// Paths.
// ---------------------------------------------------------------------------

/// Files in `dir` matching a BUILD pattern, sorted by name (heph's merge order).
pub(crate) fn package_build_files(dir: &Path, patterns: &[glob::Pattern]) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter(|e| {
            patterns
                .iter()
                .any(|p| p.matches(&e.file_name().to_string_lossy()))
        })
        .map(|e| e.path())
        .collect();
    files.sort();
    files
}

/// Package name for a BUILD-file path: its parent dir, workspace-relative,
/// forward-slashed (empty at the root).
pub(crate) fn pkg_of(root: &Path, path: &Path) -> String {
    let parent = path.parent().unwrap_or(path);
    parent
        .strip_prefix(root)
        .unwrap_or(parent)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fmt;
    impl LspEngine for Fmt {
        fn root(&self) -> &Path {
            Path::new("/")
        }
        fn function_registry(&self) -> std::sync::Arc<hplugin::function::FunctionRegistry> {
            std::sync::Arc::default()
        }
        fn driver_schema(&self, _: &str) -> Option<DriverSchema> {
            None
        }
        fn driver_names(&self) -> Vec<String> {
            Vec::new()
        }
        fn provider_names(&self) -> Vec<String> {
            Vec::new()
        }
        fn provider_state_schema(&self, _: &str) -> Option<StateSchema> {
            None
        }
        fn provider_options(&self, _: &str) -> hplugin::config::Options {
            hplugin::config::Options::default()
        }
    }

    #[test]
    fn formatting_replaces_whole_document() {
        let edits = format("BUILD", "target(name=\"a\",deps=[\"x\",\"y\"])\n", &Fmt);
        let edits = edits.as_array().expect("edits");
        assert_eq!(edits.len(), 1, "one whole-document edit");
        assert_eq!(
            edits[0]["newText"],
            "target(\n    name = \"a\",\n    deps = [\"x\", \"y\"],\n)\n"
        );
        assert_eq!(
            edits[0]["range"]["start"],
            serde_json::json!({"line": 0, "character": 0})
        );
    }

    #[test]
    fn formatting_already_formatted_yields_no_edits() {
        assert_eq!(format("BUILD", "x = 1\n", &Fmt), serde_json::json!([]));
    }

    #[test]
    fn formatting_skip_directive_declines() {
        let out = format(
            "BUILD",
            "# heph:fmt skip-file\ntarget(  name=\"a\"  )\n",
            &Fmt,
        );
        assert!(
            out.is_null(),
            "skip-file directive must produce no edits: {out}"
        );
    }

    #[test]
    fn pkg_of_is_workspace_relative() {
        let root = Path::new("/ws");
        assert_eq!(pkg_of(root, Path::new("/ws/a/b/BUILD")), "a/b");
        assert_eq!(pkg_of(root, Path::new("/ws/BUILD")), "");
    }

    #[test]
    fn target_line_reads_tokens_of_a_broken_file() {
        let text = "x = (\ntarget(name = \"a\")\ntarget(name = \"util\", cmd = \n";
        assert_eq!(target_line(text, "util"), Some(2));
        assert_eq!(target_line(text, "nope"), None);
    }

    #[test]
    fn every_site_has_a_merge_policy() {
        assert_eq!(merge_for(&Site::None), Merge::PassThrough);
    }
}
