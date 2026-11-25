//! `#[derive(Record)]`: the field table of a command's JSON output type.
//!
//! Reads the struct through `serde_derive_internals`, serde's own attribute
//! model, so the names in the table are exactly the keys `derive(Serialize)`
//! writes: `rename` and `rename_all` are honoured, `skip`/`skip_serializing`
//! fields are left out, `skip_serializing_if` is allowed (it only ever drops a
//! key, so the keys stay a subset of the table), and `flatten` is refused
//! (its keys are not known here).
//!
//! Per field, `#[record(...)]` takes one ident:
//!
//! * nothing — `free`, in the default set: the command already computes it;
//! * `opt_in` — `free`, but only printed when `--fields` names it;
//! * `resolved` — needs a spec or def the command would not otherwise fetch;
//! * `built` — needs the target's inputs hashed, so its dependencies build.
//!
//! A misspelt ident is a compile error. The doc comment on a field is its
//! description in `--help` and `--fields help`.
//!
//! The derive emits `impl output::Record` (the field table) and one typed
//! `output::FieldId` constant per field, named after the field in upper snake
//! case (`hashin` → `HASHIN`), for `JsonArgs::wants`.

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{format_ident, quote};
use serde_derive_internals::{Ctxt, Derive, ast, attr::TagType};
use syn::{DeriveInput, Expr, GenericParam, parse_macro_input};

/// The most fields one `Record` may have: the selection is a `u64` bitmask.
const MAX_FIELDS: usize = 64;

/// Names `--fields` gives a meaning of its own, so no field may take them.
const RESERVED: &[&str] = &["all", "help"];

