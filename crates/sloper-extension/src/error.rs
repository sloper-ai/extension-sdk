//! Stable redacted errors at guest capability boundaries.

use std::{
    error::Error as StdError,
    fmt,
    io,
};

use time::OffsetDateTime;
use wasip2::clocks::wall_clock::Datetime;

use crate::__private::bindings::{
    exports::sloper::extension::action::{
        Failure,
        Retry,
    },
    sloper::api::errors::Error as WitError,
};

/// Result returned by an extension action or capability.
pub type Result<T> = std::result::Result<T, Error>;

/// A stable error category carrying only authored static diagnostic text.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The owner must repair a provider connection.
    NotConnected(&'static str),
    /// Authored action parameters are invalid.
    InvalidParameters(&'static str),
    /// The provider refused the action.
    Rejected(&'static str),
    /// A declared or platform ceiling was exceeded.
    TooLarge,
    /// The attempt was cancelled or fenced.
    Stopped,
    /// A dependency may succeed on a subsequent attempt.
    Unavailable {
        /// Static diagnostic supplied by the action author.
        message: &'static str,
        /// Earliest provider retry time, if known.
        not_before: Option<OffsetDateTime>,
    },
    /// An invalid host argument or local action failure.
    Internal(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotConnected(message)
            | Self::InvalidParameters(message)
            | Self::Rejected(message)
            | Self::Internal(message)
            | Self::Unavailable {
                message, ..
            } => message,
            Self::TooLarge => "value exceeds its declared limit",
            Self::Stopped => "operation stopped",
        })
    }
}
impl StdError for Error {}

impl Error {
    /// Reports that the owner must reconnect the provider account.
    #[must_use]
    pub const fn not_connected(message: &'static str) -> Self {
        Self::NotConnected(message)
    }

    /// Reports invalid authored action parameters.
    #[must_use]
    pub const fn invalid_parameters(message: &'static str) -> Self {
        Self::InvalidParameters(message)
    }

    /// Reports a terminal provider refusal.
    #[must_use]
    pub const fn rejected(message: &'static str) -> Self {
        Self::Rejected(message)
    }

    /// Reports a transient failure and an optional earliest retry timestamp.
    ///
    /// ```
    /// use sloper_extension::Error;
    ///
    /// let retry_at = time::OffsetDateTime::UNIX_EPOCH;
    /// let error = Error::unavailable("Provider is temporarily unavailable.", Some(retry_at));
    /// ```
    #[must_use]
    pub const fn unavailable(message: &'static str, not_before: Option<OffsetDateTime>) -> Self {
        Self::Unavailable {
            message,
            not_before,
        }
    }

    /// Reports an action implementation failure.
    #[must_use]
    pub const fn internal(message: &'static str) -> Self {
        Self::Internal(message)
    }

    pub(crate) fn from_wit(error: &WitError) -> Self {
        // Host diagnostics can contain remote data. This boundary exposes only
        // static text; invalid host arguments are author implementation errors.
        match error {
            WitError::Unauthorized => Self::not_connected("connection is not available"),
            WitError::Invalid(_) => Self::internal("invalid capability request"),
            WitError::TooLarge => Self::TooLarge,
            WitError::Cancelled => Self::Stopped,
            WitError::Unavailable(_) => Self::unavailable("dependency temporarily unavailable", None),
        }
    }

    pub(crate) fn into_failure(self) -> Failure {
        match self {
            Self::NotConnected(message) => Failure::NotConnected(message.into()),
            Self::InvalidParameters(message) => Failure::InvalidParameters(message.into()),
            Self::Rejected(message) => Failure::Rejected(message.into()),
            Self::TooLarge => Failure::Rejected(self.to_string()),
            Self::Unavailable {
                message,
                not_before,
            } => {
                let Ok(not_before) = not_before.as_ref().map(datetime).transpose() else {
                    return Failure::Internal("invalid retry timestamp".into());
                };
                Failure::Unavailable(Retry {
                    message: message.into(),
                    not_before,
                })
            },
            Self::Stopped => Failure::Internal(self.to_string()),
            Self::Internal(message) => Failure::Internal(message.into()),
        }
    }
}

