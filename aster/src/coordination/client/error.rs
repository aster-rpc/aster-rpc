//! Structured coordination client errors.
//!
//! The client's failures used to be told apart by their message text — every
//! journal-state refusal was an `InvalidArgument`, and the lease adapter
//! flattened inner errors into its own text — so each binding re-derived the
//! taxonomy by string matching. [`CoordinationErrorKind::of`] is now the one
//! classification, and the refusals the client itself decides carry their
//! kind in [`Error::Coordination`](crate::Error::Coordination).
//!
//! Messages are unchanged: a structured error displays exactly the text its
//! untyped predecessor did, so logs and message-matching callers see no
//! difference.

use crate::error::Error;

/// What a coordination client failure means for the caller.
///
/// The bindings map these one-to-one onto their typed errors; match on the
/// kind, never on the message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CoordinationErrorKind {
    /// A resolved result awaits acknowledgement; take
    /// [`unacknowledged`](super::CoordinationClient::unacknowledged) first.
    AwaitingAck,
    /// Another request is unresolved on this journal;
    /// [`resume`](super::CoordinationClient::resume) first.
    Unresolved,
    /// Another process holds this actor/group journal's lifetime lock.
    JournalLocked,
    /// The journal is fatal (§10 reason 18): no further mutation is possible.
    JournalFatal,
    /// The group answered with a non-OK RPC status — a pre-proposal denial,
    /// for example. `code` is the [`StatusCode`](crate::rpc::StatusCode)
    /// value.
    Rpc { code: i32 },
    /// No answer resolved the request: no member answered, or an attempt
    /// budget ran out. A journaled sequence stays open and resumes with the
    /// same bytes.
    Unavailable,
    /// The client or handle was closed. The Rust client never closes itself;
    /// the kind exists so a binding's own `close()` reports through the same
    /// taxonomy.
    Closed,
    /// A committed typed rejection (§5.2 outcome 100) with its §5.2 reason
    /// code. A rejection is a *resolved result*, not a client failure, so the
    /// Rust client returns it as a reply; the kind exists for callers (and
    /// bindings) that raise a rejection as an error.
    Rejected { reason_code: i32 },
    /// An argument or request outside the contract.
    Invalid,
    /// Anything else.
    Other,
}

impl CoordinationErrorKind {
    /// Classify any error the coordination client returns.
    pub fn of(err: &Error) -> Self {
        match err {
            Error::Coordination(e) => e.kind(),
            Error::Rpc { code, .. } => Self::Rpc { code: *code },
            Error::Transport(_) => Self::Unavailable,
            Error::InvalidArgument(_) => Self::Invalid,
            _ => Self::Other,
        }
    }

    /// The stable snake_case name the bindings expose (`awaiting_ack`,
    /// `unresolved`, `journal_locked`, `journal_fatal`, `rpc`, `unavailable`,
    /// `closed`, `rejected`, `invalid`, `other`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AwaitingAck => "awaiting_ack",
            Self::Unresolved => "unresolved",
            Self::JournalLocked => "journal_locked",
            Self::JournalFatal => "journal_fatal",
            Self::Rpc { .. } => "rpc",
            Self::Unavailable => "unavailable",
            Self::Closed => "closed",
            Self::Rejected { .. } => "rejected",
            Self::Invalid => "invalid",
            Self::Other => "other",
        }
    }
}

/// A coordination client failure with a structured [`kind`](Self::kind).
///
/// Displays its message verbatim.
#[derive(Debug)]
pub struct CoordinationError {
    kind: CoordinationErrorKind,
    message: String,
    source: Option<Box<Error>>,
}

impl CoordinationError {
    /// A structured error with `message` as its full display text.
    pub fn new(kind: CoordinationErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }

    /// As [`new`](Self::new), keeping the underlying error as the source.
    pub(crate) fn with_source(
        kind: CoordinationErrorKind,
        message: impl Into<String>,
        source: Error,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }

    /// What the failure means for the caller.
    pub fn kind(&self) -> CoordinationErrorKind {
        self.kind
    }

    /// The full display text.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for CoordinationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CoordinationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|e| e as &(dyn std::error::Error + 'static))
    }
}

/// A journal-state refusal, worded exactly as its `InvalidArgument`
/// predecessor displayed (`invalid argument: coordination client: …`).
pub(crate) fn refusal(kind: CoordinationErrorKind, what: impl std::fmt::Display) -> Error {
    Error::Coordination(CoordinationError::new(
        kind,
        format!("invalid argument: coordination client: {what}"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_client_error_classifies_without_its_message() {
        let cases = [
            (
                refusal(CoordinationErrorKind::AwaitingAck, "x"),
                CoordinationErrorKind::AwaitingAck,
                "awaiting_ack",
            ),
            (
                refusal(CoordinationErrorKind::Unresolved, "x"),
                CoordinationErrorKind::Unresolved,
                "unresolved",
            ),
            (
                refusal(CoordinationErrorKind::JournalFatal, "x"),
                CoordinationErrorKind::JournalFatal,
                "journal_fatal",
            ),
            (
                CoordinationError::new(CoordinationErrorKind::JournalLocked, "x").into(),
                CoordinationErrorKind::JournalLocked,
                "journal_locked",
            ),
            (
                Error::Rpc {
                    code: 7,
                    message: "x".into(),
                    details: Vec::new(),
                },
                CoordinationErrorKind::Rpc { code: 7 },
                "rpc",
            ),
            (
                Error::Transport(anyhow::anyhow!("x")),
                CoordinationErrorKind::Unavailable,
                "unavailable",
            ),
            (
                Error::InvalidArgument("x".into()),
                CoordinationErrorKind::Invalid,
                "invalid",
            ),
            (
                Error::Connection("x".into()),
                CoordinationErrorKind::Other,
                "other",
            ),
        ];
        for (err, kind, name) in cases {
            assert_eq!(CoordinationErrorKind::of(&err), kind, "{err:?}");
            assert_eq!(kind.as_str(), name);
        }
        assert_eq!(CoordinationErrorKind::Closed.as_str(), "closed");
        assert_eq!(
            CoordinationErrorKind::Rejected { reason_code: 5 }.as_str(),
            "rejected"
        );
    }

    #[test]
    fn a_refusal_displays_its_untyped_predecessor_s_text() {
        let typed = refusal(
            CoordinationErrorKind::AwaitingAck,
            "a resolved result awaits",
        );
        let untyped =
            Error::InvalidArgument("coordination client: a resolved result awaits".into());
        assert_eq!(typed.to_string(), untyped.to_string());
    }
}
