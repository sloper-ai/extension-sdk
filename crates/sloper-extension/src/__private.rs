//! Implementation details used by generated declarations, permitted to change.

pub use bindings::exports::sloper::extension::action::{
    Failure,
    Guest,
    Request,
};

pub use crate::{
    __sloper_embed_part as embed_part,
    __sloper_export as export,
    runtime::{
        RunContext,
        dispatch,
    },
    schema::{
        ActionParts,
        ConnectionUse,
        ResourceUse,
        SchemaBuffer,
        action_part,
        const_bytes,
        equal,
        extension_parts,
    },
};

pub fn unknown_action() -> crate::Result<()> {
    Err(crate::Error::invalid_parameters("action is not declared"))
}

/// Implementation detail embedding manifest metadata without caller-owned
/// unsafe code.
#[doc(hidden)]
#[macro_export]
macro_rules! __sloper_embed_part {
    ($name:ident, $json:expr) => {
        // SAFETY: this fixed custom section contains metadata only, never executable
        // symbols.
        #[used]
        #[cfg_attr(target_arch = "wasm32", unsafe(link_section = "sloper:parts"))]
        static $name: [u8; ($json).len()] = $crate::__private::const_bytes::<{ ($json).len() }>($json);
    };
}

/// Implementation detail forwarding the component export macro.
#[doc(hidden)]
#[macro_export]
macro_rules! __sloper_export {
    ($($tokens:tt)*) => { $crate::__private::bindings::export!($($tokens)*); };
}

// The generator owns canonical ABI shims; handwritten guest code stays safe.
#[allow(
    unsafe_code,
    unsafe_op_in_unsafe_fn,
    missing_docs,
    clippy::all,
    clippy::module_name_repetitions,
    clippy::pedantic
)]
#[doc(hidden)]
pub mod bindings {
    include!("bindings.rs");
}
