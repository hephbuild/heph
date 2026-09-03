//! Derive macros for BUILD-file target config specs.
//!
//! * `#[derive(Spec)]` — on a struct, generates `from(&HashMap<String, Value>)`
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
//! * `#[derive(SpecOneOf)]` — on a struct-variant enum, a *tagged* union: one
//!   map whose discriminant key selects the variant, and whose remaining keys
//!   are that variant's fields. Unlike `SpecUnion` it does not guess — the tag
//!   says which shape this is, so a wrong field is reported against the variant
//!   the author named rather than as "nothing matched".
//!
//!   ```ignore
//!   #[derive(SpecOneOf)]
//!   #[spec(tag = "provider")]
//!   enum Source {
//!       StaticEnv { vars: HashMap<String, String> },
//!       Exec { helper: Vec<String>, #[spec(required)] protocol: Protocol },
//!   }
//!   // {"provider": "exec", "helper": ["gh", "auth", "token"], "protocol": "raw"}
//!   ```
//!
//!   This is what makes illegal states unrepresentable: a field belonging to
//!   another variant is an unknown key, and a field the variant requires is
//!   missing at parse time rather than in hand-written cross-field validation.
//!
//! Per-variant overrides (`SpecOneOf`), under `#[spec(...)]`:
//!   * `rename = "name"`  — tag spelling differs from the `snake_case` ident
//!
//! Two more field overrides:
//!   * `alias = "key"`    — a second accepted spelling; both at once is an error
//!   * `flatten`          — the field reads its keys from the enclosing map
//!     (see [`FromSpecMap`]). `Option<T>` is absent when the tag key is.
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
    flatten: bool,
    alias: Option<String>,
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
            } else if meta.path.is_ident("flatten") {
                opts.flatten = true;
            } else if meta.path.is_ident("alias") {
                let lit: LitStr = meta.value()?.parse()?;
                opts.alias = Some(lit.value());
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
    /// Config keys, parallel to `param_tys`. `SpecOneOf` needs the pair to
    /// build a struct type per variant without re-evaluating a `DriverField`.
    keys: Vec<String>,
    /// Schema contributions from `#[spec(flatten)]` fields, each an expression
    /// yielding a `Vec<DriverField>` to splice into the enclosing schema.
    flattened_schema: Vec<proc_macro2::TokenStream>,
}

