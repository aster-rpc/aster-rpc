//! The mutation driver: the §10 client algorithm over the journal and the
//! engine (S4-D, S4-E).
//!
//! One function drives a pending journal entry to resolution; every typed
//! mutation method funnels into it. The algorithm is §10's, mechanically:
//!
//! 1. fsync the exact request as `Pending` before any network I/O;
//! 2. send **only** those bytes; a redirect, timeout, reset or malformed
//!    reply resends them and never substitutes;
//! 3. a typed reply resolves by `(disposition, reason)`:
//!    `Consumed` → fsync `ResolvedAwaitingAck` and deliver;
//!    terminal `NotConsumed` → the closing corridor;
//!    non-terminal `NotConsumed` → bounded backoff and resend;
//! 4. the closing corridor: retire this client id through ambiguity, then
//!    **replay the exact mutation** on the tombstone — the only call that can
//!    say whether the original committed — then deliver and rotate;
//! 5. an explicit `18` from retirement is the §10 proof that nothing can
//!    close the id: `FatalJournal`, surfaced, no guessing.
//!
//! A drive that exhausts its attempt budget returns with the journal still
//! `Pending`/`Closing` — the sequence stays open, nothing is abandoned, and
//! the next call (or [`resume`](super::CoordinationClient::resume)) continues
//! from the fsynced state, including across a process restart.

use std::time::Duration;

use crate::coordination::contract::payload_fory;
use crate::coordination::reason::{Disposition, Reason};
use crate::coordination::store::wire::{
    MutationReplyWireV1, RetireClientReplyWireV1, RetireClientRequestV1, STORE_PROTOCOL_VERSION,
};
use crate::error::{Error, Result};

use super::engine::{CallFailure, CallOutcome, Engine};
use super::journal::{AckToken, Journal, JournalState};

/// Attempts one drive makes before returning with the sequence still open.
const DRIVE_ATTEMPTS: usize = 8;

fn backoff(attempt: usize) -> Duration {
    Duration::from_millis(u64::min(250 * (1 << attempt.min(4)), 2_000))
}

/// What a resolved drive delivers.
#[derive(Debug)]
pub(crate) struct Delivery {
    /// The exact reply the journal fsynced.
    pub(crate) reply: MutationReplyWireV1,
    /// Whether the closing corridor produced it — in which case its
    /// acknowledgement is a **rotation** (§10), not an ordinary ack.
    pub(crate) via_retirement: bool,
    /// The token acknowledging exactly this entry must present.
    pub(crate) token: AckToken,
}

fn client_error(what: impl Into<String>) -> Error {
    Error::InvalidArgument(format!("coordination client: {}", what.into()))
}

/// §10 terminality, derived from `(disposition, reason)` and nothing else.
fn is_terminal_refusal(reply: &MutationReplyWireV1) -> bool {
    if reply.disposition_code != 2 {
        return false;
    }
    let Some(reason) = Reason::from_code(reply.reason_code) else {
        // An unknown reason is preserved for diagnostics but never
        // interpreted — and never treated as terminal, because stopping on a
        // retryable condition abandons a sequence that could still commit.
        return false;
    };
    Disposition::NotConsumed.is_terminal(reason)
}

/// Drive the journal's pending entry to resolution.
///
/// Requires the caller to hold the journal (the client serializes through one
/// `Mutex`), with the journal in `Pending`, `Closing`, or `ReplayAfterRetire`.
pub(crate) async fn drive(engine: &Engine, journal: &mut Journal) -> Result<Delivery> {
    loop {
        match journal.state() {
            JournalState::Pending => match drive_pending(engine, journal).await? {
                Some(delivery) => return Ok(delivery),
                // A terminal refusal moved the journal to Closing.
                None => continue,
            },
            JournalState::Closing => {
                retire_until_confirmed(engine, journal).await?;
            }
            JournalState::ReplayAfterRetire => {
                return replay_on_tombstone(engine, journal).await;
            }
            other => {
                return Err(client_error(format!(
                    "drive entered with the journal in {other:?}"
                )))
            }
        }
    }
}