fn datetime(value: &OffsetDateTime) -> std::result::Result<Datetime, ()> {
    Ok(Datetime {
        seconds: u64::try_from(value.unix_timestamp()).map_err(|_| ())?,
        nanoseconds: value.nanosecond(),
    })
}

impl From<Error> for io::Error {
    fn from(error: Error) -> Self {
        let kind = match error {
            Error::TooLarge => io::ErrorKind::OutOfMemory,
            Error::Stopped => io::ErrorKind::Interrupted,
            Error::NotConnected(_) => io::ErrorKind::PermissionDenied,
            Error::InvalidParameters(_) => io::ErrorKind::InvalidInput,
            _ => io::ErrorKind::Other,
        };
        Self::new(kind, error)
    }
}
impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        let mut source: Option<&(dyn StdError + 'static)> = Some(&error);
        while let Some(cause) = source {
            if let Some(error) = cause.downcast_ref::<Self>() {
                return error.clone();
            }
            source = if let Some(adapter) = cause.downcast_ref::<io::Error>() {
                adapter.get_ref().map(|inner| inner as &(dyn StdError + 'static))
            } else {
                cause.source()
            };
        }
        // Foreign I/O diagnostic text can include provider response bodies.
        Self::unavailable("dependency temporarily unavailable", None)
    }
}
impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        if error.io_error_kind() == Some(io::ErrorKind::OutOfMemory) {
            Self::TooLarge
        } else {
            Self::internal("resource serialization failed")
        }
    }
}

#[cfg(test)]
mod tests {
    use time::format_description::well_known::Rfc3339;

    use super::*;

    #[test]
    fn sdk_categories_and_payloads_survive_io_adapters() {
        let errors = [
            Error::TooLarge,
            Error::Stopped,
            Error::not_connected("Reconnect the account."),
            Error::invalid_parameters("Choose a mailbox."),
            Error::rejected("Provider rejected the action."),
            Error::unavailable(
                "Retry later.",
                Some(OffsetDateTime::parse("2030-01-02T03:04:05.123456789+02:00", &Rfc3339).unwrap()),
            ),
            Error::internal("Unexpected action state."),
        ];
        for error in errors {
            let nested = io::Error::other(io::Error::from(error.clone()));
            assert_eq!(Error::from(nested), error);
        }
    }

    #[test]
    fn foreign_diagnostics_do_not_cross_the_boundary() {
        let secret = "Bearer secret-provider-token-and-response";
        for error in [
            Error::from(io::Error::other(secret)),
            Error::from_wit(&WitError::Unavailable(secret.into())),
            Error::from_wit(&WitError::Invalid(secret.into())),
        ] {
            assert!(!error.to_string().contains(secret));
            assert!(!format!("{error:?}").contains(secret));
        }
        assert!(matches!(
            Error::from_wit(&WitError::Unauthorized),
            Error::NotConnected(_)
        ));
        assert!(matches!(
            Error::from_wit(&WitError::Invalid(secret.into())),
            Error::Internal(_)
        ));
    }

    #[test]
    fn authored_failures_retain_messages_and_retry_time() {
        assert!(
            matches!(Error::rejected("Provider rejected the action.").into_failure(), Failure::Rejected(message) if message == "Provider rejected the action.")
        );
        let retry = Error::unavailable(
            "Retry later.",
            Some(OffsetDateTime::parse("1970-01-02T02:00:01.123456789+02:00", &Rfc3339).unwrap()),
        )
        .into_failure();
        let Failure::Unavailable(retry) = retry else {
            panic!("expected retry failure")
        };
        assert_eq!(retry.message, "Retry later.");
        let datetime = retry.not_before.unwrap();
        assert_eq!(datetime.seconds, 86_401);
        assert_eq!(datetime.nanoseconds, 123_456_789);
        assert!(matches!(
            Error::unavailable("Retry later.", Some(OffsetDateTime::from_unix_timestamp(-1).unwrap())).into_failure(),
            Failure::Internal(_)
        ));
    }
}
