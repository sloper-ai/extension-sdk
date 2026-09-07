//! Precise local failures and their intentional command-line classifications.
use std::{
    fmt,
    io,
};

use serde_json::{
    Value,
    json,
};
use sloper_extension_host::{
    ComponentError,
    Error as HostRuntimeError,
    HostError,
    ReleaseError,
};
use sloper_extension_spec::{
    ManifestError,
    SourceFilenameError,
};

/// Failure from local tooling or authenticated publication.
///
/// Debug output excludes developer inputs, local paths, and credentials.
#[derive(thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The supplied arguments are invalid.
    #[error("{0}")]
    Usage(String),
    /// An extension input violates a tool constraint.
    #[error("{0}")]
    Invalid(String),
    /// Input does not satisfy a declared schema.
    #[error("input failed validation")]
    Validation(Value),
    /// Component checking produced structured findings.
    #[error("component validation failed")]
    ComponentCheck(Value),
    /// Component structure or its static world is invalid.
    #[error(transparent)]
    Component(#[from] ComponentError),
    /// Manifest rules were violated.
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    /// Source filename rules were violated.
    #[error(transparent)]
    SourceFilename(#[from] SourceFilenameError),
    /// An icon could not be decoded.
    #[error("extension icon is invalid")]
    Png(#[from] png::DecodingError),
    /// Runtime admission or execution failed.
    #[error(transparent)]
    Execution(#[from] HostRuntimeError),
    /// A local host capability failed.
    #[error(transparent)]
    Host(#[from] HostError),
    /// Release authentication failed.
    #[error(transparent)]
    Release(#[from] ReleaseError),
    /// An HTTP request could not be constructed or sent.
    #[error("The publication service could not be reached")]
    Http(#[from] reqwest::Error),
    /// Publication failed at the authenticated HTTP boundary.
    #[error(transparent)]
    Publication(#[from] crate::PublicationError),
    /// Static validation could not read or extract its input.
    #[error(transparent)]
    ValidationInput(#[from] crate::ValidationError),
    /// A trusted peer or subprocess returned an invalid result.
    #[error("{0}")]
    InvalidResponse(String),
    /// JSON input or output could not be encoded.
    #[error("JSON input is malformed")]
    Json(#[from] serde_json::Error),
    /// Filesystem work could not complete.
    #[error("the command could not complete its file operation")]
    Io(#[from] io::Error),
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Usage(_) => "Usage",
            Self::Invalid(_) => "Invalid",
            Self::Validation(_) => "Validation",
            Self::ComponentCheck(_) => "ComponentCheck",
            Self::Component(_) => "Component",
            Self::Manifest(_) => "Manifest",
            Self::SourceFilename(_) => "SourceFilename",
            Self::Png(_) => "Png",
            Self::Execution(_) => "Execution",
            Self::Host(_) => "Host",
            Self::Release(_) => "Release",
            Self::Http(_) => "Http",
            Self::Publication(_) => "Publication",
            Self::ValidationInput(_) => "ValidationInput",
            Self::InvalidResponse(_) => "InvalidResponse",
            Self::Json(_) => "Json",
            Self::Io(_) => "Io",
        })
    }
}

impl Error {
    pub(crate) fn usage(message: impl Into<String>) -> Self {
        Self::Usage(message.into())
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }

    pub(crate) fn validation(findings: Value) -> Self {
        Self::Validation(findings)
    }

    pub(crate) fn invalid_response(message: impl Into<String>) -> Self {
        Self::InvalidResponse(message.into())
    }

    /// Returns the stable shell category: usage 2, validation 3, conflict 4,
    /// missing file 5, unavailable 6, retryable 9, or operational failure 10.
    ///
    /// ```
    /// let error =
    ///     sloper_extension_cli::validate_file(std::path::Path::new("/missing/component.wasm"));
    /// assert!(error.is_err());
    /// ```
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Usage(_) | Self::Json(_) => 2,
            Self::Invalid(_)
            | Self::Validation(_)
            | Self::ComponentCheck(_)
            | Self::Component(_)
            | Self::Manifest(_)
            | Self::SourceFilename(_)
            | Self::Png(_)
            | Self::Execution(_)
            | Self::Host(_)
            | Self::Release(_) => 3,
            Self::Io(error) if error.kind() == io::ErrorKind::NotFound => 5,
            Self::Http(_) => 9,
            Self::Publication(error) => error.exit_code(),
            Self::ValidationInput(_) | Self::InvalidResponse(_) | Self::Io(_) => 10,
        }
    }

    /// Serializes safe command failure details without credentials or remote
    /// bodies.
    #[must_use]
    pub fn command_error(&self) -> Value {
        let exit = self.exit_code();
        let code = match exit {
            10 if matches!(self, Self::InvalidResponse(_)) => "INVALID_RESPONSE",
            2 => "USAGE",
            3 => "VALIDATION_FAILED",
            4 => "CONFLICT",
            5 => "NOT_FOUND",
            6 => "AUTHENTICATION_FAILED",
            8 => "POLICY_BLOCKED",
            9 => "RESOURCE_EXHAUSTED",
            _ => "INTERNAL",
        };
        let mut result = json!({"code":code,"exit":exit,"message":self.to_string(),"retryable":exit==9,"details":{}});
        match self {
            Self::Component(error) => result["details"]["findings"] = json!(error.findings()),
            Self::Manifest(error) => result["details"]["findings"] = json!(error.findings()),
            Self::Validation(findings) => result["details"]["findings"] = findings.clone(),
            Self::Publication(error) => result["details"] = error.details(),
            Self::ComponentCheck(check) => {
                result["details"]["findings"] = check["findings"].clone();
                result["details"]["check"] = check.clone();
            },
            _ => {},
        }
        result
    }
}

impl From<crate::Validation> for Error {
    fn from(verdict: crate::Validation) -> Self {
        Self::ComponentCheck(json!(verdict))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_peer_responses_keep_the_command_failure_category() {
        let error = Error::invalid_response("receipt does not match").command_error();
        assert_eq!(error["code"], "INVALID_RESPONSE");
        assert_eq!(error["exit"], 10);
        assert_eq!(error["retryable"], false);
    }
}
