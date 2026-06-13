//! Derive macros for BUILD-file target config specs.
//!
//! `#[derive(Spec)]` on a struct generates, from a single field list:
//!   * `TargetSpec::from(HashMap<String, Value>) -> anyhow::Result<Self>` — a
//!     parser that pulls each declared key out of the raw config map, parses it
//!     via the field type's [`FromSpecValue`] impl, and hard-fails on any
//!     unknown leftover key; and
//!   * `TargetSpec::schema() -> DriverSchema` — the matching LSP schema, whose
//!     per-field `ParamType` comes from the *same* [`FromSpecValue`] impl, so
//!     parser and schema can never drift.
//!
//! `#[derive(SpecUnion)]` on a newtype-variant enum builds a union: parsing
//! tries each variant's inner type in declared order and the schema renders as
//! `a | b | c`.
//!
//! Per-field overrides (all under `#[spec(...)]`):
//!   * `rename = "key"`   — config key differs from the field name
//!   * `required`         — mark the field required in the schema
//!   * `default = EXPR`   — value used when the key is absent (else `Default`)
//!   * `with = path`      — module exposing `from_spec_value`/`spec_param_type`
//!   * `parse = path`     — function `&Value -> anyhow::Result<T>` for parsing
//!   * `ty = EXPR`        — explicit `ParamType` for the schema
//!
//! Doc comments on a field become its schema `doc`.
//!
//! [`FromSpecValue`]: ../heph/htspec/trait.FromSpecValue.html

use proc_macro::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Expr, Fields, LitStr, Path, parse_macro_input};

/// Per-field `#[spec(...)]` configuration.
#[derive(Default)]
struct FieldOpts {
    rename: Option<String>,
    required: bool,
    default: Option<Expr>,
    with: Option<Path>,
    parse: Option<Path>,
    ty: Option<Expr>,
}

fn parse_field_opts(attrs: &[syn::Attribute]) -> syn::Result<FieldOpts> {
    let mut opts = FieldOpts::default();
    for attr in attrs {
        if !attr.path().is_ident("spec") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename") {
                let lit: LitStr = meta.value()?.parse()?;
                opts.rename = Some(lit.value());
            } else if meta.path.is_ident("required") {
                opts.required = true;
            } else if meta.path.is_ident("default") {
                opts.default = Some(meta.value()?.parse()?);
            } else if meta.path.is_ident("with") {
                opts.with = Some(meta.value()?.parse()?);
            } else if meta.path.is_ident("parse") {
                opts.parse = Some(meta.value()?.parse()?);
            } else if meta.path.is_ident("ty") {
                opts.ty = Some(meta.value()?.parse()?);
            } else {
                return Err(meta.error("unknown `spec` field option"));
            }
            Ok(())
        })?;
    }
    Ok(opts)
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

