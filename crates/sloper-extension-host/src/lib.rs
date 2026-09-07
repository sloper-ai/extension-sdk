#![warn(missing_debug_implementations, missing_docs, rust_2018_idioms, unreachable_pub)]
#![doc(test(
    no_crate_inject,
    attr(deny(warnings, rust_2018_idioms), allow(dead_code, unused_variables))
))]
//! The shared component admission and execution boundary.
//!
//! Enable the `runtime` feature to execute components with caller-supplied
//! capabilities through `Host`. Each attempt receives a restricted WASI 0.2
//! context and outbound HTTP.

mod component;
#[macro_use]
mod macros;
mod error;
cfg_runtime! {
    mod engine;
    mod release;
}
mod world;

pub use component::{
    ComponentError,
    assemble_parts,
    check_component,
    extract_manifest,
    extract_parts,
    stamp_manifest,
};
pub use error::Error;
cfg_runtime! {
pub use engine::{
    AccessToken,
    AdmittedComponent,
    Cursor,
    Engine,
    Failure,
    Host,
    Item,
    LogKind,
    LogLevel,
    Page,
    Request,
    Source,
};
pub use error::HostError;
pub use release::{
    AdmittedRelease,
    Error as ReleaseError,
    Release,
    Trust,
    TrustRoots,
};
}
pub use world::validate_component;
