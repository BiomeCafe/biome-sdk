//! The three ways a run can fail, each with its own exit code (spec §7).

use std::fmt;

/// Exit code: everything went through.
pub const EXIT_OK: i32 = 0;
/// Exit code: the manifest, a file it names, or the local setup is wrong.
pub const EXIT_INVALID: i32 = 1;
/// Exit code: the server (or the storage behind a presigned URL) said no.
pub const EXIT_REFUSED: i32 = 2;
/// Exit code: the network or the local disk failed.
pub const EXIT_IO: i32 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Validation failed. One problem per line.
    Invalid(String),
    /// The server answered, and the answer was no (or was unusable).
    Refused(Refusal),
    /// Network or local I/O failure.
    Io(String),
}

/// A non-success answer from the `game-publish` function or from storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// HTTP status, 0 when the problem was the shape of a success answer.
    pub status: u16,
    /// The server's `error` code (`version-exists`, `unauthorized`, ...).
    pub error: String,
    /// The server's `message`, when it sent one.
    pub message: Option<String>,
    /// Extra lines worth showing (`status` of an existing release, the
    /// `missing` uploads, a storage error body).
    pub details: Vec<String>,
}

impl Error {
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Invalid(_) => EXIT_INVALID,
            Error::Refused(_) => EXIT_REFUSED,
            Error::Io(_) => EXIT_IO,
        }
    }

    pub fn invalid(msg: impl Into<String>) -> Self {
        Error::Invalid(msg.into())
    }

    pub fn io(msg: impl Into<String>) -> Self {
        Error::Io(msg.into())
    }

    /// A success status whose body could not be used.
    pub fn bad_response(msg: impl Into<String>) -> Self {
        Error::Refused(Refusal { status: 0, error: "bad-response".into(), message: Some(msg.into()), details: Vec::new() })
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Invalid(msg) => write!(f, "validation failed:\n{}", indent(msg)),
            Error::Io(msg) => write!(f, "{msg}"),
            Error::Refused(r) => {
                if r.status == 0 {
                    write!(f, "unusable answer from the server ({})", r.error)?;
                } else {
                    write!(f, "the server refused the request (HTTP {} {})", r.status, r.error)?;
                }
                if let Some(m) = &r.message {
                    write!(f, ": {m}")?;
                }
                for d in &r.details {
                    write!(f, "\n  {d}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for Error {}

fn indent(msg: &str) -> String {
    msg.lines().map(|l| format!("  {l}")).collect::<Vec<_>>().join("\n")
}