/// Send the exact pending bytes until a typed reply resolves or the attempt
/// budget runs out. `Ok(None)` means a terminal refusal moved the journal to
/// `Closing`.
async fn drive_pending(engine: &Engine, journal: &mut Journal) -> Result<Option<Delivery>> {
    let method: String = journal.record().pending_method.clone();
    let bytes = journal.record().pending_request.clone();

    for attempt in 0..DRIVE_ATTEMPTS {
        journal.boundary("before_send")?;
        let outcome = engine.unary(&method, bytes.clone()).await;
        journal.boundary("after_send")?;

        match outcome {
            Ok(CallOutcome::Reply(payload)) => {
                let Some(reply) = decode_and_check(journal, &payload) else {
                    // A reply that does not echo this request is a malformed
                    // disposition: ambiguous, resend.
                    tokio::time::sleep(backoff(attempt)).await;
                    continue;
                };
                if reply.disposition_code == 1 {
                    let token = journal.resolve(payload)?;
                    return Ok(Some(Delivery {
                        reply,
                        via_retirement: false,
                        token,
                    }));
                }
                if is_terminal_refusal(&reply) {
                    // §10: this client id is finished. Closing is three steps,
                    // not one — retire, replay, rotate — and none is optional.
                    // The refusal itself rides along: if the replay proves the
                    // original never applied, this is the answer delivered.
                    journal.close_for_retirement(payload)?;
                    return Ok(None);
                }
                // Non-terminal NotConsumed: the exact same action is retried;
                // a client may never substitute at a sent sequence.
                tokio::time::sleep(backoff(attempt)).await;
            }
            Ok(CallOutcome::Denied { code, message }) => {
                // A status-only denial is non-terminal with an unknown reason
                // (§10): surface it, but the sequence stays open and the
                // journal stays Pending — the next drive resends the exact
                // bytes.
                return Err(Error::Rpc {
                    code,
                    message: format!(
                        "the mutation was refused pre-proposal and its sequence remains \
                         open; retry or resume: {message}"
                    ),
                    details: Vec::new(),
                });
            }
            Err(CallFailure::Ambiguous(_)) | Err(CallFailure::Unavailable(_)) => {
                tokio::time::sleep(backoff(attempt)).await;
            }
        }
    }
    Err(client_error(format!(
        "no resolution within {DRIVE_ATTEMPTS} attempts; the sequence remains open and \
         resumes with the same exact bytes"
    )))
}

/// The reply must be a **valid §5.2 answer to this journal's entry** — the
/// full request-aware verdict (`client::inbound`): entry identity, coherent
/// disposition/outcome/reason, the method's outcome, snapshot presence and
/// nested facts. The same validator the journal itself runs at `resolve`
/// and at reopen, applied here first so an invalid reply is classified as a
/// malformed disposition (ambiguous, resend) rather than reaching the
/// persist path at all.
fn decode_and_check(journal: &Journal, payload: &[u8]) -> Option<MutationReplyWireV1> {
    let reply: MutationReplyWireV1 = payload_fory::<MutationReplyWireV1>()
        .deserialize(payload)
        .ok()?;
    let record = journal.record();
    let cx = super::inbound::MutationContext {
        group_id: &record.group_id,
        local_actor: &record.actor_node_id,
        method: &record.pending_method,
        request: &record.pending_request,
        client_id: &record.client_id,
        sequence: record.sequence,
        request_hash: &record.request_hash,
    };
    super::inbound::validate_mutation_reply(&cx, &reply)
        .ok()
        .map(|()| reply)
}

