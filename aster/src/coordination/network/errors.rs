//! `S3-NETERR`: the two exhaustive mappings from Aster transport and status
//! failures into OpenRaft's error variants (§5.4).
//!
//! ## Why a single "network error" bucket is wrong
//!
//! The variant Aster returns decides OpenRaft's retry behaviour, and the four
//! behaviours are genuinely different: `Network` retries **immediately**,
//! `Unreachable` invokes backoff, `PayloadTooLarge` asks append replication to
//! split, and `Timeout` has its own path. Collapsing them turns a permanent
//! protocol fault into a hot loop, or transient route loss into node failure.
//!
//! ## The OpenRaft column is narrow, and that is not the table being vacuous
//!
//! Almost every row maps to `Unreachable`. Read the two columns together: a
//! non-`Ok` status means the Raft-level exchange **did not happen**, and
//! OpenRaft's only "did not happen" variants are `Timeout`, `Unreachable` and
//! `Network`. `Network` retries immediately, which is wrong for every row —
//! each is either permanent or load-related. `RemoteError` is unreachable from
//! a status by construction: it must carry a well-formed remote `RaftError`,
//! which arrives in an `Ok` reply body, never as a status.
//!
//! The informative column is [`FaultClass`], which is what the health surface
//! reports. It is where a peer's storage failure and a peer's refusal of our
//! bytes stay different, though both back off identically.
//!
//! ## `PayloadTooLarge` is method-aware
//!
//! It is a *request to split*, and only append replication can split. OpenRaft
//! 0.9.25 already forbids the two wrong answers at the type level — the only
//! out-of-crate constructor, `PayloadTooLarge::new_entries_hint`, hard-codes
//! `RPCTypes::AppendEntries`, `new_bytes_hint` is `pub(crate)`, and there is no
//! `Vote` constructor at all. [`oversized_request`] records *why*, so nobody
//! adds a local constructor to "fix" the asymmetry: a `Vote`-actioned one
//! reaches `unreachable!` in both `Display` and `ReplicationCore::update_hint`,
//! which is a panic inside the replication task, and an `InstallSnapshot` one
//! is silently dropped so the same oversized request retries forever.
//!
//! An oversized `vote` request is a local invariant failure — this store built
//! a frame it cannot send — and it **still has to return an `RPCError`**,
//! because `RaftNetwork::vote` is typed to. Naming the diagnosis and leaving
//! the branch unwritten is not an answer. The return is `Unreachable` and the
//! invariant is latched: `PayloadTooLarge` panics, `Network` retries the
//! identical oversized frame immediately, `Timeout` claims something that did
//! not happen, and `RemoteError` needs a remote error no peer produced.
//! `Unreachable` is the only variant meaning "this did not happen, back off"
//! without asserting a falsehood.

use openraft::error::{NetworkError, RPCError, RaftError, Unreachable};
use openraft::network::RPCTypes;

use crate::coordination::ids::CoordinationNodeId;
use crate::coordination::typeconfig::{CoordinationNode, CoordinationTypeConfig};
use crate::rpc::StatusCode;

/// An `RPCError` for this type config, carrying a remote `RaftError<E>`.
pub(crate) type CoordinationRpcError<E = openraft::error::Infallible> =
    RPCError<CoordinationNodeId, CoordinationNode, RaftError<CoordinationNodeId, E>>;

/// A `StreamingError` for this type config.
pub(crate) type CoordinationStreamingError = openraft::error::StreamingError<
    CoordinationTypeConfig,
    openraft::error::Fatal<CoordinationNodeId>,
>;

/// What the health surface learns from a failed exchange.
///
/// This is the column that actually discriminates. Two failures that both back
/// off can still be a peer that refused our bytes and a peer whose own disk
/// failed, and an operator needs to tell them apart — that is the distinction
/// `PrepareInstallError` carries on the receiving side, preserved across the
/// wire rather than lost at it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum FaultClass {
    /// Expected to clear on its own: a route flapped, a peer is restarting, a
    /// call was abandoned.
    Transient,
    /// The peer is up and refusing work because it is overloaded. Backoff is
    /// the correct response; an immediate retry is what exhausted it.
    PeerLoad,
    /// The peer's own failure — its storage, its invariants.
    PeerFault,
    /// The peer refused **our** bytes as bad. The same body will be refused
    /// again, so this is latched against that body rather than retried blind.
    PeerRefusal,
    /// Repairable only by an operator: a revoked admission, a sender outside
    /// the allowlist.
    PermanentUntilRepaired,
    /// Not repairable by retry at all: a version mismatch, a wrong group, a
    /// decode failure, an unauthenticated peer. Latched and surfaced, because
    /// OpenRaft has no permanent-fault variant to carry it.
    PermanentProtocol,
    /// **Our** invariant broke. The peer is not at fault and may be perfectly
    /// healthy.
    LocalInvariant,
}

