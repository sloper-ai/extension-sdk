use std::collections::BTreeSet;

use proc_macro2::{
    Span,
    TokenStream,
};
use quote::{
    quote,
    quote_spanned,
};
use syn::{
    Attribute,
    Data,
    DeriveInput,
    Expr,
    Fields,
    GenericArgument,
    Lit,
    LitStr,
    Path,
    PathArguments,
    Type,
    spanned::Spanned,
};

#[derive(Default)]
struct Serde {
    rename: Option<String>,
    rename_all: Option<String>,
    default: bool,
    skip: bool,
    flatten: bool,
}

#[derive(Default)]
struct Options {
    members: Vec<(String, serde_json::Value)>,
    item_format: Option<String>,
    crate_path: Option<Path>,
}

struct Shape {
    kind: TokenStream,
    rest: TokenStream,
    source: TokenStream,
    optional: bool,
    open: TokenStream,
}

pub(super) fn derive(input: &DeriveInput, derive_resource: bool) -> syn::Result<TokenStream> {
    let name = &input.ident;
    if !input.generics.lifetimes().collect::<Vec<_>>().is_empty() {
        return Err(syn::Error::new_spanned(
            &input.generics,
            "Schemas own their data; replace borrowed fields and lifetime parameters with owned values.",
        ));
    }
    let type_options = options(&input.attrs)?;
    let sdk: Path = type_options
        .crate_path
        .clone()
        .unwrap_or_else(|| syn::parse_quote!(::sloper_extension));
    let type_serde = serde(&input.attrs, "type")?;
    let mut generics = input.generics.clone();
    for parameter in generics.type_params_mut() {
        parameter.bounds.push(syn::parse_quote!(#sdk::Schema));
    }
    let (impl_generics, type_generics, where_clause) = generics.split_for_impl();
    let (kind, rest, source, open) = definition(input, &type_options, &type_serde, &sdk)?;
    let resource_part = if derive_resource {
        resource(input, &sdk, &impl_generics, &type_generics, where_clause)?
    } else {
        TokenStream::new()
    };
    let type_checks = constraint_checks(&kind, &source, &type_options, &sdk);
    // The associated buffers are inside the user's generic impl, rather than
    // nested constants that cannot capture its type or const parameters.
    Ok(quote_spanned! {name.span()=>
        impl #impl_generics #name #type_generics #where_clause {
            #[doc(hidden)]
            const __SLOPER_SCHEMA_REST: #sdk::__private::SchemaBuffer = { #(#type_checks;)* #rest };
            #[doc(hidden)]
            const __SLOPER_SCHEMA_JSON: #sdk::__private::SchemaBuffer = {
                let mut __schema = #sdk::__private::SchemaBuffer::new();
                __schema.push("{\"type\":"); __schema.quoted(<Self as #sdk::Schema>::TYPE);
                __schema.push(<Self as #sdk::Schema>::REST); __schema.push("}"); __schema
            };
        }
        impl #impl_generics #sdk::Schema for #name #type_generics #where_clause {
            const TYPE: &'static str = #kind;
            const REST: &'static str = Self::__SLOPER_SCHEMA_REST.as_str();
            const HAS_SOURCE: bool = #source;
            const JSON: &'static str = Self::__SLOPER_SCHEMA_JSON.as_str();
            const OPEN: bool = #open;
        }
        #resource_part
    })
}

fn definition(
    input: &DeriveInput,
    type_options: &Options,
    type_serde: &Serde,
    sdk: &Path,
) -> syn::Result<(TokenStream, TokenStream, TokenStream, TokenStream)> {
    let name = &input.ident;
    Ok(match &input.data {
        Data::Struct(data) => {
            match &data.fields {
                Fields::Named(fields) => object_schema(input, fields, type_options, type_serde, sdk)?,
                Fields::Unnamed(fields) if fields.unnamed.len() == 1 => {
                    let field = &fields.unnamed[0];
                    let serde_field = serde(&field.attrs, "field")?;
                    if serde_field.skip || serde_field.flatten || serde_field.rename.is_some() || serde_field.default {
                        return Err(syn::Error::new_spanned(
                            field,
                            "A schema newtype cannot skip, flatten, rename, or default its inner value.",
                        ));
                    }
                    let shape = shape(&field.ty, sdk, &name.to_string(), type_options)?;
                    if shape.optional {
                        return Err(syn::Error::new_spanned(
                            field,
                            "Use `Option<YourType>` at the property instead of an optional schema newtype.",
                        ));
                    }
                    let rest_value = shape.rest;
                    let extras = extra_members(type_options);
                    let rest = quote!({ let mut __schema = #sdk::__private::SchemaBuffer::new(); __schema.members(#rest_value, #extras); __schema });
                    (shape.kind, rest, shape.source, shape.open)
                },
                _ => {
                    return Err(syn::Error::new_spanned(
                        &data.fields,
                        "Use a named-field object or a single-field newtype; tuple and unit structs are unsupported \
                         schemas.",
                    ));
                },
            }
        },
        Data::Enum(data) => {
            let mut values = Vec::new();
            let mut names = BTreeSet::new();
            for variant in &data.variants {
                if !matches!(variant.fields, Fields::Unit) {
                    return Err(syn::Error::new_spanned(
                        variant,
                        "Data-carrying enums are unsupported; use a unit enum or a closed object.",
                    ));
                }
                let local = serde(&variant.attrs, "variant")?;
                if local.skip {
                    continue;
                }
                let value = local
                    .rename
                    .unwrap_or_else(|| rename(&variant.ident.to_string(), type_serde.rename_all.as_deref(), true));
                if !names.insert(value.clone()) {
                    return Err(syn::Error::new_spanned(
                        variant,
                        "Serialized enum values must be unique.",
                    ));
                }
                values.push(value);
            }
            if values.is_empty() || values.len() > 256 {
                return Err(syn::Error::new_spanned(
                    data.enum_token,
                    "A schema enum requires between one and 256 visible unit variants.",
                ));
            }
            let values =
                serde_json::to_string(&values).map_err(|error| syn::Error::new(input.span(), error.to_string()))?;
            let extra = extra_members(type_options);
            (
                quote!("string"),
                quote!({ let mut __schema = #sdk::__private::SchemaBuffer::new(); __schema.push(",\"enum\":"); __schema.push(#values); __schema.push(#extra); __schema }),
                quote!(false),
                quote!(false),
            )
        },
        Data::Union(_) => {
            return Err(syn::Error::new_spanned(
                name,
                "Unions are unsupported schemas; use a closed object or unit enum.",
            ));
        },
    })
}

fn object_schema(
    input: &DeriveInput,
    fields: &syn::FieldsNamed,
    type_options: &Options,
    type_serde: &Serde,
    sdk: &Path,
) -> syn::Result<(TokenStream, TokenStream, TokenStream, TokenStream)> {
    let mut members = Vec::new();
    let mut required = Vec::new();
    let mut sources = Vec::new();
    let mut opens = Vec::new();
    let mut names = BTreeSet::new();
    let mut open = false;
    for field in &fields.named {
        let serde_field = serde(&field.attrs, "field")?;
        if serde_field.skip {
            continue;
        }
        let field_name = field
            .ident
            .as_ref()
            .ok_or_else(|| syn::Error::new_spanned(field, "Schema object properties require named fields."))?;
        let field_options = options(&field.attrs)?;
        if serde_field.flatten {
            if last_type(&field.ty) != Some("Fields".into()) {
                return Err(syn::Error::new_spanned(
                    &field.ty,
                    "Only `Fields` may use `#[serde(flatten)]`; spell `Fields` directly.",
                ));
            }
            if open {
                return Err(syn::Error::new_spanned(
                    field,
                    "Only one flattened `Fields` property is permitted.",
                ));
            }
            if serde_field.default
                || serde_field.rename.is_some()
                || !field_options.members.is_empty()
                || field_options.item_format.is_some()
            {
                return Err(syn::Error::new_spanned(
                    field,
                    "Flattened `Fields` cannot have schema bounds, a rename, or a default.",
                ));
            }
            open = true;
            continue;
        }
        let property = serde_field
            .rename
            .clone()
            .unwrap_or_else(|| rename(&field_name.to_string(), type_serde.rename_all.as_deref(), false));
        validate_property(&property, field.span())?;
        if !names.insert(property.clone()) {
            return Err(syn::Error::new_spanned(
                field,
                "Serialized schema property names must be unique.",
            ));
        }
        let shape = shape(&field.ty, sdk, &input.ident.to_string(), &field_options)?;
        let schema = complete_schema(&shape, &field_options, sdk, field.span());
        sources.push(shape.source.clone());
        opens.push(shape.open.clone());
        members.push(quote! { __schema.quoted(#property); __schema.push(":"); __schema.push(#schema); });
        if !shape.optional && !serde_field.default && !type_serde.default {
            required.push(property);
        }
    }
    if members.len() > 256 {
        return Err(syn::Error::new_spanned(
            fields,
            "An object may declare at most 256 schema properties.",
        ));
    }
    let mut writes = Vec::new();
    for (index, member) in members.into_iter().enumerate() {
        if index > 0 {
            writes.push(quote!(__schema.push(",");));
        }
        writes.push(member);
    }
    let required =
        serde_json::to_string(&required).map_err(|error| syn::Error::new(input.span(), error.to_string()))?;
    let additional = if open {
        "true"
    } else {
        "false"
    };
    let extras = extra_members(type_options);
    let rest = quote!({
        let mut __schema = #sdk::__private::SchemaBuffer::new();
        __schema.push(",\"properties\":{"); #(#writes)*
        __schema.push("},\"required\":"); __schema.push(#required);
        __schema.push(",\"additionalProperties\":"); __schema.push(#additional);
        __schema.push(#extras); __schema
    });
    Ok((
        quote!("object"),
        rest,
        quote!(false #(|| #sources)*),
        quote!(#open #(|| #opens)*),
    ))
}

fn last_type(ty: &Type) -> Option<String> {
    if let Type::Path(path) = ty {
        path.path.segments.last().map(|segment| segment.ident.to_string())
    } else {
        None
    }
}

fn shape(ty: &Type, sdk: &Path, owner: &str, options: &Options) -> syn::Result<Shape> {
    if let Type::Array(array) = ty {
        let item = shape(&array.elem, sdk, owner, &Options::default())?;
        let json = complete_schema(&item, &item_options(options), sdk, ty.span());
        let len = &array.len;
        let source = item.source;
        return Ok(Shape {
            kind: quote!("array"),
            rest: quote!({ assert!(#len <= 100_000, "Fixed schema arrays cannot exceed 100,000 items."); let mut __schema = #sdk::__private::SchemaBuffer::new(); __schema.push(",\"items\":"); __schema.push(#json); __schema.push(",\"minItems\":"); __schema.number(#len); __schema.push(",\"maxItems\":"); __schema.number(#len); __schema }.as_str()),
            source,
            open: item.open,
            optional: false,
        });
    }
    let (name, arguments) = path_arguments(ty, owner)?;
    match name.as_str() {
        "Option" => {
            if arguments.len() != 1 {
                return Err(syn::Error::new_spanned(ty, "Spell nullable fields as `Option<T>`."));
            }
            let mut inner = shape(arguments[0], sdk, owner, options)?;
            if inner.optional {
                return Err(syn::Error::new_spanned(
                    ty,
                    "Nested `Option` cannot be represented; use one `Option<T>`.",
                ));
            }
            inner.optional = true;
            Ok(inner)
        },
        "Vec" => {
            if arguments.len() != 1 {
                return Err(syn::Error::new_spanned(ty, "Spell arrays as `Vec<T>`."));
            }
            let inner = shape(arguments[0], sdk, owner, &Options::default())?;
            let json = complete_schema(&inner, &item_options(options), sdk, ty.span());
            let source = inner.source;
            Ok(Shape {
                kind: quote!("array"),
                rest: quote!({ let mut __schema = #sdk::__private::SchemaBuffer::new(); __schema.push(",\"items\":"); __schema.push(#json); __schema }.as_str()),
                source,
                open: inner.open,
                optional: false,
            })
        },
        "BTreeMap" => {
            if arguments.len() != 2 || last_type(arguments[0]) != Some("String".into()) {
                return Err(syn::Error::new_spanned(
                    ty,
                    "Object maps must be spelled `BTreeMap<String, T>`.",
                ));
            }
            let inner = shape(arguments[1], sdk, owner, &Options::default())?;
            let json = complete_schema(&inner, &Options::default(), sdk, ty.span());
            let source = inner.source;
            Ok(Shape {
                kind: quote!("object"),
                rest: quote!({ let mut __schema = #sdk::__private::SchemaBuffer::new(); __schema.push(",\"additionalProperties\":"); __schema.push(#json); __schema }.as_str()),
                source,
                open: quote!(true),
                optional: false,
            })
        },
        _ if options.item_format.is_some() => {
            Err(syn::Error::new_spanned(
                ty,
                "Declare array item formats directly on `Vec<T>` or `[T; N]`, optionally inside `Option`.",
            ))
        },
        _ => {
            Ok(Shape {
                kind: quote!(<#ty as #sdk::Schema>::TYPE),
                rest: quote!(<#ty as #sdk::Schema>::REST),
                source: quote!(<#ty as #sdk::Schema>::HAS_SOURCE),
                open: quote!(<#ty as #sdk::Schema>::OPEN),
                optional: false,
            })
        },
    }
}

fn path_arguments<'a>(ty: &'a Type, owner: &str) -> syn::Result<(String, Vec<&'a Type>)> {
    let Type::Path(path) = ty else {
        return Err(syn::Error::new_spanned(
            ty,
            "Unsupported schema type; use owned scalars, objects, unit enums, `Option`, `Vec`, arrays, or \
             `BTreeMap<String, T>`.",
        ));
    };
    let segment = path
        .path
        .segments
        .last()
        .ok_or_else(|| syn::Error::new_spanned(ty, "A schema type name is required."))?;
    let name = segment.ident.to_string();
    if name == owner || name == "Self" {
        return Err(syn::Error::new_spanned(
            ty,
            "Recursive schema types are unsupported; remove the recursive property.",
        ));
    }
    if [
        "u64", "i64", "u128", "i128", "usize", "isize", "u8", "i8", "u16", "i16", "f32", "PathBuf", "Path", "Box",
        "Rc", "Arc", "HashMap", "HashSet", "Value", "Fields",
    ]
    .contains(&name.as_str())
    {
        return Err(syn::Error::new_spanned(
            ty,
            "Unsupported schema type; use `String`, `bool`, `f64`, `i32`, `u32`, a derived user type, or an \
             explicitly supported container.",
        ));
    }
    let arguments = match &segment.arguments {
        PathArguments::None => Vec::new(),
        PathArguments::AngleBracketed(args) => {
            args.args
                .iter()
                .filter_map(|arg| {
                    if let GenericArgument::Type(ty) = arg {
                        Some(ty)
                    } else {
                        None
                    }
                })
                .collect()
        },
        PathArguments::Parenthesized(_) => {
            return Err(syn::Error::new_spanned(ty, "Function types cannot have a schema."));
        },
    };
    Ok((name, arguments))
}

fn complete_schema(shape: &Shape, options: &Options, sdk: &Path, span: Span) -> TokenStream {
    let kind = &shape.kind;
    let rest = &shape.rest;
    let before = if shape.optional {
        quote!(__schema.push("{\"type\":["); __schema.quoted(#kind); __schema.push(",\"null\"]");)
    } else {
        quote!(__schema.push("{\"type\":"); __schema.quoted(#kind);)
    };
    let extra = extra_members(options);
    let checks = constraint_checks(kind, &shape.source, options, sdk);
    quote_spanned! {span=> { let mut __schema = #sdk::__private::SchemaBuffer::new(); #(#checks;)* #before __schema.members(#rest, #extra); __schema.push("}"); __schema }.as_str() }
}

fn constraint_checks(kind: &TokenStream, has_source: &TokenStream, options: &Options, sdk: &Path) -> Vec<TokenStream> {
    let mut checks: Vec<_> = options.members.iter().filter_map(|(name, _)| match name.as_str() {
        "format" => Some(quote!(assert!(#sdk::__private::equal(#kind, "string") && !#has_source, "Scalar formats require a string property without a managed source."))),
        "minLength"|"maxLength" => Some(quote!(assert!(#sdk::__private::equal(#kind, "string"), "String schema constraints require a string property."))),
        "mediaTypes"|"maxBytes" => Some(quote!(assert!(#has_source && #sdk::__private::equal(#kind, "string"), "`media_types` and `max_bytes` require a `Source` property."))),
        "minimum"|"maximum" => Some(quote!(assert!(#sdk::__private::equal(#kind, "number") || #sdk::__private::equal(#kind, "integer"), "Numeric bounds require a numeric property."))),
        "minItems"|"maxItems"|"uniqueItems" => Some(quote!(assert!(#sdk::__private::equal(#kind, "array"), "Array bounds require an array property."))),
        _ => None,
    }).collect();
    if options.item_format.is_some() {
        checks.push(quote!(
            assert!(#sdk::__private::equal(#kind, "array"), "`items(format = \"email\")` requires an array property.")
        ));
    }
    checks
}

fn item_options(options: &Options) -> Options {
    Options {
        members: options
            .item_format
            .iter()
            .map(|format| ("format".into(), format.clone().into()))
            .collect(),
        ..Options::default()
    }
}

fn extra_members(options: &Options) -> String {
    let mut json = String::new();
    for (name, value) in &options.members {
        json.push(',');
        json.push_str(&serde_json::to_string(name).expect("schema keyword is a string"));
        json.push(':');
        json.push_str(&value.to_string());
    }
    json
}

fn options(attributes: &[Attribute]) -> syn::Result<Options> {
    let mut options = Options::default();
    let mut names = BTreeSet::new();
    for attr in attributes.iter().filter(|attr| attr.path().is_ident("schema")) {
        attr.parse_nested_meta(|meta| {
            let name = meta
                .path
                .get_ident()
                .ok_or_else(|| meta.error("Use a supported schema keyword."))?
                .to_string();
            if !names.insert(name.clone()) {
                return Err(meta.error(format!("`{name}` set multiple times.")));
            }
            if name == "crate" {
                let path: LitStr = meta.value()?.parse()?;
                options.crate_path = Some(path.parse()?);
                return Ok(());
            }
            if name == "format" {
                let value: LitStr = meta.value()?.parse()?;
                options.members.push(("format".into(), format_value(&value)?.into()));
                return Ok(());
            }
            if name == "items" {
                meta.parse_nested_meta(|item| {
                    if !item.path.is_ident("format") {
                        return Err(item.error("Use `items(format = \"email\")` to declare an array's string format."));
                    }
                    if options.item_format.is_some() {
                        return Err(item.error("`format` set multiple times."));
                    }
                    let value: LitStr = item.value()?.parse()?;
                    options.item_format = Some(format_value(&value)?);
                    Ok(())
                })?;
                return Ok(());
            }
            let key = match name.as_str() {
                "title" => "title",
                "description" => "description",
                "min_length" => "minLength",
                "max_length" => "maxLength",
                "minimum" => "minimum",
                "maximum" => "maximum",
                "min_items" => "minItems",
                "max_items" => "maxItems",
                "unique_items" => "uniqueItems",
                "media_types" => "mediaTypes",
                "max_bytes" => "maxBytes",
                _ => {
                    return Err(meta.error(
                        "Supported schema options are `title`, `description`, `format`, `items(format = \"email\")`, \
                         `min_length`, `max_length`, `minimum`, `maximum`, `min_items`, `max_items`, `unique_items`, \
                         `media_types`, `max_bytes`, and `crate`. Types are derived from Rust types.",
                    ));
                },
            };
            let value: Expr = meta.value()?.parse()?;
            let json = literal(&value)?;
            validate_option(key, &name, &json, &meta)?;
            options.members.push((key.into(), json));
            Ok(())
        })?;
    }
    for (low, high) in [
        ("minLength", "maxLength"),
        ("minItems", "maxItems"),
        ("minimum", "maximum"),
    ] {
        let left = options
            .members
            .iter()
            .find(|(k, _)| k == low)
            .and_then(|(_, v)| v.as_f64());
        let right = options
            .members
            .iter()
            .find(|(k, _)| k == high)
            .and_then(|(_, v)| v.as_f64());
        if left.zip(right).is_some_and(|(left, right)| left > right) {
            return Err(syn::Error::new(
                Span::call_site(),
                format!("`{low}` must not exceed `{high}`."),
            ));
        }
    }
    Ok(options)
}

fn validate_option(
    key: &str,
    name: &str,
    json: &serde_json::Value,
    meta: &syn::meta::ParseNestedMeta<'_>,
) -> syn::Result<()> {
    match key {
        "title" | "description" => {
            let Some(text) = json.as_str() else {
                return Err(meta.error("Schema display text must be a string literal."));
            };
            validate_text(
                text,
                if key == "title" {
                    60
                } else {
                    500
                },
                meta.path.span(),
            )?;
        },
        "uniqueItems" if !json.is_boolean() => {
            return Err(meta.error("`unique_items` requires `true` or `false`."));
        },
        "minLength" | "maxLength" | "minItems" | "maxItems" | "maxBytes" => {
            let maximum = if key == "maxBytes" {
                104_857_600
            } else if key.ends_with("Items") {
                100_000
            } else {
                1_000_000
            };
            let minimum = u64::from(key == "maxBytes");
            if json.as_u64().is_none_or(|v| v < minimum || v > maximum) {
                return Err(meta.error(format!("`{name}` must be an integer from {minimum} through {maximum}.")));
            }
        },
        "minimum" | "maximum"
            if json
                .as_f64()
                .is_none_or(|v| !v.is_finite() || v.abs() > 9_007_199_254_740_991.0) =>
        {
            return Err(meta.error("Numeric bounds must be finite and within ±(2^53 − 1)."));
        },
        "mediaTypes" => {
            let Some(values) = json.as_array() else {
                return Err(meta.error("`media_types` requires an array of MIME-type string literals."));
            };
            let mut unique = BTreeSet::new();
            if values.is_empty()
                || values.iter().any(|v| {
                    v.as_str()
                        .is_none_or(|s| !s.contains('/') || s.contains(char::is_whitespace) || !unique.insert(s))
                })
            {
                return Err(meta.error("Media types must be a non-empty array of unique MIME-type strings."));
            }
        },
        _ => {},
    }
    Ok(())
}

fn format_value(value: &LitStr) -> syn::Result<String> {
    let format = value.value();
    if !["date", "date-time", "email", "uri", "uuid"].contains(&format.as_str()) {
        return Err(syn::Error::new_spanned(
            value,
            "Supported string formats are `date`, `date-time`, `email`, `uri`, and `uuid`; managed sources use \
             `Source`.",
        ));
    }
    Ok(format)
}

fn literal(expr: &Expr) -> syn::Result<serde_json::Value> {
    match expr {
        Expr::Lit(expr) => {
            match &expr.lit {
                Lit::Str(s) => Ok(s.value().into()),
                Lit::Bool(b) => Ok(b.value.into()),
                Lit::Int(i) => i.base10_parse::<u64>().map(Into::into),
                Lit::Float(f) => {
                    let value = f.base10_parse::<f64>()?;
                    serde_json::Number::from_f64(value)
                        .map(serde_json::Value::Number)
                        .ok_or_else(|| syn::Error::new_spanned(f, "Numeric bounds must be finite."))
                },
                _ => {
                    Err(syn::Error::new_spanned(
                        expr,
                        "Use a string, numeric, boolean, or array literal.",
                    ))
                },
            }
        },
        Expr::Unary(unary) if matches!(unary.op, syn::UnOp::Neg(_)) => {
            let value = literal(&unary.expr)?
                .as_f64()
                .ok_or_else(|| syn::Error::new_spanned(expr, "Only numeric literals can be negative."))?;
            serde_json::Number::from_f64(-value)
                .map(serde_json::Value::Number)
                .ok_or_else(|| syn::Error::new_spanned(expr, "Numeric bounds must be finite."))
        },
        Expr::Array(array) => {
            array
                .elems
                .iter()
                .map(literal)
                .collect::<syn::Result<Vec<_>>>()
                .map(Into::into)
        },
        _ => {
            Err(syn::Error::new_spanned(
                expr,
                "Schema constraints require literal values.",
            ))
        },
    }
}

fn serde(attributes: &[Attribute], owner: &str) -> syn::Result<Serde> {
    let mut result = Serde::default();
    let mut names = BTreeSet::new();
    for attr in attributes.iter().filter(|attr| attr.path().is_ident("serde")) {
        attr.parse_nested_meta(|meta| {
            let name = meta
                .path
                .get_ident()
                .ok_or_else(|| meta.error("Use a supported serde attribute."))?
                .to_string();
            if !names.insert(name.clone()) {
                return Err(meta.error(format!("`{name}` set multiple times.")));
            }
            match name.as_str() {
                "rename" => result.rename = Some(meta.value()?.parse::<LitStr>()?.value()),
                "rename_all" if owner == "type" => {
                    let rule = meta.value()?.parse::<LitStr>()?;
                    if ![
                        "lowercase",
                        "UPPERCASE",
                        "PascalCase",
                        "camelCase",
                        "snake_case",
                        "SCREAMING_SNAKE_CASE",
                        "kebab-case",
                        "SCREAMING-KEBAB-CASE",
                    ]
                    .contains(&rule.value().as_str())
                    {
                        return Err(meta.error("Use a serde-supported `rename_all` rule."));
                    }
                    result.rename_all = Some(rule.value());
                },
                "default" if owner != "variant" => {
                    result.default = true;
                    if meta.input.peek(syn::Token![=]) {
                        let _: LitStr = meta.value()?.parse()?;
                    }
                },
                "skip" if owner != "type" => result.skip = true,
                "flatten" if owner == "field" => result.flatten = true,
                _ => {
                    return Err(meta.error(
                        "Only `rename`, `rename_all`, `default`, `skip`, and flatten on `Fields` may change schema \
                         serialization.",
                    ));
                },
            }
            Ok(())
        })?;
    }
    if result.skip && (result.flatten || result.rename.is_some() || result.default) {
        return Err(syn::Error::new(
            Span::call_site(),
            "`skip` cannot be combined with flatten, rename, or default.",
        ));
    }
    Ok(result)
}

fn rename(name: &str, rule: Option<&str>, variant: bool) -> String {
    let Some(rule) = rule else {
        return name.into();
    };
    let snake = if variant {
        let mut out = String::new();
        for (index, c) in name.char_indices() {
            if index > 0 && c.is_uppercase() {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        }
        out
    } else {
        name.into()
    };
    let pascal = || {
        snake
            .split('_')
            .filter(|s| !s.is_empty())
            .map(|word| {
                let mut chars = word.chars();
                let mut s = chars
                    .next()
                    .map(char::to_uppercase)
                    .map(Iterator::collect::<String>)
                    .unwrap_or_default();
                s.extend(chars);
                s
            })
            .collect::<String>()
    };
    match rule {
        "lowercase" => {
            if variant {
                name.to_lowercase()
            } else {
                name.into()
            }
        },
        "UPPERCASE" => name.to_uppercase(),
        "snake_case" => snake,
        "SCREAMING_SNAKE_CASE" => snake.to_uppercase(),
        "kebab-case" => snake.replace('_', "-"),
        "SCREAMING-KEBAB-CASE" => snake.replace('_', "-").to_uppercase(),
        "PascalCase" => {
            if variant {
                name.into()
            } else {
                pascal()
            }
        },
        "camelCase" => {
            let p = if variant {
                name.into()
            } else {
                pascal()
            };
            let mut chars = p.chars();
            let mut out = chars
                .next()
                .map(char::to_lowercase)
                .map(Iterator::collect::<String>)
                .unwrap_or_default();
            out.extend(chars);
            out
        },
        _ => name.into(),
    }
}

pub(super) fn validate_text(text: &str, maximum: usize, span: Span) -> syn::Result<()> {
    if text.chars().count() > maximum
        || text.chars().any(|c| {
            c.is_control()
                || matches!(c,'\u{061c}'|'\u{200e}'|'\u{200f}'|'\u{202a}'..='\u{202e}'|'\u{2066}'..='\u{2069}')
        })
    {
        return Err(syn::Error::new(
            span,
            format!("Display text must be at most {maximum} characters without control or bidi formatting characters."),
        ));
    }
    Ok(())
}

fn validate_property(name: &str, span: Span) -> syn::Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .enumerate()
            .all(|(i, c)| c == '_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit()))
    {
        return Err(syn::Error::new(
            span,
            "Serialized property names must match `[A-Za-z_][A-Za-z0-9_]*` and fit 64 characters.",
        ));
    }
    Ok(())
}

fn resource(
    input: &DeriveInput,
    sdk: &Path,
    impl_generics: &syn::ImplGenerics<'_>,
    type_generics: &syn::TypeGenerics<'_>,
    where_clause: Option<&syn::WhereClause>,
) -> syn::Result<TokenStream> {
    let mut name = None;
    let mut key = None;
    let mut seen = BTreeSet::new();
    for attr in input.attrs.iter().filter(|attr| attr.path().is_ident("resource")) {
        attr.parse_nested_meta(|meta| {
            let field = meta
                .path
                .get_ident()
                .ok_or_else(|| meta.error("Use `name` or `key`."))?
                .to_string();
            if !seen.insert(field.clone()) {
                return Err(meta.error(format!("`{field}` set multiple times.")));
            }
            match field.as_str() {
                "name" => name = Some(meta.value()?.parse::<LitStr>()?),
                "key" => key = Some(meta.value()?.parse::<syn::Ident>()?),
                _ => {
                    return Err(meta.error(
                        "Declare a resource with `#[resource(name = \"items\", key = id)]`; read-only resources may \
                         omit `key`.",
                    ));
                },
            }
            Ok(())
        })?;
    }
    let name = name.ok_or_else(|| {
        syn::Error::new_spanned(
            &input.ident,
            "Declare `#[resource(name = \"items\", key = id)]`; read-only resources may omit `key`.",
        )
    })?;
    super::declaration::validate_key(&name.value(), name.span())?;
    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            input,
            "Resources require a named-field object.",
        ));
    };
    let Fields::Named(fields) = &data.fields else {
        return Err(syn::Error::new_spanned(
            input,
            "Resources require a named-field object.",
        ));
    };
    let outer = serde(&input.attrs, "type")?;
    let key = if let Some(key) = key {
        let field = fields
            .named
            .iter()
            .find(|f| f.ident.as_ref() == Some(&key))
            .ok_or_else(|| syn::Error::new_spanned(&key, "The resource key must name a declared property."))?;
        let local = serde(&field.attrs, "field")?;
        let bound = options(&field.attrs)?
            .members
            .into_iter()
            .find(|(k, _)| k == "maxLength")
            .and_then(|(_, v)| v.as_u64());
        if last_type(&field.ty) != Some("String".into())
            || local.skip
            || local.flatten
            || local.default
            || outer.default
            || bound.is_none_or(|bound| bound == 0 || bound > 512)
        {
            return Err(syn::Error::new_spanned(
                field,
                "A resource key must be a required `String` with `#[schema(max_length = 512)]` or a smaller positive \
                 bound.",
            ));
        }
        Some(
            local
                .rename
                .unwrap_or_else(|| rename(&key.to_string(), outer.rename_all.as_deref(), false)),
        )
    } else {
        None
    };
    let ident = &input.ident;
    let key_value = key.as_ref().map_or(
        quote!(::core::option::Option::None),
        |k| quote!(::core::option::Option::Some(#k)),
    );
    let key_json = key.map(|key| quote!(__part.push(",\"key\":");__part.quoted(#key);));
    Ok(quote! {
        impl #impl_generics #ident #type_generics #where_clause {
            #[doc(hidden)]
            const __SLOPER_RESOURCE_PART:#sdk::__private::SchemaBuffer={let mut __part=#sdk::__private::SchemaBuffer::new();__part.push("{\"kind\":\"resource\",\"name\":");__part.quoted(#name);#key_json __part.push(",\"schema\":");__part.push(<Self as #sdk::Schema>::JSON);__part.push("}");__part};
        }
        impl #impl_generics #sdk::Resource for #ident #type_generics #where_clause {
            const NAME:&'static str=#name;const KEY:Option<&'static str>=#key_value;const PART:&'static str=Self::__SLOPER_RESOURCE_PART.as_str();
        }
    })
}
