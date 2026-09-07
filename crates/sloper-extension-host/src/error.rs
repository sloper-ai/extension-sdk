//! Typed failures at admission and execution boundaries.

cfg_runtime! {
use std::{
    error::Error as StdError,
    io::Error as IoError,
    sync::Arc,
};

}

use crate::ComponentError;

/// A component cannot be admitted or its attempt cannot complete.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Component bytes fail extraction, manifest validation, or static world
    /// validation.
    #[error("extension component is invalid")]
    Component(#[from] ComponentError),
    /// The engine rejected code or failed to create its runtime instance.
    #[error("extension execution failed")]
    #[cfg(feature = "runtime")]
    Runtime(#[from] wasmtime::Error),
    /// Guest computation trapped outside a normal host-call result.
    #[error("extension trapped")]
    #[cfg(feature = "runtime")]
    Trapped(#[source] wasmtime::Error),
    /// A pinned request disagrees with its admitted component.
    #[error("invalid extension input: {0}")]
    #[cfg(feature = "runtime")]
    Invalid(&'static str),
    /// The attempt exhausted its absolute execution deadline.
    #[error("extension deadline exceeded")]
    #[cfg(feature = "runtime")]
    Deadline,
    /// The persisted cancellation grace period ended.
    #[error("operation stopped")]
    #[cfg(feature = "runtime")]
    Stopped,
    /// An ignored or unfinished source writer prevents success.
    #[error("source writer failed")]
    #[cfg(feature = "runtime")]
    SourceFailed,
    /// Reading a lent source failed, even if the guest ignored it.
    #[error("source could not be read")]
    #[cfg(feature = "runtime")]
    SourceUnreadable,
    /// Reading a lent source failed with a retained native cause.
    #[error("source could not be read")]
    #[cfg(feature = "runtime")]
    SourceRead(#[source] Arc<dyn StdError + Send + Sync>),
    /// An internal guest failure followed a rejected item write.
    #[error("extension item was invalid")]
    #[cfg(feature = "runtime")]
    ItemInvalid,
    /// The guest exhausted a documented ceiling.
    #[error("extension limit exceeded")]
    #[cfg(feature = "runtime")]
    LimitExceeded,
    /// A caller-supplied adapter failed while finishing the attempt.
    #[error("extension host failed")]
    #[cfg(feature = "runtime")]
    Host(#[from] HostError),
    /// An independent epoch ticker could not be started.
    #[error("extension deadline watcher could not start")]
    #[cfg(feature = "runtime")]
    Watchdog(#[source] IoError),
    /// The private scratch directory for an attempt could not be created.
    #[error("extension scratch directory could not be created")]
    #[cfg(feature = "runtime")]
    Scratch(#[source] IoError),
}

cfg_runtime! {
impl Error {
    /// Reports an invalid pinned request with static, safe text.
    #[must_use]
    pub const fn invalid(message: &'static str) -> Self {
        Self::Invalid(message)
    }

    /// Reports elapsed attempt time.
    #[must_use]
    pub const fn deadline() -> Self {
        Self::Deadline
    }

    /// Reports elapsed cancellation grace.
    #[must_use]
    pub const fn stopped() -> Self {
        Self::Stopped
    }

    /// Reports an unsuccessful source writer.
    #[must_use]
    pub const fn source_failed() -> Self {
        Self::SourceFailed
    }

    /// Reports a failed source reader.
    #[must_use]
    pub const fn source_unreadable() -> Self {
        Self::SourceUnreadable
    }

    /// Reports an invalid staged item.
    #[must_use]
    pub const fn item_invalid() -> Self {
        Self::ItemInvalid
    }

    /// Reports a documented memory, item, or source ceiling.
    #[must_use]
    pub const fn limit_exceeded() -> Self {
        Self::LimitExceeded
    }

    pub(crate) fn watchdog(source: IoError) -> Self {
        Self::Watchdog(source)
    }

    pub(crate) fn scratch(source: IoError) -> Self {
        Self::Scratch(source)
    }
}

/// Static redacted errors from caller-supplied capability implementations.
#[derive(Clone, Debug, thiserror::Error)]
pub enum HostError {
    /// The owner must repair the declared provider connection.
    #[error("connection requires repair")]
    Unauthorized,
    /// The guest supplied an invalid capability argument.
    #[error("invalid extension input: {0}")]
    Invalid(&'static str),
    /// An item or source exceeds its documented ceiling.
    #[error("extension input exceeds its limit")]
    TooLarge,
    /// Cancellation or a stale attempt fence prevents further writes.
    #[error("operation stopped")]
    Stopped,
    /// A lent source is unreadable.
    #[error("source could not be read")]
    SourceUnreadable,
    /// A lent source failed with an underlying transport or storage cause.
    #[error("source could not be read")]
    SourceRead(#[source] Arc<dyn StdError + Send + Sync>),
    /// Persisting produced source bytes failed with a native storage cause.
    #[error("source could not be written")]
    SourceWrite(#[source] Arc<dyn StdError + Send + Sync>),
    /// The dependency may succeed on a subsequent attempt.
    #[error("extension host is unavailable")]
    Unavailable,
}

impl HostError {
    /// Reports a declared connection requiring repair.
    #[must_use]
    pub const fn unauthorized() -> Self {
        Self::Unauthorized
    }

    /// Reports a guest capability argument error.
    #[must_use]
    pub const fn invalid(message: &'static str) -> Self {
        Self::Invalid(message)
    }

    /// Reports a documented ceiling.
    #[must_use]
    pub const fn too_large() -> Self {
        Self::TooLarge
    }

    /// Reports cancellation or fencing.
    #[must_use]
    pub const fn stopped() -> Self {
        Self::Stopped
    }

    /// Reports unreadable lent bytes.
    #[must_use]
    pub const fn source_unreadable() -> Self {
        Self::SourceUnreadable
    }

    /// Reports unreadable lent bytes while preserving the native cause for
    /// server-side diagnostics.
    pub fn source_read(source: impl StdError + Send + Sync + 'static) -> Self {
        Self::SourceRead(Arc::new(source))
    }

    /// Reports a failed produced-source write while preserving its native
    /// cause.
    pub fn source_write(source: impl StdError + Send + Sync + 'static) -> Self {
        Self::SourceWrite(Arc::new(source))
    }

    /// Reports a transient dependency failure.
    #[must_use]
    pub const fn unavailable() -> Self {
        Self::Unavailable
    }
}

}