impl FaultClass {
    /// Whether this class should be latched and surfaced to an operator rather
    /// than counted as ordinary churn.
    ///
    /// Backoff handles the transient and load classes; nothing handles the
    /// rest, which is exactly why they need to be visible.
    pub(crate) fn is_latched(self) -> bool {
        match self {
            FaultClass::Transient | FaultClass::PeerLoad => false,
            FaultClass::PeerFault
            | FaultClass::PeerRefusal
            | FaultClass::PermanentUntilRepaired
            | FaultClass::PermanentProtocol
            | FaultClass::LocalInvariant => true,
        }
    }
}

/// Which OpenRaft retry behaviour a failure selects.
///
/// Named rather than constructed directly so both mapping tables can be written
/// as total functions returning a value that is easy to assert on. The
/// conversion into a real `RPCError` or `StreamingError` happens at the call
/// site, which is the only place that has the peer id and the source error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Retry {
    /// Back off. The peer may return, or an operator may repair it.
    Unreachable,
    /// Retry immediately: a local I/O error on an open connection, which is
    /// the only case worth an immediate retry.
    Network,
    /// OpenRaft's own timeout path.
    Timeout,
}

/// One classified failure: what OpenRaft should do, and what an operator should
/// see.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Classified {
    pub(crate) retry: Retry,
    pub(crate) fault: FaultClass,
}

/// The Aster-side failures a Raft call can end in.
///
/// This is the **domain** of the mapping. An earlier revision of `S3-NETERR`
/// enumerated the OpenRaft variants and stopped there, which names the codomain
/// and never the domain — so it could not be checked for exhaustiveness in the
/// direction that matters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Transport {
    /// Connect failed; no route; the endpoint is not dialable.
    NotDialable,
    /// The connection was reset or closed mid-call.
    ///
    /// `Unreachable`, not `Network`: an immediate retry into a reset connection
    /// re-dials the same broken path.
    ConnectionLost,
    /// A local send/receive I/O error on an open stream.
    LocalIo,
    /// The deadline elapsed awaiting the reply.
    Deadline,
    /// The remote answered with a status.
    Status(StatusCode),
    /// The envelope's protocol or codec version is not ours.
    VersionMismatch,
    /// The envelope names another group.
    WrongGroup,
    /// The payload would not decode, or its message type does not match the
    /// method it arrived on.
    ///
    /// Never `PayloadTooLarge`: a decode failure is not a request to split.
    Undecodable,
    /// An inbound frame exceeded the negotiated cap.
    ///
    /// A peer's protocol fault, and it follows the version-mismatch row rather
    /// than becoming `PayloadTooLarge` — which would ask the peer to split
    /// something it may not be able to split.
    InboundOversized,
    /// **We** built a frame we cannot send, on a method that cannot split.
    OutboundOversized,
}

/// The unary mapping. Total over [`Transport`], with no wildcard.
pub(crate) fn classify(failure: &Transport) -> Classified {
    let (retry, fault) = match failure {
        Transport::NotDialable => (Retry::Unreachable, FaultClass::Transient),
        Transport::ConnectionLost => (Retry::Unreachable, FaultClass::Transient),
        Transport::LocalIo => (Retry::Network, FaultClass::Transient),
        Transport::Deadline => (Retry::Timeout, FaultClass::Transient),
        Transport::Status(code) => return classify_status(*code),
        Transport::VersionMismatch => (Retry::Unreachable, FaultClass::PermanentProtocol),
        Transport::WrongGroup => (Retry::Unreachable, FaultClass::PermanentProtocol),
        Transport::Undecodable => (Retry::Unreachable, FaultClass::PermanentProtocol),
        Transport::InboundOversized => (Retry::Unreachable, FaultClass::PermanentProtocol),
        Transport::OutboundOversized => (Retry::Unreachable, FaultClass::LocalInvariant),
    };
    Classified { retry, fault }
}

