//! Error type with exit-code mapping.
//!
//! Exit code convention, the same one go-can uses:
//!   0 — success
//!   1 — user error (bad argument, unknown mode, a module that refused a request)
//!   2 — system error (no permission, a device that went away, a parse failure)

use thiserror::Error;

/// Human wording for a protocol status byte, matching the table in
/// `Documentation/multibus-usb-protocol.md`.
pub fn status_text(status: u8) -> &'static str {
    match status {
        0x00 => "ok",
        0x01 => "unknown command",
        0x02 => "bad length",
        0x03 => "bad parameter",
        0x04 => "not supported in this role",
        0x05 => "isoSPI link down",
        0x06 => "busy",
        _ => "unknown status",
    }
}

#[derive(Debug, Error)]
pub enum Error {
    /// Something the caller asked for that cannot be honoured.
    #[error("{0}")]
    User(String),

    /// Something about the machine that stopped the request.
    #[error("system error: {0}")]
    System(String),

    /// The module did not answer, or answered with something unusable.
    #[error("{0}")]
    Module(String),

    /// The module answered, with a status other than ok. `status` is kept
    /// rather than folded into the message because callers act on it: a module
    /// running older firmware answers `unknown command` to anything new, and
    /// that is a different situation from a value it rejected.
    #[error("command {command:#04X} refused: {reason}")]
    Status {
        command: u8,
        status: u8,
        reason: &'static str,
    },

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

impl Error {
    pub fn user(message: impl Into<String>) -> Self {
        Error::User(message.into())
    }

    pub fn system(message: impl Into<String>) -> Self {
        Error::System(message.into())
    }

    pub fn module(message: impl Into<String>) -> Self {
        Error::Module(message.into())
    }

    pub fn status(command: u8, status: u8) -> Self {
        Error::Status {
            command,
            status,
            reason: status_text(status),
        }
    }

    /// Whether this is the module saying it has no handler for the command.
    /// Used to tell "your firmware is older than this tool" apart from a real
    /// refusal, so the UI can say so instead of showing a raw status byte.
    pub fn is_unknown_command(&self) -> bool {
        matches!(self, Error::Status { status: 0x01, .. })
    }

    pub fn exit_code(&self) -> i32 {
        match self {
            Error::User(_) | Error::Status { .. } => 1,
            _ => 2,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
