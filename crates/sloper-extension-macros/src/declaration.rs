use std::collections::BTreeSet;

use proc_macro2::{
    Span,
    TokenStream,
};
use quote::{
    format_ident,
    quote,
    quote_spanned,
};
use syn::{
    Attribute,
    Data,
    DeriveInput,
    ExprArray,
    FnArg,
    GenericArgument,
    Ident,
    ItemFn,
    LitStr,
    Path,
    PathArguments,
    Token,
    Type,
    ext::IdentExt,
    parse::{
        Parse,
        ParseStream,
    },
    punctuated::Punctuated,
    spanned::Spanned,
};

use super::schema::validate_text;

pub(super) fn validate_key(name: &str, span: Span) -> syn::Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name.as_bytes()[0].is_ascii_lowercase()
        || !name
            .split('-')
            .all(|word| !word.is_empty() && word.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()))
    {
        return Err(syn::Error::new(
            span,
            "Manifest keys must use kebab-case and fit 64 characters.",
        ));
    }
    Ok(())
}

fn sdk_path(attributes: TokenStream) -> syn::Result<Path> {
    if attributes.is_empty() {
        return Ok(syn::parse_quote!(::sloper_extension));
    }
    let mut path = None;
    let parser = syn::meta::parser(|meta| {
        if !meta.path.is_ident("crate") {
            return Err(
                meta.error("The action name is derived from its function; only `crate = \"path\"` is supported.")
            );
        }
        if path.is_some() {
            return Err(meta.error("`crate` set multiple times."));
        }
        let value: LitStr = meta.value()?.parse()?;
        path = Some(value.parse()?);
        Ok(())
    });
    syn::parse::Parser::parse2(parser, attributes)?;
    path.ok_or_else(|| syn::Error::new(Span::call_site(), "Use `#[action]` or `#[action(crate = \"path\")]`."))
}

pub(super) fn connection(input: &DeriveInput) -> syn::Result<TokenStream> {
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.generics,
            "Connection declarations are concrete unit types and cannot have generic parameters.",
        ));
    }
    if !matches!(input.data, Data::Struct(ref data) if matches!(data.fields, syn::Fields::Unit)) {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "Declare a connection as a unit struct with `#[connection(name = \"ledger\", profile = \"acme.ledger\", \
             scopes = [\"items.read\"])]`.",
        ));
    }
    let mut name = None;
    let mut profile = None;
    let mut scopes = None;
    let mut sdk = syn::parse_quote!(::sloper_extension);
    let mut seen = BTreeSet::new();
    for attribute in input.attrs.iter().filter(|a| a.path().is_ident("connection")) {
        attribute.parse_nested_meta(|meta| {
            let key = meta
                .path
                .get_ident()
                .ok_or_else(|| meta.error("Use `name`, `profile`, `scopes`, or `crate`."))?
                .to_string();
            if !seen.insert(key.clone()) {
                return Err(meta.error(format!("`{key}` set multiple times.")));
            }
            match key.as_str() {
                "name" => name = Some(meta.value()?.parse::<LitStr>()?),
                "profile" => profile = Some(meta.value()?.parse::<LitStr>()?),
                "scopes" => scopes = Some(meta.value()?.parse::<ExprArray>()?),
                "crate" => {
                    let value: LitStr = meta.value()?.parse()?;
                    sdk = value.parse::<Path>()?;
                }
                _ => return Err(meta.error(
                    "Supported connection options are `name`, `profile`, `scopes`, and `crate`.",
                )),
            }
            Ok(())
        })?;
    }
    let name = name.ok_or_else(|| {
        syn::Error::new_spanned(&input.ident, "A connection declaration requires `name = \"ledger\"`.")
    })?;
    validate_key(&name.value(), name.span())?;
    let profile = profile.ok_or_else(|| {
        syn::Error::new_spanned(
            &input.ident,
            "A connection declaration requires `profile = \"acme.ledger\"`.",
        )
    })?;
    validate_text(&profile.value(), 128, profile.span())?;
    if profile.value().is_empty() {
        return Err(syn::Error::new_spanned(
            &profile,
            "A connection profile cannot be empty.",
        ));
    }
    let scopes = scopes.ok_or_else(|| {
        syn::Error::new_spanned(
            &input.ident,
            "A connection declaration requires non-empty `scopes = [\"items.read\"]`.",
        )
    })?;
    let values = scope_values(scopes)?;
    let part = serde_json::json!({"kind":"connection","name":name.value(),"profile":profile.value(),"scopes":values})
        .to_string();
    let ident = &input.ident;
    Ok(quote_spanned! {ident.span()=>
        impl #sdk::ConnectionType for #ident {
            const NAME: &'static str = #name;
            const PROFILE: &'static str = #profile;
            const SCOPES: &'static [&'static str] = &[#(#values),*];
            const PART: &'static str = #part;
        }
    })
}

