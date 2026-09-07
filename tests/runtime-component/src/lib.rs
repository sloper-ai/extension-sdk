#![warn(missing_debug_implementations, missing_docs, rust_2018_idioms, unreachable_pub)]

//! The actual Wasm fixture for SDK dispatch conformance.

// Native builds host the E2E suite; only Wasm builds contain action exports.
#[cfg(target_arch = "wasm32")]
mod guest;
