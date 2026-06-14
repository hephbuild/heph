//! Derive macros for BUILD-file target config specs.
//!
//! * `#[derive(Spec)]` — on a struct, generates `from(HashMap<String, Value>)`
//!   (the top-level config parser) and `schema() -> DriverSchema` (the matching
//!   LSP schema). Per-field `ParamType`s come from each field type's
//!   [`FromSpecValue`] impl, so parser and schema can't drift.
//! * `#[derive(SpecStruct)]` — on a struct, generates a [`FromSpecValue`] impl
//!   for a *nested* object: a `Value::Map` with well-known keys, each parsed
//!   into a field; unknown keys are rejected. Its `spec_param_type` renders as
//!   `map[<union of field types>]`.
//! * `#[derive(SpecEnum)]` — on a unit-variant enum, generates a
//!   [`FromSpecValue`] that parses a string into a variant (variant name lowered
//!   to `snake_case` by default). `spec_param_type` is `string`.
//! * `#[derive(SpecUnion)]` — on a newtype-variant enum, a union: parsing tries
//!   each variant's inner type in declared order; schema renders `a | b | c`.
//!
//! Per-field overrides (`Spec` / `SpecStruct`), all under `#[spec(...)]`:
//!   * `rename = "key"`   — config key differs from the field name
//!   * `required`         — mark the field required in the schema (`Spec` only)
//!   * `default = EXPR`   — value used when the key is absent (else `Default`)
//!   * `with = path`      — module exposing `from_spec_value`/`spec_param_type`
//!   * `parse = path`     — function `&Value -> anyhow::Result<T>` for parsing
//!   * `ty = EXPR`        — explicit `ParamType` for the schema
//!
//! Doc comments on a field become its schema `doc`.
//!
//! Per-variant overrides (`SpecEnum`), under `#[spec(...)]`:
//!   * `rename = "name"`  — string spelling differs from the `snake_case` ident
//!   * `skip`             — variant is never parsed from a string (used only as
//!     the `#[default]` / absent value, e.g. a `None` variant)

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

/// Per-variant `#[spec(...)]` configuration for `SpecEnum`.
#[derive(Default)]
struct VariantOpts {
    rename: Option<String>,
    skip: bool,
}