fn scope_values(scopes: ExprArray) -> syn::Result<Vec<String>> {
    let span = scopes.span();
    let mut values = Vec::new();
    let mut unique = BTreeSet::new();
    for scope in scopes.elems {
        let syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(scope),
            ..
        }) = scope
        else {
            return Err(syn::Error::new_spanned(scope, "Each scope must be a string literal."));
        };
        let value = scope.value();
        validate_text(&value, 512, scope.span())?;
        if value.is_empty() || value.chars().any(char::is_whitespace) || !unique.insert(value.clone()) {
            return Err(syn::Error::new_spanned(
                scope,
                "Scopes must be non-empty, unique strings without whitespace.",
            ));
        }
        values.push(value);
    }
    if values.is_empty() || values.len() > 32 {
        return Err(syn::Error::new(
            span,
            "Declare between one and 32 unique connection scopes.",
        ));
    }
    Ok(values)
}

fn documentation(attributes: &[Attribute]) -> syn::Result<Option<String>> {
    let lines = attributes
        .iter()
        .filter_map(|attribute| {
            if !attribute.path().is_ident("doc") {
                return None;
            }
            match &attribute.meta {
                syn::Meta::NameValue(value) => {
                    match &value.value {
                        syn::Expr::Lit(syn::ExprLit {
                            lit: syn::Lit::Str(text),
                            ..
                        }) => Some(text.value().trim().to_owned()),
                        _ => None,
                    }
                },
                _ => None,
            }
        })
        .collect::<Vec<_>>();
    if lines.is_empty() {
        return Ok(None);
    }
    let description = lines.join(" ");
    validate_text(&description, 200, attributes[0].span())?;
    Ok(Some(description))
}

fn capability(ty: &Type) -> syn::Result<Option<(String, Type)>> {
    let Type::Path(path) = ty else {
        return Ok(None);
    };
    let Some(segment) = path.path.segments.last() else {
        return Ok(None);
    };
    let name = segment.ident.to_string();
    if !["Connection", "Reader", "Writer"].contains(&name.as_str()) {
        return Ok(None);
    }
    let PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return Err(syn::Error::new_spanned(
            ty,
            "Capability handles require one declared type argument.",
        ));
    };
    if arguments.args.len() != 1 {
        return Err(syn::Error::new_spanned(
            ty,
            "Capability handles require exactly one declared type argument.",
        ));
    }
    let GenericArgument::Type(inner) = &arguments.args[0] else {
        return Err(syn::Error::new_spanned(
            ty,
            "Capability handles require one type argument.",
        ));
    };
    Ok(Some((name, inner.clone())))
}