/// `#[derive(Spec)]` — generate `from()` + `schema()` for a struct of config fields.
#[proc_macro_derive(Spec, attributes(spec))]
pub fn derive_spec(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand_spec(input) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand_spec(input: DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let name = &input.ident;
    let fields = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(named) => &named.named,
            _ => {
                return Err(syn::Error::new_spanned(
                    name,
                    "`Spec` requires a struct with named fields",
                ));
            }
        },
        _ => {
            return Err(syn::Error::new_spanned(
                name,
                "`Spec` can only be derived for structs",
            ));
        }
    };

    let mut parse_stmts = Vec::new();
    let mut field_inits = Vec::new();
    let mut schema_fields = Vec::new();

    for field in fields {
        let ident = field.ident.as_ref().expect("named field");
        let fty = &field.ty;
        let opts = parse_field_opts(&field.attrs)?;
        let key = opts.rename.clone().unwrap_or_else(|| ident.to_string());
        let doc = doc_string(&field.attrs);
        let required = opts.required;

        // Expression that parses a present `&Value` into the field type.
        let parse_call = if let Some(p) = &opts.parse {
            quote! { #p(__v) }
        } else if let Some(w) = &opts.with {
            quote! { #w::from_spec_value(__v) }
        } else {
            quote! { <#fty as crate::htspec::FromSpecValue>::from_spec_value(__v) }
        };

        // Expression producing the default when the key is absent.
        let default_expr = match &opts.default {
            Some(e) => quote! { #e },
            None => quote! { <#fty as ::core::default::Default>::default() },
        };

        // `ParamType` for the schema entry.
        let param_ty = if let Some(t) = &opts.ty {
            quote! { #t }
        } else if let Some(w) = &opts.with {
            quote! { #w::spec_param_type() }
        } else {
            quote! { <#fty as crate::htspec::FromSpecValue>::spec_param_type() }
        };

        let var = quote::format_ident!("__field_{}", ident);
        parse_stmts.push(quote! {
            let #var: #fty = match __m.remove(#key) {
                ::core::option::Option::Some(__v) => {
                    (#parse_call).with_context(|| ::std::format!("parse `{}`", #key))?
                }
                ::core::option::Option::None => #default_expr,
            };
        });
        field_inits.push(quote! { #ident: #var });

        schema_fields.push(quote! {
            crate::engine::driver::DriverField {
                name: #key.to_string(),
                ty: #param_ty,
                doc: #doc.to_string(),
                required: #required,
            }
        });
    }

    Ok(quote! {
        impl #name {
            /// Parse a raw BUILD-file config map into this spec. Unknown keys are
            /// a hard error. Generated by `#[derive(Spec)]`.
            pub fn from(
                __config: ::std::collections::HashMap<::std::string::String, crate::htvalue::Value>,
            ) -> ::anyhow::Result<Self> {
                use ::anyhow::Context as _;
                let mut __m: ::std::collections::HashMap<&str, &crate::htvalue::Value> =
                    __config.iter().map(|(__k, __v)| (__k.as_str(), __v)).collect();

                #(#parse_stmts)*

                if !__m.is_empty() {
                    let mut __unknown: ::std::vec::Vec<&str> = __m.into_keys().collect();
                    __unknown.sort_unstable();
                    ::anyhow::bail!("unknown entries found: {:?}", __unknown);
                }

                ::core::result::Result::Ok(Self { #(#field_inits),* })
            }

            /// Declarative LSP schema for this spec; field types mirror the
            /// `FromSpecValue` impls used by `from`. Generated by `#[derive(Spec)]`.
            pub fn schema() -> crate::engine::driver::DriverSchema {
                crate::engine::driver::DriverSchema {
                    fields: ::std::vec![ #(#schema_fields),* ],
                }
            }
        }
    })
}

/// `#[derive(SpecUnion)]` — a config value accepting one of several shapes.
///
/// Each enum variant must be a newtype (one unnamed field). Parsing tries the
/// variants in declared order, returning the first that succeeds; the schema is
/// the union of the variants' element types.
#[proc_macro_derive(SpecUnion)]
pub fn derive_spec_union(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand_spec_union(input) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand_spec_union(input: DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let name = &input.ident;
    let variants = match &input.data {
        Data::Enum(e) => &e.variants,
        _ => {
            return Err(syn::Error::new_spanned(
                name,
                "`SpecUnion` can only be derived for enums",
            ));
        }
    };

    let mut try_arms = Vec::new();
    let mut member_tys = Vec::new();
    for variant in variants {
        let vident = &variant.ident;
        let inner = match &variant.fields {
            Fields::Unnamed(f) if f.unnamed.len() == 1 => &f.unnamed[0].ty,
            _ => {
                return Err(syn::Error::new_spanned(
                    vident,
                    "`SpecUnion` variants must be newtypes (exactly one unnamed field)",
                ));
            }
        };
        try_arms.push(quote! {
            if let ::core::result::Result::Ok(__x) =
                <#inner as crate::htspec::FromSpecValue>::from_spec_value(__v)
            {
                return ::core::result::Result::Ok(#name::#vident(__x));
            }
        });
        member_tys.push(quote! {
            <#inner as crate::htspec::FromSpecValue>::spec_param_type()
        });
    }

    Ok(quote! {
        impl crate::htspec::FromSpecValue for #name {
            fn from_spec_value(__v: &crate::htvalue::Value) -> ::anyhow::Result<Self> {
                #(#try_arms)*
                ::anyhow::bail!(
                    "invalid: expected {}, got: {:?}",
                    <Self as crate::htspec::FromSpecValue>::spec_param_type().render(),
                    __v
                )
            }

            fn spec_param_type() -> crate::htvalue::signature::ParamType {
                crate::htspec::flatten_union(::std::vec![ #(#member_tys),* ])
            }
        }
    })
}