fn parse_variant_opts(attrs: &[syn::Attribute]) -> syn::Result<VariantOpts> {
    let mut opts = VariantOpts::default();
    for attr in attrs {
        if !attr.path().is_ident("spec") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename") {
                let lit: LitStr = meta.value()?.parse()?;
                opts.rename = Some(lit.value());
            } else if meta.path.is_ident("skip") {
                opts.skip = true;
            } else {
                return Err(meta.error("unknown `spec` variant option"));
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

/// Whether a variant carries the std `#[default]` attribute.
fn has_default_attr(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| a.path().is_ident("default"))
}

/// `CamelCase` → `snake_case`, the default `SpecEnum` variant spelling.
fn snake_case(ident: &str) -> String {
    let mut out = String::new();
    for (i, c) in ident.char_indices() {
        if c.is_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// Codegen shared by `Spec` and `SpecStruct`: for each named field, a statement
/// pulling its key out of `__m: HashMap<&str, &Value>` (parse or default), the
/// struct-init fragment, the schema `DriverField`, and the field's `ParamType`.
struct FieldCodegen {
    parse_stmts: Vec<proc_macro2::TokenStream>,
    field_inits: Vec<proc_macro2::TokenStream>,
    schema_fields: Vec<proc_macro2::TokenStream>,
    param_tys: Vec<proc_macro2::TokenStream>,
}

fn field_codegen(
    fields: &syn::punctuated::Punctuated<syn::Field, syn::token::Comma>,
) -> syn::Result<FieldCodegen> {
    let mut out = FieldCodegen {
        parse_stmts: Vec::new(),
        field_inits: Vec::new(),
        schema_fields: Vec::new(),
        param_tys: Vec::new(),
    };

    for field in fields {
        let ident = field.ident.as_ref().expect("named field");
        let fty = &field.ty;
        let opts = parse_field_opts(&field.attrs)?;
        let key = opts.rename.clone().unwrap_or_else(|| ident.to_string());
        let doc = doc_string(&field.attrs);
        let required = opts.required;

        let parse_call = if let Some(p) = &opts.parse {
            quote! { #p(__v) }
        } else if let Some(w) = &opts.with {
            quote! { #w::from_spec_value(__v) }
        } else {
            quote! { <#fty as crate::htspec::FromSpecValue>::from_spec_value(__v) }
        };

        let default_expr = match &opts.default {
            Some(e) => quote! { #e },
            None => quote! { <#fty as ::core::default::Default>::default() },
        };

        let param_ty = if let Some(t) = &opts.ty {
            quote! { #t }
        } else if let Some(w) = &opts.with {
            quote! { #w::spec_param_type() }
        } else {
            quote! { <#fty as crate::htspec::FromSpecValue>::spec_param_type() }
        };

        // A required field with no value is a hard error; otherwise the
        // absent value falls back to its default.
        let absent_arm = if required {
            quote! {
                ::core::option::Option::None =>
                    ::anyhow::bail!("missing required `{}`", #key),
            }
        } else {
            quote! { ::core::option::Option::None => #default_expr, }
        };

        let var = quote::format_ident!("__field_{}", ident);
        out.parse_stmts.push(quote! {
            let #var: #fty = match __m.remove(#key) {
                ::core::option::Option::Some(__v) => {
                    (#parse_call).with_context(|| ::std::format!("parse `{}`", #key))?
                }
                #absent_arm
            };
        });
        out.field_inits.push(quote! { #ident: #var });
        out.schema_fields.push(quote! {
            crate::htspec::DriverField {
                name: #key.to_string(),
                ty: #param_ty,
                doc: #doc.to_string(),
                required: #required,
            }
        });
        out.param_tys.push(param_ty);
    }

    Ok(out)
}

/// The unknown-leftover-key guard shared by `Spec` / `SpecStruct`.
fn unknown_keys_check() -> proc_macro2::TokenStream {
    quote! {
        if !__m.is_empty() {
            let mut __unknown: ::std::vec::Vec<&str> = __m.into_keys().collect();
            __unknown.sort_unstable();
            ::anyhow::bail!("unknown entries found: {:?}", __unknown);
        }
    }
}

fn named_fields<'a>(
    input: &'a DeriveInput,
    derive: &str,
) -> syn::Result<&'a syn::punctuated::Punctuated<syn::Field, syn::token::Comma>> {
    match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(named) => Ok(&named.named),
            _ => Err(syn::Error::new_spanned(
                &input.ident,
                format!("`{derive}` requires a struct with named fields"),
            )),
        },
        _ => Err(syn::Error::new_spanned(
            &input.ident,
            format!("`{derive}` can only be derived for structs"),
        )),
    }
}

/// `#[derive(Spec)]` — generate `from()` + `schema()` for a top-level config struct.
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
    let fields = named_fields(&input, "Spec")?;
    let cg = field_codegen(fields)?;
    let FieldCodegen {
        parse_stmts,
        field_inits,
        schema_fields,
        ..
    } = cg;
    let unknown = unknown_keys_check();

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
                #unknown

                ::core::result::Result::Ok(Self { #(#field_inits),* })
            }

            /// Declarative LSP schema for this spec; field types mirror the
            /// `FromSpecValue` impls used by `from`. Generated by `#[derive(Spec)]`.
            pub fn schema() -> crate::htspec::DriverSchema {
                crate::htspec::DriverSchema {
                    fields: ::std::vec![ #(#schema_fields),* ],
                }
            }
        }
    })
}

/// `#[derive(SpecStruct)]` — a nested config object: a `Value::Map` with
/// well-known keys parsed into struct fields. Implements [`FromSpecValue`].
#[proc_macro_derive(SpecStruct, attributes(spec))]
pub fn derive_spec_struct(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand_spec_struct(input) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand_spec_struct(input: DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let name = &input.ident;
    let fields = named_fields(&input, "SpecStruct")?;
    let cg = field_codegen(fields)?;
    let FieldCodegen {
        parse_stmts,
        field_inits,
        param_tys,
        ..
    } = cg;
    let unknown = unknown_keys_check();

    Ok(quote! {
        impl crate::htspec::FromSpecValue for #name {
            fn from_spec_value(__value: &crate::htvalue::Value) -> ::anyhow::Result<Self> {
                use ::anyhow::Context as _;
                let __map = match __value {
                    crate::htvalue::Value::Map(__m) => __m,
                    __other => ::anyhow::bail!(
                        "invalid: expected a map, got: {:?}", __other
                    ),
                };
                let mut __m: ::std::collections::HashMap<&str, &crate::htvalue::Value> =
                    __map.iter().map(|(__k, __v)| (__k.as_str(), __v)).collect();

                #(#parse_stmts)*
                #unknown

                ::core::result::Result::Ok(Self { #(#field_inits),* })
            }

            fn spec_param_type() -> crate::htvalue::signature::ParamType {
                crate::htvalue::signature::ParamType::map(
                    crate::htspec::flatten_union(::std::vec![ #(#param_tys),* ])
                )
            }
        }
    })
}

/// `#[derive(SpecEnum)]` — a string-valued enum: each unit variant maps to its
/// `snake_case` name (override with `#[spec(rename = "...")]`). A `#[default]`
/// variant additionally accepts a null value; `#[spec(skip)]` excludes a variant
/// from string parsing. Implements [`FromSpecValue`].
#[proc_macro_derive(SpecEnum, attributes(spec))]
pub fn derive_spec_enum(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand_spec_enum(input) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand_spec_enum(input: DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let name = &input.ident;
    let variants = match &input.data {
        Data::Enum(e) => &e.variants,
        _ => {
            return Err(syn::Error::new_spanned(
                name,
                "`SpecEnum` can only be derived for enums",
            ));
        }
    };

    let mut match_arms = Vec::new();
    let mut valid_names = Vec::new();
    let mut has_default = false;
    for variant in variants {
        let vident = &variant.ident;
        if !matches!(variant.fields, Fields::Unit) {
            return Err(syn::Error::new_spanned(
                vident,
                "`SpecEnum` variants must be unit variants",
            ));
        }
        if has_default_attr(&variant.attrs) {
            has_default = true;
        }
        let opts = parse_variant_opts(&variant.attrs)?;
        if opts.skip {
            continue;
        }
        let key = opts
            .rename
            .unwrap_or_else(|| snake_case(&vident.to_string()));
        match_arms.push(quote! { #key => ::core::result::Result::Ok(#name::#vident), });
        valid_names.push(key);
    }

    let valid = valid_names.join(", ");
    let null_arm = if has_default {
        quote! {
            if let crate::htvalue::Value::Null() = __value {
                return ::core::result::Result::Ok(<Self as ::core::default::Default>::default());
            }
        }
    } else {
        quote! {}
    };

    Ok(quote! {
        impl crate::htspec::FromSpecValue for #name {
            fn from_spec_value(__value: &crate::htvalue::Value) -> ::anyhow::Result<Self> {
                #null_arm
                let __s = <::std::string::String as crate::htspec::FromSpecValue>::from_spec_value(__value)?;
                match __s.as_str() {
                    #(#match_arms)*
                    __other => ::anyhow::bail!(
                        "invalid: expected one of [{}], got: {:?}", #valid, __other
                    ),
                }
            }

            fn spec_param_type() -> crate::htvalue::signature::ParamType {
                crate::htvalue::signature::ParamType::String
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
            Fields::Unnamed(f) if f.unnamed.len() == 1 => {
                &f.unnamed.first().expect("len == 1 checked").ty
            }
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