#[proc_macro_derive(Record, attributes(record))]
pub fn derive_record(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// `#[derive(Shape)]`: the keys and JSON kinds of a type nested inside a
/// `Record`'s field (a list element, an optional object, an enum), as a
/// hidden inherent `__OUTPUT_SHAPE` constant that the field snapshot reads.
/// It emits no `::output` path, so a crate that does not depend on `output`
/// can derive it.
///
/// Each entry is `(path, kind)`, the path relative to the value: `.key` for a
/// struct's field; for an externally tagged enum, `("", "\"name\"")` per unit
/// variant, `.name` per newtype variant, `.name.key` per struct-variant
/// field; for an internally tagged one, `(".tag", "\"name\"")` per variant and
/// `{tag=name}.key` per variant field.
#[proc_macro_derive(Shape)]
pub fn derive_shape(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand_shape(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// What `#[record(...)]` declares about one field.
#[derive(Clone, Copy)]
enum Tier {
    Free { default: bool },
    Resolved,
    Built,
}

fn expand(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let cx = Ctxt::new();
    let private = syn::Ident::new("__private", Span::call_site());
    let container = ast::Container::from_ast(&cx, input, Derive::Serialize, &private);
    // A bad serde attribute is reported twice, by serde's derive and here:
    // `check` must run (a `Ctxt` dropped unchecked panics), and once it has
    // failed the container is half-read, so expanding on would only add
    // errors of our own about a type that does not compile anyway.
    cx.check()?;
    let Some(container) = container else {
        return Err(syn::Error::new_spanned(
            input,
            "derive(Record) could not read this type's serde attributes",
        ));
    };

    if container.attrs.transparent() {
        return Err(refused(input, "`#[serde(transparent)]`"));
    }
    if container.attrs.type_into().is_some() {
        return Err(refused(input, "`#[serde(into = ...)]`"));
    }
    if container.attrs.remote().is_some() {
        return Err(refused(input, "`#[serde(remote = ...)]`"));
    }
    if !matches!(container.attrs.tag(), TagType::External) {
        return Err(refused(
            input,
            "a serde `tag`/`content`/`untagged` attribute (it writes a key the field table cannot list)",
        ));
    }

    let fields = match &container.data {
        ast::Data::Struct(ast::Style::Struct, fields) => fields,
        _ => {
            return Err(syn::Error::new_spanned(
                input,
                "derive(Record) needs a struct with named fields: a record serializes as one JSON object",
            ));
        }
    };

    // Only lifetimes: `Record::Static` names the type with every lifetime
    // `'static`, and a type parameter would make the field table depend on it.
    for param in &input.generics.params {
        if !matches!(param, GenericParam::Lifetime(_)) {
            return Err(syn::Error::new_spanned(
                param,
                "derive(Record) supports lifetime parameters only",
            ));
        }
    }

    let ident = &input.ident;
    let statics = input.generics.params.iter().map(|_| quote!('static));
    let static_ty = if input.generics.params.is_empty() {
        quote!(#ident)
    } else {
        quote!(#ident<#(#statics),*>)
    };

    let mut entries = Vec::new();
    let mut ids = Vec::new();
    let mut shape = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for field in fields {
        let tier = record_attr(&field.original.attrs)?;
        if field.attrs.skip_serializing() {
            if tier.is_some() {
                return Err(syn::Error::new_spanned(
                    field.original,
                    "`#[record(...)]` on a field serde never serializes",
                ));
            }
            continue;
        }
        if field.attrs.flatten() {
            return Err(syn::Error::new_spanned(
                field.original,
                "derive(Record) refuses `#[serde(flatten)]`: the flattened keys would not be in the field table; list them on a view type instead",
            ));
        }
        let name = field.attrs.name().serialize_name().value.clone();
        check_name(field.original, &name, &names)?;
        names.push(name.clone());

        let index = entries.len();
        let tier = tier.unwrap_or(Tier::Free { default: true });
        let (tier_tokens, default) = match tier {
            Tier::Free { default } => (quote!(::output::Tier::Free), default),
            Tier::Resolved => (quote!(::output::Tier::Resolved), false),
            Tier::Built => (quote!(::output::Tier::Built), false),
        };
        let doc = doc_string(&field.original.attrs);
        let ty = field.ty;
        let shape_key = format!(".{name}");
        let kind = kind(ty);
        shape.push(quote!((#shape_key, #kind)));
        entries.push(quote! {
            ::output::Field {
                name: #name,
                doc: #doc,
                tier: #tier_tokens,
                default: #default,
                kind: #kind,
            }
        });

        let member = match &field.member {
            syn::Member::Named(ident) => ident.to_string(),
            syn::Member::Unnamed(i) => i.index.to_string(),
        };
        let const_name = format_ident!("{}", member.trim_start_matches("r#").to_uppercase());
        let doc = format!("The `{name}` field, for `JsonArgs::wants`.");
        ids.push(quote! {
            #[doc = #doc]
            pub const #const_name: ::output::FieldId<#static_ty> =
                ::output::FieldId::__new(#index);
        });
    }

    if entries.len() > MAX_FIELDS {
        return Err(syn::Error::new_spanned(
            input,
            format!("derive(Record) supports at most {MAX_FIELDS} serialized fields"),
        ));
    }

    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    Ok(quote! {
        impl #impl_generics ::output::Record for #ident #ty_generics #where_clause {
            type Static = #static_ty;
            const FIELDS: &'static [::output::Field] = &[#(#entries),*];
        }

        impl #impl_generics #ident #ty_generics #where_clause {
            #(#ids)*

            #[doc(hidden)]
            pub const __OUTPUT_SHAPE: &'static [(&'static str, &'static str)] = &[#(#shape),*];
        }
    })
}

fn refused(input: &DeriveInput, what: &str) -> syn::Error {
    syn::Error::new_spanned(
        &input.ident,
        format!("derive(Record) refuses {what}: every key must be a listed field"),
    )
}

/// The `#[record(...)]` of one field, if it has one.
fn record_attr(attrs: &[syn::Attribute]) -> syn::Result<Option<Tier>> {
    let mut tier = None;
    for attr in attrs {
        if !attr.path().is_ident("record") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            let next = if meta.path.is_ident("free") {
                Tier::Free { default: true }
            } else if meta.path.is_ident("opt_in") {
                Tier::Free { default: false }
            } else if meta.path.is_ident("resolved") {
                Tier::Resolved
            } else if meta.path.is_ident("built") {
                Tier::Built
            } else {
                return Err(meta.error(
                    "unknown record tier; expected one of `free`, `opt_in`, `resolved`, `built`",
                ));
            };
            if tier.replace(next).is_some() {
                return Err(meta.error("a field has exactly one record tier"));
            }
            Ok(())
        })?;
    }
    Ok(tier)
}

/// A serialized name `--fields` can select: not reserved, not a duplicate,
/// and free of the characters its parser splits on or reads as a prefix.
fn check_name(field: &syn::Field, name: &str, seen: &[String]) -> syn::Result<()> {
    let problem = if name.is_empty() {
        Some("an empty name".to_string())
    } else if RESERVED.contains(&name) {
        Some(format!("`{name}`, which `--fields` reserves"))
    } else if name.starts_with('+') || name.contains(',') {
        Some(format!(
            "`{name}`: `--fields` splits on `,` and reads `+` as \"add\""
        ))
    } else if seen.iter().any(|s| s == name) {
        Some(format!("`{name}` twice"))
    } else {
        None
    };
    match problem {
        Some(problem) => Err(syn::Error::new_spanned(
            field,
            format!("derive(Record): a field serializes as {problem}"),
        )),
        None => Ok(()),
    }
}

/// Join the text of all `#[doc = "..."]` attributes, trimming each line.
fn doc_string(attrs: &[syn::Attribute]) -> String {
    let mut lines = Vec::new();
    for attr in attrs {
        if !attr.path().is_ident("doc") {
            continue;
        }
        if let syn::Meta::NameValue(nv) = &attr.meta
            && let Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            }) = &nv.value
        {
            lines.push(s.value().trim().to_string());
        }
    }
    lines.join(" ").trim().to_string()
}

/// The JSON kind a field's type serializes as: what the snapshot pins, so a
/// `use` rename, a borrow turned owned (`&str` → `String`) or `PathBuf` for
/// `String` moves nothing. `string`, `number`, `bool`, `any`
/// (`serde_json::Value`), `null`; `K?` nullable, `[K]` array, `{K}` map;
/// `@Name` any other type, which the snapshot test resolves through its
/// registry of nested types (their `__OUTPUT_SHAPE`, from this derive or
/// `derive(Shape)`). A `serialize_with` is trusted to keep the type's kind
/// (`output::ser`'s do).
fn kind(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Reference(r) => kind(&r.elem),
        syn::Type::Paren(p) => kind(&p.elem),
        syn::Type::Group(g) => kind(&g.elem),
        syn::Type::Slice(s) => format!("[{}]", kind(&s.elem)),
        syn::Type::Array(a) => format!("[{}]", kind(&a.elem)),
        syn::Type::Tuple(t) if t.elems.is_empty() => "null".to_string(),
        syn::Type::Tuple(t) => {
            let elems: Vec<String> = t.elems.iter().map(kind).collect();
            format!("({})", elems.join(","))
        }
        syn::Type::Path(p) if p.qself.is_none() => path_kind(&p.path),
        other => format!("@{}", quote!(#other)),
    }
}

fn path_kind(path: &syn::Path) -> String {
    let Some(last) = path.segments.last() else {
        return "@?".to_string();
    };
    let args: Vec<&syn::Type> = match &last.arguments {
        syn::PathArguments::AngleBracketed(a) => a
            .args
            .iter()
            .filter_map(|arg| match arg {
                syn::GenericArgument::Type(t) => Some(t),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    let arg = |i: usize| args.get(i).map_or_else(|| "any".to_string(), |t| kind(t));
    let name = last.ident.to_string();
    match name.as_str() {
        "String" | "str" | "PathBuf" | "Path" | "char" => "string".to_string(),
        "u8" | "u16" | "u32" | "u64" | "u128" | "usize" | "i8" | "i16" | "i32" | "i64" | "i128"
        | "isize" | "f32" | "f64" => "number".to_string(),
        "bool" => "bool".to_string(),
        // `serde_json::Value`: whatever JSON the command passes through.
        "Value" => "any".to_string(),
        "Option" => format!("{}?", arg(0)),
        "Vec" | "VecDeque" | "BTreeSet" | "HashSet" | "IndexSet" => format!("[{}]", arg(0)),
        "BTreeMap" | "HashMap" | "IndexMap" => format!("{{{}}}", arg(1)),
        "Box" | "Arc" | "Rc" | "Cow" => arg(0),
        _ => format!("@{name}"),
    }
}

/// `#[derive(Shape)]`: the `__OUTPUT_SHAPE` of a type nested in a record.
fn expand_shape(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let cx = Ctxt::new();
    let private = syn::Ident::new("__private", Span::call_site());
    let container = ast::Container::from_ast(&cx, input, Derive::Serialize, &private);
    cx.check()?;
    let Some(container) = container else {
        return Err(syn::Error::new_spanned(
            input,
            "derive(Shape) could not read this type's serde attributes",
        ));
    };
    let refuse = |what: &str| {
        syn::Error::new_spanned(
            &input.ident,
            format!("derive(Shape) cannot describe {what}; give the type to the snapshot by hand"),
        )
    };
    if container.attrs.transparent() || container.attrs.type_into().is_some() {
        return Err(refuse("`transparent` or `into`"));
    }
    let mut entries = Vec::new();
    match &container.data {
        ast::Data::Struct(ast::Style::Struct, fields) => {
            if !matches!(container.attrs.tag(), TagType::External) {
                return Err(refuse("a tagged struct"));
            }
            struct_entries(fields, "", &mut entries)?;
        }
        ast::Data::Struct(ast::Style::Newtype, fields) => {
            entries.push((String::new(), newtype_kind(fields)));
        }
        ast::Data::Enum(variants) => {
            for v in variants.iter().filter(|v| !v.attrs.skip_serializing()) {
                let name = v.attrs.name().serialize_name().value.clone();
                match (container.attrs.tag(), v.style) {
                    (TagType::External, ast::Style::Unit) => {
                        entries.push((String::new(), format!("\"{name}\"")));
                    }
                    (TagType::External, ast::Style::Newtype) => {
                        entries.push((format!(".{name}"), newtype_kind(&v.fields)));
                    }
                    (TagType::External, ast::Style::Struct) => {
                        struct_entries(&v.fields, &format!(".{name}"), &mut entries)?;
                    }
                    (TagType::Internal { tag }, ast::Style::Unit | ast::Style::Struct) => {
                        entries.push((format!(".{tag}"), format!("\"{name}\"")));
                        struct_entries(&v.fields, &format!("{{{tag}={name}}}"), &mut entries)?;
                    }
                    _ => return Err(refuse("this enum representation")),
                }
            }
        }
        _ => return Err(refuse("a tuple or unit struct")),
    }
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    let ident = &input.ident;
    let entries = entries.iter().map(|(path, kind)| quote!((#path, #kind)));
    Ok(quote! {
        impl #impl_generics #ident #ty_generics #where_clause {
            #[doc(hidden)]
            pub const __OUTPUT_SHAPE: &'static [(&'static str, &'static str)] = &[#(#entries),*];
        }
    })
}

/// The kind of a newtype's one field.
fn newtype_kind(fields: &[ast::Field<'_>]) -> String {
    fields
        .first()
        .map_or_else(|| "any".to_string(), |f| kind(f.ty))
}

/// One `(.key, kind)` per serialized field, each key under `prefix`.
fn struct_entries(
    fields: &[ast::Field<'_>],
    prefix: &str,
    out: &mut Vec<(String, String)>,
) -> syn::Result<()> {
    for field in fields.iter().filter(|f| !f.attrs.skip_serializing()) {
        if field.attrs.flatten() {
            return Err(syn::Error::new_spanned(
                field.original,
                "derive(Shape) refuses `#[serde(flatten)]`: the flattened keys are not known here",
            ));
        }
        let name = field.attrs.name().serialize_name().value.clone();
        out.push((format!("{prefix}.{name}"), kind(field.ty)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! The refusals' messages. That each one fails a build is the
    //! `compile_fail` doctests' job, in `output`'s `src/derive_refusals.rs`.

    use super::expand;
    use syn::{DeriveInput, parse_quote};

    fn refusal(input: DeriveInput) -> String {
        match expand(&input) {
            Ok(_) => panic!("derive(Record) accepted {}", input.ident),
            Err(err) => err.to_string(),
        }
    }

    #[test]
    fn refusals_say_what_was_refused() {
        let cases: [(DeriveInput, &str); 4] = [
            (
                parse_quote! { struct Outer { #[serde(flatten)] inner: Inner } },
                "derive(Record) refuses `#[serde(flatten)]`: the flattened keys would not be in the field table; list them on a view type instead",
            ),
            (
                parse_quote! { struct Row { #[record(bulit)] hashin: Option<String> } },
                "unknown record tier; expected one of `free`, `opt_in`, `resolved`, `built`",
            ),
            (
                parse_quote! { struct Row { #[serde(rename = "all")] everything: u8 } },
                "derive(Record): a field serializes as `all`, which `--fields` reserves",
            ),
            (
                parse_quote! { enum Row { A } },
                "derive(Record) needs a struct with named fields: a record serializes as one JSON object",
            ),
        ];
        for (input, want) in cases {
            assert_eq!(refusal(input), want);
        }
    }
}
