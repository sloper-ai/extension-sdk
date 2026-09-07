#![warn(missing_debug_implementations, missing_docs, rust_2018_idioms, unreachable_pub)]
#![doc(test(
    no_crate_inject,
    attr(deny(warnings, rust_2018_idioms), allow(dead_code, unused_variables))
))]

//! Typed guest capabilities for the Sloper extension component world.

mod error;
mod runtime;
mod schema;
pub use error::{
    Error,
    Result,
};
pub use runtime::{
    AccessToken,
    Batch,
    Connection,
    Reader,
    Scanned,
    Source,
    SourceReader,
    SourceWriter,
    Writer,
    configuration,
    operation,
};
pub use schema::{
    ConnectionType,
    Fields,
    Resource,
    Schema,
};
pub use sloper_extension_macros::{
    Connection,
    Resource,
    Schema,
    action,
    extension,
};

// Hidden macro support is an implementation detail outside the stable API.
#[doc(hidden)]
pub mod __private;
