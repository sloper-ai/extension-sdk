//! Failures while materializing the canonical build inputs.

/// A canonical spec resource could not be written.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Filesystem access failed while writing WIT inputs.
    #[error("canonical WIT inputs could not be written")]
    Io(#[from] std::io::Error),
}