fn field_codegen(
    fields: &syn::punctuated::Punctuated<syn::Field, syn::token::Comma>,
) -> syn::Result<FieldCodegen> {
    let mut out = FieldCodegen {
        parse_stmts: Vec::new(),
        field_inits: Vec::new(),
        schema_fields: Vec::new(),
        param_tys: Vec::new(),
        keys: Vec::new(),
        flattened_schema: Vec::new(),
    };

    for field in fields {
        let ident = field.ident.as_ref().expect("named field");
        let fty = &field.ty;
        let opts = parse_field_opts(&field.attrs)?;
        let key = opts.rename.clone().unwrap_or_else(|| ident.to_string());
        let doc = doc_string(&field.attrs);
        let required = opts.required;

        // A flattened field has no key of its own: it reads its keys straight
        // out of the enclosing map and removes them, so what remains is still
        // the outer struct's to account for. This is what lets one parser serve
        // both `{"provider": "exec", …}` nested in a list and the same keys
        // spelled inline on the target, with no second key list to drift.
        if opts.flatten {
            let var = quote::format_ident!("__field_{}", ident);
            out.parse_stmts.push(quote! {
                let #var: #fty =
                    <#fty as crate::htspec::FromSpecMap>::from_spec_map(&mut __m)?;
            });
            out.field_inits.push(quote! { #ident: #var });
            out.flattened_schema.push(quote! {
                <#fty as crate::htspec::FromSpecMap>::spec_schema_fields()
            });
            out.param_tys
                .push(quote! { <#fty as crate::htspec::FromSpecValue>::spec_param_type() });
            continue;
        }

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

        // A second accepted spelling for the same field. Both at once is a
        // mistake worth naming: it reads as if they compose, and they do not.
        let lookup = match &opts.alias {
            None => quote! { __m.remove(#key) },
            Some(alias) => quote! {{
                let __primary = __m.remove(#key);
                let __alias = __m.remove(#alias);
                if __primary.is_some() && __alias.is_some() {
                    ::anyhow::bail!(
                        "`{}` and `{}` are two spellings of the same field; set one",
                        #key, #alias
                    );
                }
                __primary.or(__alias)
            }},
        };

        let var = quote::format_ident!("__field_{}", ident);
        out.parse_stmts.push(quote! {
            let #var: #fty = match #lookup {
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
        out.keys.push(key);
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
        flattened_schema,
        ..
    } = cg;
    let unknown = unknown_keys_check();

    Ok(quote! {
        impl #name {
            /// Parse a raw BUILD-file config map into this spec. Unknown keys are
            /// a hard error. Generated by `#[derive(Spec)]`.
            ///
            /// Borrows the config: every field is decoded through
            /// `FromSpecValue::from_spec_value(&Value)`, so nothing is ever moved
            /// out of the map. Taking it by value only forced each driver's
            /// `parse` to deep-clone the `Arc<TargetSpec>`'s config first — for a
            /// `go_compile` target that map holds one entry per transitive lib, so
            /// the clone alone was ~5 allocations per dependency edge, discarded
            /// as soon as `from` returned.
            pub fn from(
                __config: &::std::collections::HashMap<::std::string::String, crate::htvalue::Value>,
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
                let mut __fields: ::std::vec::Vec<crate::htspec::DriverField> =
                    ::std::vec![ #(#schema_fields),* ];
                // A flattened field contributes its own keys inline, because
                // that is how they are spelled on the target.
                #( __fields.extend(#flattened_schema); )*
                crate::htspec::DriverSchema { fields: __fields }
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

/// `#[derive(SpecOneOf)]` — a tagged union: a map whose discriminant key
/// chooses the variant.
///
/// The enum needs `#[spec(tag = "key")]`; every variant must have named fields
/// (a variant with none is written `Foo {}`). Field attributes are the same set
/// `SpecStruct` accepts.
#[proc_macro_derive(SpecOneOf, attributes(spec))]
pub fn derive_spec_one_of(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand_spec_one_of(input) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

/// The `#[spec(tag = "...")]` attribute on a `SpecOneOf` enum.
fn parse_tag_attr(attrs: &[syn::Attribute], name: &syn::Ident) -> syn::Result<String> {
    let mut tag = None;
    for attr in attrs {
        if !attr.path().is_ident("spec") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("tag") {
                let lit: LitStr = meta.value()?.parse()?;
                tag = Some(lit.value());
                Ok(())
            } else {
                Err(meta.error("unknown `spec` option on a SpecOneOf enum"))
            }
        })?;
    }
    tag.ok_or_else(|| {
        syn::Error::new_spanned(
            name,
            "`SpecOneOf` needs a discriminant: #[spec(tag = \"provider\")]",
        )
    })
}

fn expand_spec_one_of(input: DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let name = &input.ident;
    let tag = parse_tag_attr(&input.attrs, name)?;
    let variants = match &input.data {
        Data::Enum(e) => &e.variants,
        _ => {
            return Err(syn::Error::new_spanned(
                name,
                "`SpecOneOf` can only be derived for enums",
            ));
        }
    };

    let mut match_arms = Vec::new();
    let mut struct_tys = Vec::new();
    let mut tag_names = Vec::new();
    let mut all_schema_fields: Vec<proc_macro2::TokenStream> = Vec::new();

    for variant in variants {
        let vident = &variant.ident;
        let fields = match &variant.fields {
            Fields::Named(f) => &f.named,
            _ => {
                return Err(syn::Error::new_spanned(
                    vident,
                    "`SpecOneOf` variants must have named fields (write `Foo {}` for none)",
                ));
            }
        };
        let opts = parse_variant_opts(&variant.attrs)?;
        let key = opts
            .rename
            .unwrap_or_else(|| snake_case(&vident.to_string()));

        let cg = field_codegen(fields)?;
        let FieldCodegen {
            parse_stmts,
            field_inits,
            param_tys,
            keys,
            schema_fields,
            ..
        } = cg;

        match_arms.push(quote! {
            #key => {
                #(#parse_stmts)*
                ::core::result::Result::Ok(#name::#vident { #(#field_inits),* })
            }
        });
        all_schema_fields.push(quote! { #(#schema_fields),* });

        // A struct type per variant, tag field included, so an editor can tell
        // the author which keys this `provider` accepts.
        struct_tys.push(quote! {
            crate::htvalue::signature::ParamType::strukt(::std::vec![
                (#tag, crate::htvalue::signature::ParamType::String),
                #((#keys, #param_tys)),*
            ])
        });
        tag_names.push(key);
    }

    let valid = tag_names.join(", ");
    let unknown_after_variant = unknown_keys_check();

    Ok(quote! {
        impl crate::htspec::FromSpecMap for #name {
            fn from_spec_map(
                __m: &mut ::std::collections::HashMap<&str, &crate::htvalue::Value>,
            ) -> ::anyhow::Result<Self> {
                use ::anyhow::Context as _;
                let __tag_value = __m
                    .remove(#tag)
                    .ok_or_else(|| ::anyhow::anyhow!(
                        "missing `{}`; expected one of: {}", #tag, #valid
                    ))?;
                let __tag = <::std::string::String as crate::htspec::FromSpecValue>::from_spec_value(
                    __tag_value
                ).with_context(|| ::std::format!("parse `{}`", #tag))?;

                // Each arm removes only the keys its own variant owns. Nested
                // in a map that is all ours, `from_spec_value` then rejects
                // whatever is left; flattened into a larger struct, the
                // remainder is the outer struct's to account for.
                match __tag.as_str() {
                    #(#match_arms)*
                    __other => ::anyhow::bail!(
                        "unknown `{}` {:?}; expected one of: {}", #tag, __other, #valid
                    ),
                }
            }

            fn spec_tag() -> &'static str {
                #tag
            }

            fn spec_schema_fields() -> ::std::vec::Vec<crate::htspec::DriverField> {
                let mut __out = ::std::vec![
                    crate::htspec::DriverField {
                        name: #tag.to_string(),
                        ty: crate::htvalue::signature::ParamType::String,
                        doc: ::std::format!("One of: {}", #valid),
                        required: false,
                    }
                ];
                #( __out.extend(::std::vec![ #all_schema_fields ]); )*
                __out
            }
        }

        impl crate::htspec::FromSpecValue for #name {
            fn from_spec_value(__value: &crate::htvalue::Value) -> ::anyhow::Result<Self> {
                let __map = match __value {
                    crate::htvalue::Value::Map(__m) => __m,
                    __other => ::anyhow::bail!("invalid: expected a map, got: {:?}", __other),
                };
                let mut __m: ::std::collections::HashMap<&str, &crate::htvalue::Value> =
                    __map.iter().map(|(__k, __v)| (__k.as_str(), __v)).collect();
                let __parsed = <Self as crate::htspec::FromSpecMap>::from_spec_map(&mut __m)?;
                #unknown_after_variant
                ::core::result::Result::Ok(__parsed)
            }

            fn spec_param_type() -> crate::htvalue::signature::ParamType {
                crate::htspec::flatten_union(::std::vec![ #(#struct_tys),* ])
            }
        }
    })
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
