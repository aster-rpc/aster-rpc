//! The CLI error type: a kind that fixes the exit code, a message, the cause
//! chain and an optional hint. Any `std::error::Error` or `anyhow::Error`
//! converts with `?` as [`ErrorKind::Failure`].

use std::fmt;
use std::io::Write;

use serde::Serialize;
use serde_json::json;

use crate::out::{ERROR, HINT, Styler};

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// Anything not classified below.
    Failure,
    /// Invalid arguments or an operation impossible in this mode.
    Usage,
    /// A named profile, key, node or job does not exist.
    NotFound,
    /// Authentication or authorisation refused.
    Denied,
    /// A peer, daemon or service could not be reached.
    Unavailable,
    /// Cancelled by the user (Ctrl-C, `q`).
    Cancelled,
}

impl ErrorKind {
    pub fn exit_code(self) -> u8 {
        match self {
            Self::Failure => 1,
            Self::Usage => 2,
            Self::NotFound => 3,
            Self::Denied => 4,
            Self::Unavailable => 5,
            Self::Cancelled => 130,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Error {
    kind: ErrorKind,
    message: String,
    causes: Vec<String>,
    hint: Option<String>,
    details: Option<serde_json::Value>,
}

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            causes: Vec::new(),
            hint: None,
            details: None,
        }
    }

    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Usage, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, message)
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn with_kind(mut self, kind: ErrorKind) -> Self {
        self.kind = kind;
        self
    }

    /// Structured application error context carried across local tool transports.
    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }
    pub fn details(&self) -> Option<&serde_json::Value> {
        self.details.as_ref()
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn to_json(&self) -> serde_json::Value {
        let mut value = json!({"error": {
            "kind": self.kind,
            "message": self.message,
            "causes": self.causes,
            "hint": self.hint,
        }});
        if let Some(details) = &self.details {
            value["error"]["details"] = details.clone();
        }
        value
    }

    /// Write the error to stderr in the human or JSON form.
    pub fn report(&self, json: bool, style: Styler) {
        let mut err = std::io::stderr().lock();
        if json {
            let _ = writeln!(err, "{}", self.to_json());
            return;
        }
        // A user's own cancellation is reported, not treated as a fault.
        if self.kind == ErrorKind::Cancelled {
            let _ = writeln!(err, "{}", self.message);
            return;
        }
        let _ = writeln!(err, "{} {}", style.paint(ERROR, "error:"), self.message);
        for cause in &self.causes {
            let _ = writeln!(err, "  caused by: {cause}");
        }
        if let Some(hint) = &self.hint {
            let _ = writeln!(err, "  {} {hint}", style.paint(HINT, "hint:"));
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)?;
        for cause in &self.causes {
            write!(f, ": {cause}")?;
        }
        Ok(())
    }
}

// Any `std::error::Error` or `anyhow::Error`. `Error` deliberately does not
// implement `std::error::Error`, so this does not overlap the reflexive
// `From<T> for T`.
impl<E: Into<anyhow::Error>> From<E> for Error {
    fn from(e: E) -> Self {
        let e: anyhow::Error = e.into();
        let mut chain = e.chain().map(ToString::to_string);
        Self {
            kind: ErrorKind::Failure,
            message: chain.next().unwrap_or_default(),
            causes: chain.collect(),
            hint: None,
            details: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    #[test]
    fn anyhow_chain_becomes_message_and_causes() {
        let e: Error = Err::<(), _>(std::io::Error::other("disk full"))
            .context("saving config")
            .unwrap_err()
            .into();
        assert_eq!(e.kind(), ErrorKind::Failure);
        assert_eq!(e.message(), "saving config");
        assert_eq!(e.to_string(), "saving config: disk full");
    }

    #[test]
    fn json_form_is_stable() {
        let e = Error::not_found("no profile `x`").with_hint("run `aster profile list`");
        assert_eq!(
            e.to_json(),
            json!({"error": {"kind": "not_found", "message": "no profile `x`", "causes": [], "hint": "run `aster profile list`"}})
        );
        assert_eq!(e.kind().exit_code(), 3);
    }
}