/// The remote-status mapping. Total over all eighteen `StatusCode` variants,
/// with no wildcard.
///
/// `StatusCode::from_i32` funnels every unrecognised code into `Unknown`, so
/// there is no nineteenth case: an exhaustive `match` here is exhaustive over
/// the wire, not merely over the enum.
pub(crate) fn classify_status(code: StatusCode) -> Classified {
    let (retry, fault) = match code {
        // Not an error. Reached only if a caller classifies a successful reply,
        // which is a caller bug rather than a transport outcome — a reply body
        // that fails to decode is `Undecodable`, not a status.
        StatusCode::Ok => (Retry::Unreachable, FaultClass::PermanentProtocol),
        StatusCode::Cancelled => (Retry::Unreachable, FaultClass::Transient),
        // Also the landing place for any code this build does not recognise.
        StatusCode::Unknown => (Retry::Unreachable, FaultClass::PermanentProtocol),
        // The peer rejected what we sent as malformed; resending cannot help.
        StatusCode::InvalidArgument => (Retry::Unreachable, FaultClass::PermanentProtocol),
        // The only row that is not `Unreachable`.
        StatusCode::DeadlineExceeded => (Retry::Timeout, FaultClass::Transient),
        StatusCode::NotFound => (Retry::Unreachable, FaultClass::Transient),
        // Not produced by the Raft service; receiving it means we are not
        // talking to what we think we are.
        StatusCode::AlreadyExists => (Retry::Unreachable, FaultClass::PermanentProtocol),
        StatusCode::PermissionDenied => (Retry::Unreachable, FaultClass::PermanentUntilRepaired),
        StatusCode::ResourceExhausted => (Retry::Unreachable, FaultClass::PeerLoad),
        // The peer is up but not in a state to serve Raft.
        StatusCode::FailedPrecondition => (Retry::Unreachable, FaultClass::Transient),
        StatusCode::Aborted => (Retry::Unreachable, FaultClass::PermanentProtocol),
        StatusCode::OutOfRange => (Retry::Unreachable, FaultClass::PermanentProtocol),
        // Ordinary during a rolling start, before the service registers.
        StatusCode::Unimplemented => (Retry::Unreachable, FaultClass::Transient),
        // The peer's own failure, including its `PrepareInstallError::Storage`.
        StatusCode::Internal => (Retry::Unreachable, FaultClass::PeerFault),
        StatusCode::Unavailable => (Retry::Unreachable, FaultClass::Transient),
        // Our bytes were refused as corrupt. The same body is refused again.
        StatusCode::DataLoss => (Retry::Unreachable, FaultClass::PeerRefusal),
        StatusCode::Unauthenticated => (Retry::Unreachable, FaultClass::PermanentProtocol),
        StatusCode::ContractViolation => (Retry::Unreachable, FaultClass::PermanentProtocol),
    };
    Classified { retry, fault }
}

/// What an oversized **outbound** request becomes, by method.
///
/// The whole method-awareness rule in one place, returning `None` for the one
/// method that can split so the caller must construct the hint itself with the
/// real entry count.
///
/// `append_entries` is `None` — the caller builds
/// `PayloadTooLarge::new_entries_hint(n)` with `n >= 1`, and the hint must
/// strictly decrease the next request or replication retries the same frame
/// forever. `vote` and `install_snapshot` are `Some`, and both are
/// [`FaultClass::LocalInvariant`]: nothing about the peer is wrong.
pub(crate) fn oversized_request(method: RPCTypes) -> Option<Classified> {
    match method {
        // Can split. The caller supplies the hint.
        RPCTypes::AppendEntries => None,
        // Fixed-shape and unsplittable. A `Vote`-actioned `PayloadTooLarge`
        // reaches `unreachable!` in OpenRaft's own `Display` and
        // `update_hint`, so this must never become one.
        RPCTypes::Vote => Some(Classified {
            retry: Retry::Unreachable,
            fault: FaultClass::LocalInvariant,
        }),
        // OpenRaft drops an `InstallSnapshot` hint with a log line, so the same
        // oversized request would retry unchanged forever. Chunk sizing is ours
        // to get right before sending.
        RPCTypes::InstallSnapshot => Some(Classified {
            retry: Retry::Unreachable,
            fault: FaultClass::LocalInvariant,
        }),
    }
}

