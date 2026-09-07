//! Compile-time declarations for the Sloper extension SDK.
//!
//! The SDK re-exports these macros; application crates use `sloper_extension`.
#![warn(rust_2018_idioms)]

use proc_macro::TokenStream;
use quote::quote;

mod declaration;
mod schema;

/// Derives a closed, bounded schema from a Rust struct or unit enum.
#[proc_macro_derive(Schema, attributes(schema, serde))]
pub fn schema(input: TokenStream) -> TokenStream {
    schema::derive(&syn::parse_macro_input!(input as syn::DeriveInput), false)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Derives an item schema and its named resource declaration.
#[proc_macro_derive(Resource, attributes(resource, schema, serde))]
pub fn resource(input: TokenStream) -> TokenStream {
    schema::derive(&syn::parse_macro_input!(input as syn::DeriveInput), true)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Declares a named OAuth profile and scope set.
#[proc_macro_derive(Connection, attributes(connection))]
pub fn connection(input: TokenStream) -> TokenStream {
    declaration::connection(&syn::parse_macro_input!(input as syn::DeriveInput))
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Derives resource and connection capabilities from an asynchronous action.
#[proc_macro_attribute]
pub fn action(attributes: TokenStream, input: TokenStream) -> TokenStream {
    let original: proc_macro2::TokenStream = input.clone().into();
    match declaration::action(attributes.into(), syn::parse::<syn::ItemFn>(input)) {
        Ok(output) => output.into(),
        Err(error) => {
            let error = error.into_compile_error();
            quote!(#original #error).into()
        },
    }
}

/// Declares one extension and its complete explicit action list.
#[proc_macro]
pub fn extension(input: TokenStream) -> TokenStream {
    declaration::extension(syn::parse_macro_input!(input as declaration::Extension))
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}