/// `Closing`: retry sessionless retirement through ambiguity until a reply
/// confirms it — or an explicit `18` proves nothing ever can (§10).
async fn retire_until_confirmed(engine: &Engine, journal: &mut Journal) -> Result<()> {
    let actor = engine.local_id()?;
    let request = RetireClientRequestV1 {
        protocol_version: STORE_PROTOCOL_VERSION,
        group_id: engine.group.as_bytes().to_vec(),
        target_node_id: actor.to_vec(),
        target_client_id: journal.record().client_id.clone(),
    };
    let bytes = payload_fory::<RetireClientRequestV1>()
        .serialize(&request)
        .map_err(|_| client_error("the retirement request does not encode"))?;

    for attempt in 0..DRIVE_ATTEMPTS {
        journal.boundary("before_retire_send")?;
        let outcome = engine.unary("retire_client", bytes.clone()).await;
        journal.boundary("after_retire_send")?;
        match outcome {
            Ok(CallOutcome::Reply(payload)) => {
                let Ok(reply) = payload_fory::<RetireClientReplyWireV1>()
                    .deserialize::<RetireClientReplyWireV1>(&payload)
                else {
                    tokio::time::sleep(backoff(attempt)).await;
                    continue;
                };
                // A retirement confirmation must come from **this** group
                // and protocol: an outcome-9 reply naming another world
                // confirms nothing, and honoring it would replay the
                // original on a tombstone that may not exist.
                if reply.protocol_version != STORE_PROTOCOL_VERSION
                    || reply.group_id != engine.group.as_bytes().to_vec()
                {
                    tokio::time::sleep(backoff(attempt)).await;
                    continue;
                }
                if reply.outcome_code == 9 {
                    journal.retirement_confirmed()?;
                    return Ok(());
                }
                if reply.reason_code == Reason::RetainedSessionCapReached.code() {
                    // §10: an explicit 18 is a proof, not a failure to retry.
                    // The absent session cannot be created and neither can the
                    // delayed original's, so the original can never commit.
                    journal.fatal()?;
                    return Err(client_error(
                        "retained-session capacity is exhausted; this journal cannot be \
                         closed and no further mutation is possible (§10 reason 18)",
                    ));
                }
                tokio::time::sleep(backoff(attempt)).await;
            }
            Ok(CallOutcome::Denied { code, message }) => {
                // Self-retirement needs no grant; a denial here is a
                // deployment fault. Remain Closing — ambiguity discipline
                // still applies to whatever caused it.
                return Err(Error::Rpc {
                    code,
                    message: format!(
                        "retirement was refused; the journal remains Closing: {message}"
                    ),
                    details: Vec::new(),
                });
            }
            Err(_) => tokio::time::sleep(backoff(attempt)).await,
        }
    }
    Err(client_error(
        "retirement did not confirm within the attempt budget; the journal remains Closing",
    ))
}

/// `ReplayAfterRetire`: replay the exact original mutation on the tombstone.
///
/// Never `probe_client`: only the replay returns the saved *outcome*, and a
/// committed rejection and a committed success both read as "consumed" to a
/// probe (§10).
async fn replay_on_tombstone(engine: &Engine, journal: &mut Journal) -> Result<Delivery> {
    let method = journal.record().pending_method.clone();
    let bytes = journal.record().pending_request.clone();

    for attempt in 0..DRIVE_ATTEMPTS {
        journal.boundary("before_replay_send")?;
        let outcome = engine.unary(&method, bytes.clone()).await;
        journal.boundary("after_replay_send")?;
        match outcome {
            Ok(CallOutcome::Reply(payload)) => {
                let Some(reply) = decode_and_check(journal, &payload) else {
                    tokio::time::sleep(backoff(attempt)).await;
                    continue;
                };
                match (reply.disposition_code, reply.reason_code) {
                    // The saved reply: the original **committed**, and this is
                    // its exact result — possibly a committed rejection.
                    (1, _) => {
                        let token = journal.resolve(payload)?;
                        return Ok(Delivery {
                            reply,
                            via_retirement: true,
                            token,
                        });
                    }
                    // ClientRetired: retirement won, the original never
                    // applied and never will. The delivered answer is the
                    // terminal refusal that opened the corridor — the reason
                    // the group actually gave — not the tombstone's generic
                    // refusal.
                    (2, code) if code == Reason::ClientRetired.code() => {
                        let refusal = journal.record().terminal_refusal.clone();
                        let delivered: MutationReplyWireV1 = payload_fory::<MutationReplyWireV1>()
                            .deserialize(&refusal)
                            .map_err(|_| {
                                client_error("the stored terminal refusal does not decode")
                            })?;
                        let token = journal.resolve(refusal)?;
                        return Ok(Delivery {
                            reply: delivered,
                            via_retirement: true,
                            token,
                        });
                    }
                    // A tombstoned session answers nothing else; anything
                    // else is not this session's answer.
                    _ => tokio::time::sleep(backoff(attempt)).await,
                }
            }
            Ok(CallOutcome::Denied { code, message }) => {
                return Err(Error::Rpc {
                    code,
                    message: format!(
                        "the replay was refused; the journal remains ReplayAfterRetire: {message}"
                    ),
                    details: Vec::new(),
                });
            }
            Err(_) => tokio::time::sleep(backoff(attempt)).await,
        }
    }
    Err(client_error(
        "the replay did not resolve within the attempt budget; it resumes with the same bytes",
    ))
}