pub(super) fn action(attributes: TokenStream, input: syn::Result<ItemFn>) -> syn::Result<TokenStream> {
    let sdk = sdk_path(attributes)?;
    let input = input?;
    let signature = &input.sig;
    if signature.asyncness.is_none() {
        return Err(syn::Error::new_spanned(
            signature.fn_token,
            "An action must be `async fn` and return `sloper_extension::Result<()>`.",
        ));
    }
    if !signature.generics.params.is_empty()
        || !matches!(signature.safety, syn::Safety::Default)
        || signature.abi.is_some()
        || signature.variadic.is_some()
    {
        return Err(syn::Error::new_spanned(
            signature,
            "Actions must be concrete safe Rust async functions without generic parameters or variadic arguments.",
        ));
    }
    let ident = &signature.ident;
    let action_name = ident.unraw().to_string().replace('_', "-");
    validate_key(&action_name, ident.span())?;
    let module = format_ident!("__sloper_action_{}", ident.unraw());
    let ActionArguments {
        connections,
        resources,
        arguments,
        parameter_type,
        connection_types,
    } = action_arguments(signature, &sdk)?;
    let description = documentation(&input.attrs)?;
    let description = description.map_or(
        quote!(::core::option::Option::None),
        |description| quote!(::core::option::Option::Some(#description)),
    );
    let parameters = parameter_type.map_or(quote!(::core::option::Option::None), |ty| quote!({ assert!(#sdk::__private::equal(<#ty as #sdk::Schema>::TYPE, "object"), "Action parameters must be an object schema."); ::core::option::Option::Some(<#ty as #sdk::Schema>::JSON) }));
    let connection = connection_types.first().map_or(
        quote!(::core::option::Option::None),
        |ty| quote!(::core::option::Option::Some(<#ty as #sdk::ConnectionType>::NAME)),
    );
    // The wrapper is anchored to the user's signature so type errors in the
    // capability or result spec point at their declaration.
    Ok(quote_spanned! {signature.span()=>
        #input
        #[doc(hidden)]
        #[allow(non_snake_case)]
        #[allow(clippy::absolute_paths, clippy::large_const_arrays, clippy::wildcard_imports)]
        pub(crate) mod #module {
            #[allow(unused_imports)]
            use super::*;
            pub(crate) const NAME: &str = #action_name;
            pub(crate) const PARTS: #sdk::__private::ActionParts = #sdk::__private::ActionParts { name:NAME, resources:&[#(#resources),*], connections:&[#(#connections),*] };
            const BUFFER: #sdk::__private::SchemaBuffer = #sdk::__private::action_part(&PARTS, #description, #parameters);
            pub(crate) const JSON: &str = BUFFER.as_str();
            #sdk::__private::embed_part!(DECLARATION, JSON);
            pub(crate) async fn run(__context: &#sdk::__private::RunContext) -> #sdk::Result<()> {
                let __connection: ::core::option::Option<&'static str> = #connection;
                super::#ident(#(#arguments),*).await
            }
        }
    })
}

struct ActionArguments {
    connections: Vec<TokenStream>,
    resources: Vec<TokenStream>,
    arguments: Vec<TokenStream>,
    parameter_type: Option<Box<Type>>,
    connection_types: Vec<Type>,
}

fn action_arguments(signature: &syn::Signature, sdk: &Path) -> syn::Result<ActionArguments> {
    let mut connections = Vec::new();
    let mut resources = Vec::new();
    let mut arguments = Vec::new();
    let mut parameter_type = None;
    let mut readers = 0;
    let mut writers = 0;
    let mut connection_types = Vec::new();
    let mut duplicate = BTreeSet::new();
    for argument in &signature.inputs {
        let FnArg::Typed(argument) = argument else {
            return Err(syn::Error::new_spanned(
                argument,
                "Actions cannot take a receiver; declare a free async function.",
            ));
        };
        if !matches!(&argument.pat.as_ref(), syn::Pat::Ident(_)) {
            return Err(syn::Error::new_spanned(
                &argument.pat,
                "Name each action argument; destructure owned parameters inside the function.",
            ));
        }
        if let Some((kind, ty)) = capability(&argument.ty)? {
            let identity = (kind.clone(), quote!(#ty).to_string());
            if !duplicate.insert(identity) {
                return Err(syn::Error::new_spanned(
                    argument,
                    "Repeated capability handles are invalid; use one reader, writer, or connection for each declared \
                     type.",
                ));
            }
            match kind.as_str() {
                "Connection" => {
                    connection_types.push(ty.clone());
                    connections.push(quote!(#sdk::__private::ConnectionUse { name:<#ty as #sdk::ConnectionType>::NAME, part:<#ty as #sdk::ConnectionType>::PART }));
                    arguments.push(quote!(__context.connection::<#ty>()));
                },
                "Reader" => {
                    readers += 1;
                    resources.push(quote!(#sdk::__private::ResourceUse { name:<#ty as #sdk::Resource>::NAME, part:<#ty as #sdk::Resource>::PART, write:false }));
                    arguments.push(quote!(__context.reader::<#ty>().await?));
                },
                "Writer" => {
                    writers += 1;
                    resources.push(quote!({ assert!(<#ty as #sdk::Resource>::KEY.is_some(), "A writable resource requires a declared bounded string key."); #sdk::__private::ResourceUse { name:<#ty as #sdk::Resource>::NAME, part:<#ty as #sdk::Resource>::PART, write:true } }));
                    arguments.push(quote!(__context.writer::<#ty>(__connection).await?));
                },
                _ => unreachable!("capability parser returns one of three variants"),
            }
        } else {
            if matches!(&argument.ty.as_ref(), Type::Reference(_) | Type::Ptr(_)) {
                return Err(syn::Error::new_spanned(
                    &argument.ty,
                    "Action parameters must be owned values.",
                ));
            }
            if parameter_type.replace(argument.ty.clone()).is_some() {
                return Err(syn::Error::new_spanned(
                    argument,
                    "An action accepts at most one owned parameters value.",
                ));
            }
            let ty = &argument.ty;
            arguments.push(quote!(__context.parameters::<#ty>()?));
        }
    }
    if readers > 0 && parameter_type.is_some() {
        return Err(syn::Error::new_spanned(
            signature,
            "Readers exclude owned parameters; remove the parameters argument.",
        ));
    }
    if writers > 0 && connections.len() > 1 {
        return Err(syn::Error::new_spanned(
            signature,
            "An action with a writer accepts at most one connection.",
        ));
    }
    Ok(ActionArguments {
        connections,
        resources,
        arguments,
        parameter_type,
        connection_types,
    })
}

pub(super) struct Extension {
    name: LitStr,
    label: Option<LitStr>,
    configuration: Option<Type>,
    actions: Vec<Path>,
    sdk: Path,
}

impl Parse for Extension {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut name = None;
        let mut label = None;
        let mut configuration = None;
        let mut actions = None;
        let mut sdk = syn::parse_quote!(::sloper_extension);
        let mut seen = BTreeSet::new();
        while !input.is_empty() {
            let key = input.call(Ident::parse_any)?;
            input.parse::<Token![:]>()?;
            if !seen.insert(key.to_string()) {
                return Err(syn::Error::new_spanned(&key, format!("`{key}` set multiple times.")));
            }
            match key.to_string().as_str() {
                "name" => name = Some(input.parse()?),
                "label" => label = Some(input.parse()?),
                "configuration" => configuration = Some(input.parse()?),
                "actions" => {
                    let content;
                    syn::bracketed!(content in input);
                    actions = Some(
                        Punctuated::<Path, Token![,]>::parse_terminated(&content)?
                            .into_iter()
                            .collect::<Vec<_>>(),
                    );
                },
                "crate" => {
                    let value: LitStr = input.parse()?;
                    sdk = value.parse()?;
                },
                _ => {
                    return Err(syn::Error::new_spanned(
                        key,
                        "Supported extension fields are `name`, `label`, `configuration`, `actions`, and `crate`; \
                         Cargo supplies version and description.",
                    ));
                },
            }
            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
        }
        Ok(Self {
            name: name.ok_or_else(|| input.error("Declare `name: \"publisher.extension\"`."))?,
            label,
            configuration,
            actions: actions.ok_or_else(|| input.error("Declare an explicit `actions: [run]` list."))?,
            sdk,
        })
    }
}

pub(super) fn extension(input: Extension) -> syn::Result<TokenStream> {
    let Extension {
        name,
        label,
        configuration,
        actions,
        sdk,
    } = input;
    let identity = name.value();
    if identity.len() > 128 || identity.split('.').count() < 2 {
        return Err(syn::Error::new_spanned(
            &name,
            "Extension names require a publisher namespace and dotted kebab-case labels, at most 128 characters.",
        ));
    }
    for label in identity.split('.') {
        validate_key(label, name.span())?;
    }
    if let Some(label) = &label {
        validate_text(&label.value(), 60, label.span())?;
    }
    if actions.is_empty() || actions.len() > 64 {
        return Err(syn::Error::new_spanned(
            &name,
            "Declare between one and 64 explicit actions.",
        ));
    }
    let mut modules = Vec::new();
    let mut unique = BTreeSet::new();
    for mut action in actions {
        if !unique.insert(quote!(#action).to_string()) {
            return Err(syn::Error::new_spanned(
                action,
                "Action paths cannot repeat in the explicit list.",
            ));
        }
        let last = action
            .segments
            .last_mut()
            .ok_or_else(|| syn::Error::new_spanned(&name, "An action path is required."))?;
        if !matches!(last.arguments, PathArguments::None) {
            return Err(syn::Error::new_spanned(last, "Actions cannot take generic arguments."));
        }
        last.ident = format_ident!("__sloper_action_{}", last.ident.unraw());
        modules.push(action);
    }
    let label = label.map_or(
        quote!(::core::option::Option::None),
        |value| quote!(::core::option::Option::Some(#value)),
    );
    let configuration=configuration.map_or(quote!(::core::option::Option::None),|ty|quote!({
        assert!(#sdk::__private::equal(<#ty as #sdk::Schema>::TYPE,"object"),"Extension configuration must be an object.");
        assert!(!<#ty as #sdk::Schema>::OPEN,"Extension configuration cannot accept open fields.");
        assert!(!<#ty as #sdk::Schema>::HAS_SOURCE,"Source fields are forbidden in extension configuration.");
        ::core::option::Option::Some(<#ty as #sdk::Schema>::JSON)
    }));
    Ok(quote! {
        #[doc(hidden)]
        const __SLOPER_EXTENSION_BUFFER:#sdk::__private::SchemaBuffer=#sdk::__private::extension_parts(#name,env!("CARGO_PKG_VERSION"),#label,env!("CARGO_PKG_DESCRIPTION"),#configuration,&[#(&#modules::PARTS),*]);
        #[doc(hidden)]
        pub const SLOPER_EXTENSION_PARTS:&str=__SLOPER_EXTENSION_BUFFER.as_str();
        #sdk::__private::embed_part!(__SLOPER_EXTENSION_DECLARATION, SLOPER_EXTENSION_PARTS);
        #[doc(hidden)]
        #[derive(Debug)]
        pub struct __SloperExtension;
        impl #sdk::__private::Guest for __SloperExtension {
            async fn run(request:#sdk::__private::Request)->::core::result::Result<(),#sdk::__private::Failure>{
                #sdk::__private::dispatch(request,|context|::std::boxed::Box::pin(async move {
                    match context.action(){#(#modules::NAME=>#modules::run(context).await,)*_=>#sdk::__private::unknown_action()}
                })).await
            }
        }
        #[cfg(target_arch="wasm32")]
        #sdk::__private::export!(__SloperExtension with_types_in #sdk::__private::bindings);
    })
}