/// The snapshot-streaming mapping.
///
/// The same domain as [`classify`] plus two cases only a stream has, and
/// **without** `PayloadTooLarge`: `StreamingError` has no such variant, chunk
/// sizing is ours to get right before sending, and a too-large chunk is a local
/// invariant failure rather than a transport outcome to map.
///
/// OpenRaft itself converts a snapshot-stream remote fatal into `Unreachable`,
/// hence backoff. Do not invent a terminal remote-fatal bucket to "improve" on
/// that.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StreamFailure {
    /// We intentionally stopped sending. Not an error condition.
    Closed,
    /// Local storage failed while reading the body. Local fatal; ends this
    /// node's stream.
    LocalStorage,
    /// Every unary failure, which classifies identically.
    Transport(Transport),
}

/// What a stream failure becomes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StreamRetry {
    Closed,
    StorageError,
    Timeout,
    Unreachable,
    Network,
}

/// The streaming mapping. Total over [`StreamFailure`], with no wildcard.
pub(crate) fn classify_stream(failure: &StreamFailure) -> (StreamRetry, FaultClass) {
    match failure {
        StreamFailure::Closed => (StreamRetry::Closed, FaultClass::Transient),
        StreamFailure::LocalStorage => (StreamRetry::StorageError, FaultClass::LocalInvariant),
        StreamFailure::Transport(t) => {
            let Classified { retry, fault } = classify(t);
            let retry = match retry {
                Retry::Unreachable => StreamRetry::Unreachable,
                Retry::Network => StreamRetry::Network,
                Retry::Timeout => StreamRetry::Timeout,
            };
            (retry, fault)
        }
    }
}

/// Build the `RPCError` a classified unary failure returns.
///
/// `Timeout` needs an id, a target and a deadline that only the call site has,
/// so it is not constructed here; [`Retry::Timeout`] callers build it
/// themselves. Everything else is expressible from the classification and a
/// message.
pub(crate) fn to_rpc_error<E>(classified: Classified, detail: String) -> CoordinationRpcError<E>
where
    E: std::error::Error,
{
    match classified.retry {
        Retry::Unreachable => RPCError::Unreachable(Unreachable::new(&IoLike(detail))),
        Retry::Network => RPCError::Network(NetworkError::new(&IoLike(detail))),
        // A timeout carries a deadline this function does not have. Mapping it
        // to `Unreachable` here would be a quiet downgrade, so the call site
        // constructs it; reaching this arm means a caller ignored that.
        Retry::Timeout => RPCError::Unreachable(Unreachable::new(&IoLike(format!(
            "{detail} (timeout reported without a deadline)"
        )))),
    }
}

/// A minimal `Error` wrapper, because OpenRaft's `Unreachable::new` and
/// `NetworkError::new` take `&impl Error` rather than a string.
#[derive(Debug)]
struct IoLike(String);

impl std::fmt::Display for IoLike {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for IoLike {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `StatusCode` this build knows, so the table test cannot silently
    /// cover fewer than all of them.
    ///
    /// Written out rather than derived, and cross-checked against `from_i32`
    /// below: the enum has no `ALL`, so a list is the only way to iterate it,
    /// and an unchecked list is exactly how a mapping ends up covering four of
    /// eighteen.
    const ALL_STATUS: &[StatusCode] = &[
        StatusCode::Ok,
        StatusCode::Cancelled,
        StatusCode::Unknown,
        StatusCode::InvalidArgument,
        StatusCode::DeadlineExceeded,
        StatusCode::NotFound,
        StatusCode::AlreadyExists,
        StatusCode::PermissionDenied,
        StatusCode::ResourceExhausted,
        StatusCode::FailedPrecondition,
        StatusCode::Aborted,
        StatusCode::OutOfRange,
        StatusCode::Unimplemented,
        StatusCode::Internal,
        StatusCode::Unavailable,
        StatusCode::DataLoss,
        StatusCode::Unauthenticated,
        StatusCode::ContractViolation,
    ];

    /// Every non-status transport failure, so a table can cover all of them.
    ///
    /// `Transport::Status` is excluded because its own eighteen cases are the
    /// separate table above; including it here would need a representative
    /// status and would make "every transport failure" quietly mean "every one
    /// except the branching one".
    const ALL_TRANSPORT: &[Transport] = &[
        Transport::NotDialable,
        Transport::ConnectionLost,
        Transport::LocalIo,
        Transport::Deadline,
        Transport::VersionMismatch,
        Transport::WrongGroup,
        Transport::Undecodable,
        Transport::InboundOversized,
        Transport::OutboundOversized,
    ];

