//! Publication failures preserve generated response metadata and redact
//! diagnostics.

use std::fmt;

use generated_client::Error as ClientError;
use reqwest::header::InvalidHeaderValue;

#[derive(thiserror::Error)]
pub(crate) enum Error {
    #[error("HTTP header value is invalid")]
    HeaderValue(#[from] InvalidHeaderValue),
    #[error("Publication request failed")]
    Client(#[source] Box<ClientError>),
}

impl Error {
    pub(crate) fn api(&self) -> Option<&ClientError> {
        match self {
            Self::Client(error) => Some(error),
            Self::HeaderValue(_) => None,
        }
    }
}

impl From<ClientError> for Error {
    fn from(error: ClientError) -> Self {
        Self::Client(Box::new(error))
    }
}

impl From<ClientError> for crate::Error {
    fn from(error: ClientError) -> Self {
        Error::from(error).into()
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}
