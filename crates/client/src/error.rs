//! Typed client errors.

use std::fmt;

use yas_wire::{Extensions, core::Status};

/// Result alias used throughout this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Everything that can go wrong while talking to a YAS server.
///
/// The variants separate the cases a caller reacts to differently:
///
/// - [`Error::Connect`]: the endpoint could not be reached or authenticated;
///   nothing was sent. Retrying later is safe.
/// - [`Error::Disconnected`] and [`Error::GoAway`]: an established session
///   ended. Requests in flight have an unknown outcome unless they carried an
///   operation ID (Process `SPAWN`/`CONTROL`, FS `COMMIT`/`APPLY`), in which
///   case the same request may be retried on a new session of the *same
///   server boot*.
/// - [`Error::Status`]: the server answered with a non-OK status, such as
///   `NotFound` or `Conflict`. [`Error::status`] extracts it.
/// - [`Error::Timeout`]: no answer before a local deadline.
/// - [`Error::Unsupported`]: the negotiated catalogue lacks the family or
///   operation (for instance a server started with `--no-processes`, or a
///   read-only session).
/// - [`Error::Protocol`]: the peer violated the protocol, or a value could
///   not be encoded. Not retryable on the same session.
/// - [`Error::Invalid`]: the caller passed an argument the protocol cannot
///   carry (an empty argv, a relative path where an absolute one is needed).
/// - [`Error::Closed`]: the local handle was closed or dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The endpoint could not be reached, started, or authenticated.
    Connect(String),
    /// The session's byte stream failed or reached end of file.
    Disconnected(String),
    /// The server announced it is closing the session.
    GoAway {
        /// Status carried by `GOAWAY`.
        status: Status,
        /// Rendered detail extensions.
        detail: String,
    },
    /// A request completed with a status other than `OK`.
    Status {
        /// What failed, for messages (for example `YAS request 0x0040/0x0002`).
        operation: String,
        /// The status the server returned.
        status: Status,
        /// Rendered detail extensions.
        detail: String,
        /// Raw detail extensions, for family-specific decoding (for example
        /// FS `ConflictDetail`).
        extensions: Extensions,
    },
    /// A local deadline passed before the server answered.
    Timeout(String),
    /// The negotiated catalogue does not offer this family or operation.
    Unsupported(String),
    /// The peer violated the protocol, or a local value failed to encode.
    Protocol(String),
    /// The caller passed an argument the protocol cannot carry.
    Invalid(String),
    /// The client was closed locally.
    Closed,
}

impl Error {
    /// The server status for [`Error::Status`], `None` otherwise.
    pub fn status(&self) -> Option<Status> {
        match self {
            Self::Status { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// Whether the server answered `NOT_FOUND`.
    pub fn is_not_found(&self) -> bool {
        self.status() == Some(Status::NotFound)
    }

    /// Whether the server answered `CONFLICT` (a failed precondition).
    pub fn is_conflict(&self) -> bool {
        self.status() == Some(Status::Conflict)
    }

    /// Whether the session is gone ([`Error::Disconnected`],
    /// [`Error::GoAway`] or [`Error::Closed`]); a new connection is needed.
    pub fn is_disconnected(&self) -> bool {
        matches!(
            self,
            Self::Disconnected(_) | Self::GoAway { .. } | Self::Closed
        )
    }

    pub(crate) fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol(message.into())
    }

    pub(crate) fn disconnected(message: impl Into<String>) -> Self {
        Self::Disconnected(message.into())
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }

    pub(crate) fn status_from(
        operation: impl Into<String>,
        status: Status,
        extensions: Extensions,
    ) -> Self {
        Self::Status {
            operation: operation.into(),
            status,
            detail: format_result_detail(&extensions),
            extensions,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(message)
            | Self::Disconnected(message)
            | Self::Timeout(message)
            | Self::Unsupported(message)
            | Self::Protocol(message)
            | Self::Invalid(message) => f.write_str(message),
            Self::GoAway { status, detail } => {
                write!(f, "YAS server is closing with {status:?}: {detail}")
            }
            Self::Status {
                operation,
                status,
                detail,
                ..
            } => write!(f, "{operation} failed with {status:?}: {detail}"),
            Self::Closed => f.write_str("YAS client is closed"),
        }
    }
}

impl std::error::Error for Error {}

impl From<Error> for String {
    fn from(error: Error) -> Self {
        error.to_string()
    }
}

impl From<yas_wire::Error> for Error {
    fn from(error: yas_wire::Error) -> Self {
        Self::Protocol(format!("YAS wire error: {error}"))
    }
}

pub(crate) fn wire_error(error: yas_wire::Error) -> Error {
    Error::from(error)
}

/// Render Result or GOAWAY detail extensions as `tag N[!]=hex` items.
pub fn format_result_detail(detail: &Extensions) -> String {
    if detail.0.is_empty() {
        "no detail".to_string()
    } else {
        detail
            .0
            .iter()
            .map(|extension| {
                format!(
                    "tag {}{}={}",
                    extension.tag,
                    if extension.required { "!" } else { "" },
                    hex(&extension.value)
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[usize::from(byte >> 4)] as char);
        output.push(DIGITS[usize::from(byte & 0x0f)] as char);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_errors_render_like_the_cli_always_did() {
        let error = Error::status_from(
            "YAS request 0x0040/0x0002",
            Status::NotFound,
            Extensions::default(),
        );
        assert_eq!(
            error.to_string(),
            "YAS request 0x0040/0x0002 failed with NotFound: no detail"
        );
        assert!(error.is_not_found());
        assert!(!error.is_disconnected());
        let text: String = error.into();
        assert!(text.contains("NotFound"));
    }
}