    /// The list is all eighteen and nothing else.
    ///
    /// The oracle is `from_i32`, which is the wire's own view: every code it
    /// can produce must be in the list, and every code in the list must round
    /// trip. That is what makes the table below exhaustive **over the wire**
    /// rather than merely over an enum someone remembered to extend.
    #[test]
    fn the_status_list_is_every_code_the_wire_can_produce() {
        assert_eq!(ALL_STATUS.len(), 18, "§5.4 declares eighteen statuses");

        for code in ALL_STATUS {
            assert_eq!(
                StatusCode::from_i32(code.as_i32()),
                *code,
                "{code:?} must round trip"
            );
        }

        // And every reachable wire value lands on one of them. 0..=200 covers
        // the declared range, the reserved gap, and the Aster-native code.
        for raw in 0..=200i32 {
            let mapped = StatusCode::from_i32(raw);
            assert!(
                ALL_STATUS.contains(&mapped),
                "wire code {raw} maps to {mapped:?}, which the table does not cover"
            );
        }
    }

    /// The eighteen-row table, asserted in **both** columns.
    ///
    /// The expectations are written here independently of the implementation,
    /// so this is a real oracle rather than a restatement. The mutations
    /// `S3-NETERR` names — mapping `ResourceExhausted` to `Network`, answering
    /// `Storage` as `DataLoss` — each fail exactly one row.
    #[test]
    fn every_remote_status_maps_to_its_declared_variant_and_fault_class() {
        let table: &[(StatusCode, Retry, FaultClass)] = &[
            (
                StatusCode::Ok,
                Retry::Unreachable,
                FaultClass::PermanentProtocol,
            ),
            (
                StatusCode::Cancelled,
                Retry::Unreachable,
                FaultClass::Transient,
            ),
            (
                StatusCode::Unknown,
                Retry::Unreachable,
                FaultClass::PermanentProtocol,
            ),
            (
                StatusCode::InvalidArgument,
                Retry::Unreachable,
                FaultClass::PermanentProtocol,
            ),
            (
                StatusCode::DeadlineExceeded,
                Retry::Timeout,
                FaultClass::Transient,
            ),
            (
                StatusCode::NotFound,
                Retry::Unreachable,
                FaultClass::Transient,
            ),
            (
                StatusCode::AlreadyExists,
                Retry::Unreachable,
                FaultClass::PermanentProtocol,
            ),
            (
                StatusCode::PermissionDenied,
                Retry::Unreachable,
                FaultClass::PermanentUntilRepaired,
            ),
            (
                StatusCode::ResourceExhausted,
                Retry::Unreachable,
                FaultClass::PeerLoad,
            ),
            (
                StatusCode::FailedPrecondition,
                Retry::Unreachable,
                FaultClass::Transient,
            ),
            (
                StatusCode::Aborted,
                Retry::Unreachable,
                FaultClass::PermanentProtocol,
            ),
            (
                StatusCode::OutOfRange,
                Retry::Unreachable,
                FaultClass::PermanentProtocol,
            ),
            (
                StatusCode::Unimplemented,
                Retry::Unreachable,
                FaultClass::Transient,
            ),
            (
                StatusCode::Internal,
                Retry::Unreachable,
                FaultClass::PeerFault,
            ),
            (
                StatusCode::Unavailable,
                Retry::Unreachable,
                FaultClass::Transient,
            ),
            (
                StatusCode::DataLoss,
                Retry::Unreachable,
                FaultClass::PeerRefusal,
            ),
            (
                StatusCode::Unauthenticated,
                Retry::Unreachable,
                FaultClass::PermanentProtocol,
            ),
            (
                StatusCode::ContractViolation,
                Retry::Unreachable,
                FaultClass::PermanentProtocol,
            ),
        ];

        assert_eq!(
            table.len(),
            ALL_STATUS.len(),
            "the table must cover every status, not a subset"
        );
        for (code, retry, fault) in table {
            let got = classify_status(*code);
            assert_eq!(
                got,
                Classified {
                    retry: *retry,
                    fault: *fault
                },
                "{code:?}"
            );
        }

        // Every declared status appears exactly once.
        let mut seen: Vec<i32> = table.iter().map(|(c, _, _)| c.as_i32()).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), table.len(), "a status is listed twice");
    }

    /// `DeadlineExceeded` is the only status that is not `Unreachable`.
    ///
    /// Stated as a property rather than left implicit in the table: it is the
    /// single exception to the "the exchange did not happen, back off" reading,
    /// and a change that made a second row `Network` would otherwise pass
    /// unnoticed among eighteen rows.
    #[test]
    fn deadline_exceeded_is_the_only_status_that_is_not_unreachable() {
        for code in ALL_STATUS {
            let retry = classify_status(*code).retry;
            if *code == StatusCode::DeadlineExceeded {
                assert_eq!(retry, Retry::Timeout);
            } else {
                assert_eq!(retry, Retry::Unreachable, "{code:?}");
            }
        }
        assert!(
            !ALL_STATUS
                .iter()
                .any(|c| classify_status(*c).retry == Retry::Network),
            "no status may retry immediately: each is permanent or load-related"
        );
    }

    /// A peer's storage failure and a peer's refusal of our bytes back off
    /// identically and stay **different** in the fault class.
    ///
    /// This is the outbound `PrepareInstallError` distinction surviving the
    /// wire. If it collapsed, an operator could not tell "their disk failed"
    /// from "they rejected my snapshot".
    #[test]
    fn a_peers_storage_failure_and_its_refusal_of_our_bytes_stay_distinguishable() {
        let storage = classify_status(StatusCode::Internal);
        let refusal = classify_status(StatusCode::DataLoss);
        let bounds = classify_status(StatusCode::InvalidArgument);

        assert_eq!(storage.retry, refusal.retry, "both back off");
        assert_ne!(storage.fault, refusal.fault, "and stay distinguishable");
        assert_eq!(storage.fault, FaultClass::PeerFault);
        assert_eq!(refusal.fault, FaultClass::PeerRefusal);
        assert_eq!(bounds.fault, FaultClass::PermanentProtocol);
    }

    /// The unary transport table, in both columns.
    #[test]
    fn every_transport_failure_maps_to_its_declared_variant_and_fault_class() {
        let table: &[(Transport, Retry, FaultClass)] = &[
            (
                Transport::NotDialable,
                Retry::Unreachable,
                FaultClass::Transient,
            ),
            (
                Transport::ConnectionLost,
                Retry::Unreachable,
                FaultClass::Transient,
            ),
            (Transport::LocalIo, Retry::Network, FaultClass::Transient),
            (Transport::Deadline, Retry::Timeout, FaultClass::Transient),
            (
                Transport::VersionMismatch,
                Retry::Unreachable,
                FaultClass::PermanentProtocol,
            ),
            (
                Transport::WrongGroup,
                Retry::Unreachable,
                FaultClass::PermanentProtocol,
            ),
            (
                Transport::Undecodable,
                Retry::Unreachable,
                FaultClass::PermanentProtocol,
            ),
            (
                Transport::InboundOversized,
                Retry::Unreachable,
                FaultClass::PermanentProtocol,
            ),
            (
                Transport::OutboundOversized,
                Retry::Unreachable,
                FaultClass::LocalInvariant,
            ),
        ];
        for (failure, retry, fault) in table {
            assert_eq!(
                classify(failure),
                Classified {
                    retry: *retry,
                    fault: *fault
                },
                "{failure:?}"
            );
        }
    }

    /// A reset connection backs off rather than retrying immediately.
    ///
    /// The mutation `S3-NETERR` names as swapping an immediate/backoff
    /// mapping: an immediate retry into a reset connection re-dials the same
    /// broken path. Only a *local* I/O error on an open stream is worth an
    /// immediate retry.
    #[test]
    fn only_a_local_io_error_retries_immediately() {
        assert_eq!(
            classify(&Transport::ConnectionLost).retry,
            Retry::Unreachable
        );
        assert_eq!(classify(&Transport::LocalIo).retry, Retry::Network);

        let immediate: Vec<&Transport> = ALL_TRANSPORT
            .iter()
            .filter(|t| classify(t).retry == Retry::Network)
            .collect();
        assert_eq!(
            immediate,
            vec![&Transport::LocalIo],
            "exactly one failure retries immediately"
        );
    }

    /// The method-awareness rule, as a property of all three methods.
    ///
    /// `append_entries` returns `None` because only it can split; the other two
    /// return a classification and never a `PayloadTooLarge`. The mutation
    /// `S3-NETERR` names — returning `new_entries_hint(1)` from `vote` — is
    /// prevented here by there being no path from `vote` to a hint at all.
    #[test]
    fn only_append_entries_may_be_asked_to_split() {
        assert_eq!(
            oversized_request(RPCTypes::AppendEntries),
            None,
            "append can split, so the caller supplies the hint"
        );

        for method in [RPCTypes::Vote, RPCTypes::InstallSnapshot] {
            let classified = oversized_request(method).expect("must classify");
            assert_eq!(classified.retry, Retry::Unreachable, "{method:?}");
            assert_eq!(
                classified.fault,
                FaultClass::LocalInvariant,
                "{method:?}: the peer is not at fault"
            );
        }
    }

    /// An oversized frame we built and an oversized frame a peer sent are
    /// different faults, though both back off.
    #[test]
    fn an_oversized_frame_is_ours_or_theirs_and_the_class_says_which() {
        assert_eq!(
            classify(&Transport::OutboundOversized).fault,
            FaultClass::LocalInvariant
        );
        assert_eq!(
            classify(&Transport::InboundOversized).fault,
            FaultClass::PermanentProtocol
        );
    }

    /// The streaming table, including the two cases only a stream has, and the
    /// property that it inherits every transport row unchanged.
    #[test]
    fn the_streaming_table_adds_two_cases_and_inherits_the_rest() {
        assert_eq!(
            classify_stream(&StreamFailure::Closed),
            (StreamRetry::Closed, FaultClass::Transient)
        );
        assert_eq!(
            classify_stream(&StreamFailure::LocalStorage),
            (StreamRetry::StorageError, FaultClass::LocalInvariant)
        );

        for transport in [
            Transport::NotDialable,
            Transport::ConnectionLost,
            Transport::LocalIo,
            Transport::Deadline,
            Transport::VersionMismatch,
            Transport::WrongGroup,
            Transport::Undecodable,
            Transport::Status(StatusCode::DataLoss),
            Transport::Status(StatusCode::Internal),
        ] {
            let unary = classify(&transport);
            let (stream_retry, stream_fault) =
                classify_stream(&StreamFailure::Transport(transport.clone()));
            assert_eq!(stream_fault, unary.fault, "{transport:?}");
            let expected = match unary.retry {
                Retry::Unreachable => StreamRetry::Unreachable,
                Retry::Network => StreamRetry::Network,
                Retry::Timeout => StreamRetry::Timeout,
            };
            assert_eq!(stream_retry, expected, "{transport:?}");
        }
    }

    /// A too-large chunk never becomes a transport outcome: `StreamingError`
    /// has no `PayloadTooLarge`, and asking a peer to split a chunk we sized
    /// wrong would be asking it to fix our bug.
    #[test]
    fn a_stream_has_no_way_to_ask_for_a_split() {
        let (retry, fault) =
            classify_stream(&StreamFailure::Transport(Transport::OutboundOversized));
        assert_eq!(retry, StreamRetry::Unreachable);
        assert_eq!(fault, FaultClass::LocalInvariant);
    }

    /// Which classes are latched. Backoff already handles the two that are not.
    #[test]
    fn only_the_classes_backoff_cannot_fix_are_latched() {
        assert!(!FaultClass::Transient.is_latched());
        assert!(!FaultClass::PeerLoad.is_latched());
        for latched in [
            FaultClass::PeerFault,
            FaultClass::PeerRefusal,
            FaultClass::PermanentUntilRepaired,
            FaultClass::PermanentProtocol,
            FaultClass::LocalInvariant,
        ] {
            assert!(latched.is_latched(), "{latched:?}");
        }
    }

    /// The constructed `RPCError` carries the retry behaviour the
    /// classification chose, and the detail survives into its message.
    #[test]
    fn the_constructed_error_matches_its_classification() {
        let unreachable: CoordinationRpcError =
            to_rpc_error(classify(&Transport::NotDialable), "no route".into());
        assert!(matches!(unreachable, RPCError::Unreachable(_)));
        assert!(unreachable.to_string().contains("no route"));

        let network: CoordinationRpcError =
            to_rpc_error(classify(&Transport::LocalIo), "write failed".into());
        assert!(matches!(network, RPCError::Network(_)));
        assert!(network.to_string().contains("write failed"));
    }
}
