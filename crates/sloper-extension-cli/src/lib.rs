#![warn(missing_debug_implementations, missing_docs, rust_2018_idioms, unreachable_pub)]
#![doc(test(
    no_crate_inject,
    attr(deny(warnings, rust_2018_idioms), allow(dead_code, unused_variables))
))]
//! Extension authoring, local execution, and publication tools.
//!
//! JSON values in local execution describe extension-owned schemas and
//! operation receipts; credentials and explicit trusted roots are supplied by
//! callers.

mod assembly;
mod error;

mod icon;
mod io;
mod options;
mod publication;
mod runner;
mod scaffold;
mod validation;

pub use assembly::{
    BuildResult,
    CheckResult,
    build,
    check,
};
pub use error::Error;
pub(crate) use io::{
    append_hex,
    hex,
    read_bounded,
    read_component,
    serialized_exceeds,
    text,
};
pub use options::{
    ResourceResult,
    RunOptions,
    RunResult,
    SourceResult,
    run,
};
pub use publication::{
    DEFAULT_API_URL,
    ExtensionComponentDownload,
    ExtensionVersion,
    ExtensionVersionState,
    OwnerKind,
    PublicationError,
    PublishOptions,
    PublishResult,
    PublisherIdentity,
    ReleaseEnvelope,
    VerifyOptions,
    VerifyResult,
    Visibility,
    publish,
    verify,
};
pub use scaffold::{
    ScaffoldResult,
    scaffold,
    sdk_revision,
};
pub use validation::{
    Validation,
    ValidationError,
    validate_file,
};
