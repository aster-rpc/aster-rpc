//! Preflight projection (§10 stages 2-4) and the committed apply path.
//!
//! One decision, two answers. [`preflight`] and [`apply_client_command`] call
//! the same `decide` functions; what differs is the **projection** -- whether a
//! refusal is denied before proposal or admitted so it reaches the log and
//! saves a result. A model that projects every refusal the same way is wrong in
//! one direction or the other: it either wedges the client on a conflict, or
//! burns a sequence for a condition that never reached the log.
//!
//! ## The forced-apply rule
//!
//! §10 stage 4 describes **conforming leader admission only**. A conforming
//! leader refuses a request-shape violation pre-proposal with an `RpcStatus`,
//! and nothing is written. A non-conforming leader can propose anything, and
//! apply may not skip a committed entry -- skipping diverges the voters, which
//! is the failure Raft exists to prevent. Halting instead would be worse: it
//! would hand a faulty leader an availability kill switch. Unknown commands are
//! fatal because voters cannot safely *interpret* them; a known bounds violation
//! is fully understood and can be rejected deterministically.
//!
//! So apply is total, and a committed request-shape violation commits:
//!
//! ```text
//! Consumed + 100 Rejected + 0 None
//! ```
//!
//! * **`Consumed`** because the entry committed. The result is saved and
//!   replayed exactly, like any other consumed sequence.
//! * **Reason `0`** truthfully: the frozen §5.2 reason space has no reason for
//!   this failure. Outcome `100` still identifies it as a rejection.
//!
//! Four invariants make that narrow rather than a loophole, and each has a test:
//!
//! 1. Forced apply mutates **no business state** -- no resource, ACL, admin,
//!    intent or route row -- but persists its saved reply, audit row, session
//!    sequence, applicable budget counters and `last_applied` atomically. That
//!    metadata is what consumes the sequence.
//! 2. `(Rejected, None)` is legal **only** for this case. Every other rejection
//!    carries a defined, nonzero coordination reason.
//! 3. A replay returns the saved typed reply, never an `RpcStatus`.
//! 4. Every logical request-shape limit is covered, not merely the obvious two.
//!    See [`ShapeLimit`](super::shape::ShapeLimit).
//!
//! ## Two kinds of replay, and one thing that is not a replay at all
//!
//! 1. **A preflight replay.** No entry was committed, so there is no audit row
//!    and `last_applied` does not move. [`preflight`] writes nothing at all.
//! 2. **A duplicate that was itself proposed and committed, at a new and higher
//!    log index.** This happens normally -- two copies of a request preflight
//!    before either applies, so both reach the log -- and needs no faulty
//!    leader. It has no business or session effects, but it **is** an applied
//!    entry: it writes its audit row and advances `last_applied`, or the entry
//!    would be indistinguishable from one that was skipped.
//! 3. **Delivery at or below `last_applied` is fatal, not a replay.** §11.3
//!    writes effects, audit and `last_applied` in one redb transaction, so
//!    "audit durable, `last_applied` not" cannot happen: either the transaction
//!    failed and none of it exists, or it committed and OpenRaft resumes from
//!    `already_committed.next_index()` and never delivers it again. A delivery
//!    that violates this is a storage or runtime invariant failure.
//!
//! ## Why the audit row cannot be the oracle for (3)
//!
//! An earlier revision compared audit rows to detect reapplication. That is
//! wrong twice over. Audit rows are **prunable**, absent for non-command
//! entries, and a **lossy projection** of the command: an `AuditRowV1` carries
//! actor, method, resource and outcome but no value, so two different
//! successful `compare_and_set` commands at one log id produce byte-identical
//! rows -- and the second value mutation would survive silently. Equality of a
//! lossy projection cannot establish command identity.
//!
//! The guard is therefore `last_applied`, checked **before any mutation**. The
//! earlier version decided and mutated first and compared audit afterwards, so
//! a command that then faulted had already changed business state.

use crate::coordination::reason::{Disposition, Outcome, Reason, RefusalReason};

use super::admin::{self, AdminAction};
use super::classify::{classify, Classification};
use super::expire::{self, ExpireCommand};
use super::grant::{self, GrantAction};
use super::limits;
use super::membership::{
    self, CancelMembershipAction, ChangeMembershipAction, MembershipProgressCommand,
};
use super::resource::{self, Decision, ResourceAction};
use super::retire::{self, RetireCommand};
use super::shape::ShapeLimit;
use super::state::{
    key_parts, AuditResult, AuditRow, ChangeSet, ClientId, LogId, NodeId, OperationId, ResourceKey,
    ResourceRow, SavedReply, SavedSnapshot, Session, State,
};

/// The `(actor, client_id, sequence, request_hash)` mutation prefix every
/// client command carries, mirroring the wire type rather than passing four
/// loose arguments.
///
/// `actor` is never a request field: it is derived from the immutable Iroh
/// transport peer (§5.2). A request field that looks like a calling actor is a
/// contract error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientRef {
    pub actor: NodeId,
    pub client_id: ClientId,
    pub sequence: u64,
    pub request_hash: [u8; 32],
}

/// A client action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Resource(ResourceAction),
    Grant(GrantAction),
    Admin(AdminAction),
    ChangeMembership(ChangeMembershipAction),
    CancelMembership(CancelMembershipAction),
}

/// The decision for whichever action was given.
///
/// One value rather than two code paths: preflight projects it and apply writes
/// it, so neither can drift from the other's idea of what the command means.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionDecision {
    Resource(Decision),
    Grant(grant::Decision),
    Admin(admin::Decision),
    Membership(membership::Decision),
}

impl Action {
    /// The §5.2 method code, used as the audit operation code.
    pub fn operation_code(&self) -> i32 {
        match self {
            Action::Resource(r) => match r {
                ResourceAction::Acquire { .. } => 1,
                ResourceAction::Renew { .. } => 2,
                ResourceAction::CompareAndSet { .. } => 3,
                ResourceAction::Release { .. } => 4,
                ResourceAction::ReleaseWithValue { .. } => 5,
                ResourceAction::Revoke { .. } => 6,
            },
            Action::Grant(_) => 7,
            Action::Admin(_) => 8,
            Action::ChangeMembership(_) => 9,
            // §5.2 skips 10; cancel is 11.
            Action::CancelMembership(_) => 11,
        }
    }

    /// The validated identity, or `None` when the request's identity is not
    /// well-formed.
    ///
    /// `None` is what keeps an unvalidated key out of bounded metadata: a saved
    /// reply and an audit row for a shapeless request carry no resource at all,
    /// rather than a copy of the malformed one. An oversized key stored in a
    /// session row would blow the 832-byte reserve charge the whole budget
    /// proof rests on, and would produce a row later validation rejects.
    /// The membership operation this action names, if any.
    fn operation_id(&self) -> Option<OperationId> {
        match self {
            Action::ChangeMembership(m) => Some(m.operation_id),
            Action::CancelMembership(m) => Some(m.operation_id),
            _ => None,
        }
    }

    fn validated_resource(&self) -> Option<ResourceKey> {
        match self {
            Action::Resource(r) => r.raw_resource().validate().ok(),
            Action::Grant(g) => g.resource.validate().ok(),
            // A `set_admin` names no resource: it changes group-level
            // authority. Storing one would put an identity in the audit row
            // that the command never touched.
            Action::Admin(_) | Action::ChangeMembership(_) | Action::CancelMembership(_) => None,
        }
    }

    fn decide(&self, state: &State, actor: &NodeId, request_hash: &[u8; 32]) -> ActionDecision {
        match self {
            Action::Resource(r) => ActionDecision::Resource(resource::decide(state, actor, r)),
            Action::Grant(g) => ActionDecision::Grant(grant::decide(state, actor, g)),
            Action::Admin(a) => ActionDecision::Admin(admin::decide(state, actor, a)),
            Action::ChangeMembership(m) => {
                ActionDecision::Membership(membership::decide_change(state, actor, request_hash, m))
            }
            Action::CancelMembership(m) => {
                ActionDecision::Membership(membership::decide_cancel(state, actor, m))
            }
        }
    }
}

/// What a leader answers before proposing (§10 stages 2-4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Admission {
    /// Stage 2: the classifier resolved the sequence. Answered as a typed reply
    /// whatever the row -- including the non-terminal ones.
    Resolved(Box<ClientResult>),
    /// Stage 3: a reason-coded admission denial. Typed reply, `NotConsumed`,
    /// nothing written.
    ///
    /// A [`RefusalReason`], so a denial cannot be built carrying `None` or a
    /// reserved code.
    Deny(RefusalReason),
    /// Stage 4: a denial with no §5.2 reason code. An `RpcStatus`, never a
    /// typed reply -- there is no code to put in one.
    Status(StatusDenial),
    /// Propose it.
    Admit,
}

/// A stage-4 denial: no §5.2 reason code exists for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusDenial {
    /// The actor lacks the required right. At apply the same condition is
    /// `12 PermissionChanged`; pre-proposal §10 gives it no reason code.
    NotAuthorized,
    /// A request-shape bound.
    Shape(ShapeLimit),
}

/// A condition meaning the log or local state is inconsistent.
///
/// Not a rejection: there is no client answer for it. The runtime moves to
/// fatal rather than continuing, because continuing would mean writing state
/// that disagrees with a peer's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fault {
    /// An entry was delivered at or below `last_applied`.
    ///
    /// A committed Raft history has exactly one entry per index, and §11.3
    /// makes an applied entry's effects, audit row and `last_applied` one
    /// transaction -- so an already-applied entry cannot be delivered again,
    /// and an older index cannot arrive after a newer one. This covers the
    /// same index under a different term too: that is a *different* entry
    /// claiming a position this node has already applied, which means the log
    /// this node applied and the log it is now being fed disagree.
    LogIdNotAdvancing {
        delivered: LogId,
        last_applied: LogId,
    },
    /// A committed entry decoded to the forward-compatibility carrier.
    ///
    /// §5.3.1: an unknown command is **fatal on every voter**, not skipped. The
    /// carrier exists so a newer peer's payload does not corrupt a decoder, not
    /// so an unrecognised command can be applied -- and applying one is
    /// impossible by definition, since this version does not know what it
    /// means. Skipping it would diverge the voters that *do* know; halting
    /// stops this one and lets the quorum continue without it.
    ///
    /// It cannot arise from a conforming leader, which rejects `Unknown` with
    /// `INVALID_ARGUMENT` before the hash is computed and therefore before
    /// proposal.
    UnknownCommand,
    /// An audit row exists for an entry `last_applied` says was never applied.
    ///
    /// §11.3 writes both in one transaction, so this state is unreachable
    /// unless that atomicity was violated -- a torn write, or a store that
    /// committed one table and not the other.
    AuditAheadOfLastApplied {
        audit_tip: LogId,
        last_applied: Option<LogId>,
    },
}

/// The result of a committed client command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientResult {
    pub disposition: Disposition,
    pub reply: SavedReply,
    /// Whether this answer came from the session table rather than from
    /// applying now.
    ///
    /// Carried on the result rather than re-derived at projection time: only
    /// apply knows, and a projection that guessed would have to read state --
    /// which is exactly what makes a replay inexact.
    pub replayed: bool,
}

impl ClientResult {
    /// Is this answer terminal (§10)?
    pub fn is_terminal(&self) -> bool {
        self.disposition.is_terminal(self.reply.reason)
    }

    /// Is this the one legal `(100 Rejected, 0 None)` pairing -- a committed
    /// request-shape violation?
    ///
    /// Every other rejection carries a defined, nonzero reason. Exposed so
    /// callers can assert the rule rather than restate it.
    pub fn is_forced_shape_rejection(&self) -> bool {
        self.reply.outcome == Outcome::Rejected && self.reply.reason == Reason::None
    }
}

/// Answer a request before proposing it (§10 stages 2-4).
///
/// Session resolution runs **first** and unconditionally: even a denial that
/// will be answered as a bare status must resolve against committed session
/// state, or a committed result is stranded behind a permanent authorization
/// failure and the sequence never terminates (§8.2.1).
pub fn preflight(state: &State, client: &ClientRef, action: &Action) -> Admission {
    let session = state.sessions.get(&(client.actor, client.client_id));
    let capacity = state.capacity_for(&client.actor);

    match classify(session, client.sequence, &client.request_hash, capacity) {
        // Stage 2. Every resolved row, terminal or not.
        Classification::Replay { saved, .. } => Admission::Resolved(Box::new(ClientResult {
            disposition: Disposition::Consumed,
            reply: *saved,
            replayed: true,
        })),
        Classification::Refuse { reason, .. } => Admission::Resolved(Box::new(ClientResult {
            disposition: Disposition::NotConsumed,
            reply: rejection(state, action.validated_resource(), reason),
            replayed: false,
        })),
        Classification::Unresolved { .. } => {
            project(action.decide(state, &client.actor, &client.request_hash))
        }
    }
}

/// Project an unresolved decision onto §10's stages 3 and 4.
///
/// Keyed on the refusal reason, not on which command produced it: the stages
/// are a property of the reason, and duplicating the mapping per command is how
/// two of them would come to disagree.
fn project(decision: ActionDecision) -> Admission {
    let (refusal, shapeless) = match decision {
        ActionDecision::Resource(Decision::Apply(_))
        | ActionDecision::Grant(grant::Decision::Apply(_))
        | ActionDecision::Admin(admin::Decision::Apply(_))
        // A no-op still commits a saved reply, so it is admitted like any
        // other successful command rather than answered pre-proposal.
        | ActionDecision::Grant(grant::Decision::NoOp { .. })
        | ActionDecision::Admin(admin::Decision::NoOp { .. })
        | ActionDecision::Membership(membership::Decision::Apply(_))
        | ActionDecision::Membership(membership::Decision::Existing { .. }) => {
            return Admission::Admit
        }
        ActionDecision::Resource(Decision::Refuse(r))
        | ActionDecision::Grant(grant::Decision::Refuse(r))
        | ActionDecision::Admin(admin::Decision::Refuse(r))
        | ActionDecision::Membership(membership::Decision::Refuse(r)) => (Some(r), None),
        ActionDecision::Resource(Decision::RefuseShapeless(l))
        | ActionDecision::Grant(grant::Decision::RefuseShapeless(l))
        | ActionDecision::Membership(membership::Decision::RefuseShapeless(l)) => (None, Some(l)),
    };
    project_refusal(refusal, shapeless)
}

fn project_refusal(refusal: Option<RefusalReason>, shapeless: Option<ShapeLimit>) -> Admission {
    match (refusal, shapeless) {
        (_, Some(limit)) => Admission::Status(StatusDenial::Shape(limit)),
        // Authorization has no §5.2 reason code pre-proposal (§10 stage 4),
        // even though apply saves it as `12 PermissionChanged`.
        (Some(RefusalReason::PermissionChanged), None) => {
            Admission::Status(StatusDenial::NotAuthorized)
        }
        // Stage 3: exactly the four reason-coded admission denials.
        (Some(reason), None) if reason.is_admission_denial() => Admission::Deny(reason),
        // Everything else is **admitted** precisely so it reaches the log and
        // commits a saved result. The clearest case is `6 AclRevisionMismatch`:
        // denied here it would be a non-terminal `NotConsumed`, and since a
        // client may never change the request at a sequence it has already
        // sent, it would retry the stale revision forever.
        (Some(_), None) => Admission::Admit,
        (None, None) => Admission::Admit,
    }
}

/// Build a rejection reply.
///
/// `resource` is the **validated** identity or `None`; a raw action key never
/// reaches here. That applies to classifier refusals too -- they name a
/// sequence, not a resource, and copying an unvalidated key into one would put
/// it in the reserve-funded session table by a different door.
/// Build a rejection reply.
///
/// The snapshot is the resource's state **as the rejection observed it**, not
/// zeros: a client that lost its response and replays a conflict must be able to
/// read the revision it actually conflicted with, or it cannot resync and will
/// send the same stale expectation again. `None` when the identity was not
/// well-formed or the command names no resource.
fn rejection(state: &State, resource: Option<ResourceKey>, reason: Reason) -> SavedReply {
    let snapshot = resource.map(|key| {
        state.resources.get(&key).map_or_else(
            || SavedSnapshot::of(key.clone(), &ResourceRow::default()),
            |row| SavedSnapshot::of(key.clone(), row),
        )
    });
    let acl_revision = snapshot.as_ref().map_or(0, |s| s.acl_revision);
    SavedReply {
        outcome: Outcome::Rejected,
        reason,
        snapshot,
        acl_revision,
        admin_revision: state.admin_revision,
        operation_id: None,
        membership_log_id: None,
    }
}

/// A committed command: OpenRaft's `D`, in model terms (§5.3).
///
/// Four variants, and only one of them has a session. Modelling the other three
/// as client actions would give them a sequence they do not have -- the mistake
/// an earlier revision made with `retire_client`, which then needed a rule
/// forbidding a client from retiring the session it was calling from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// A committed entry this version cannot interpret (§5.3.1). Modelled so
    /// the fatal path is reachable and testable, rather than being a comment
    /// about what the decoder ought to do.
    Unknown,
    Client {
        client: ClientRef,
        action: Action,
    },
    /// §10.1's forced apply: a committed client command whose **mutation
    /// prefix** is well-formed but whose action violates a logical
    /// request-shape limit -- an oversized encoding, an ill-formed field, or a
    /// request hash that does not match its action.
    ///
    /// This is a command, not an early reply. An earlier revision answered it
    /// at the wire boundary before `apply_command` ran, so a committed entry
    /// consumed no sequence, saved no result, wrote no audit row, charged no
    /// reserve, passed no delivery guard, and did not advance `last_applied` --
    /// which is precisely the set of things §10.1 requires it to do
    /// atomically. The sequence is real and the client will retry it; without a
    /// saved result that retry would be classified as a first attempt forever.
    ///
    /// `operation_code` comes from the wire action's variant, which is known
    /// even when its fields are not, so the audit row still says what was
    /// attempted.
    RejectedClient {
        client: ClientRef,
        operation_code: i32,
        /// The resource the action named, **if that identity is itself valid**.
        ///
        /// §10.1: a rejected request's identity is stored only when it is valid.
        /// An oversized `compare_and_set` names a perfectly good resource -- it
        /// is the encoded command that is too big -- so its saved reply and
        /// audit row carry it. A request whose *identity* is what was malformed
        /// carries `None`, because the session and audit charges are
        /// `256 + KEY` and an unvalidated key would blow the 832-byte row the
        /// whole reserve proof rests on.
        resource: Option<ResourceKey>,
    },
    /// A committed command whose **mutation prefix** is ill-formed: an actor id
    /// or client id of the wrong width, or a malformed request hash.
    ///
    /// Deliberately its own rule rather than a case of [`Command::RejectedClient`].
    /// Such an entry names no session -- there is no validated `(actor,
    /// client_id)` to key one by -- so it cannot consume a sequence, save a
    /// result, or be replayed.
    ///
    /// It **is** audited. §11.3 requires exactly one audit row for every
    /// committed user or system command, and a hostile malformed entry is
    /// precisely the one that must not vanish from the record. Attribution is
    /// optional rather than invented: an actor id that narrowed is recorded even
    /// when the client id or request hash did not, a system command records the
    /// proposing log leader, and only an actor id that will not narrow is stored
    /// as `None`. So the entry writes its audit row, the matching reserve
    /// charge, and `last_applied` -- and nothing else -- identically on every
    /// voter.
    Shapeless(ShapelessCommand),
    Expire(ExpireCommand),
    MembershipProgress(MembershipProgressCommand),
    RetireClient(RetireCommand),
}

/// A committed command with no usable identity (§10.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShapelessCommand {
    /// The §5.2 method the wire variant named, if it named one.
    pub operation_code: i32,
    /// Which reply the caller is waiting for. OpenRaft's `R` is one union, and
    /// a client waiting on a mutation cannot be answered with a system reply.
    pub reply_shape: ReplyShape,
    /// The system result code for a system command, so a malformed `Expire` is
    /// not answered with a membership-phase code.
    pub system_result_code: i32,
    /// The actor, when the actor id validated but something else in the prefix
    /// did not.
    ///
    /// §11.3 requires exactly one audit row per committed command, so a
    /// shapeless entry is audited too -- and a valid actor with a malformed
    /// client id is still attributable. `None` means the actor id itself would
    /// not narrow, which is the only case where attribution is genuinely
    /// unavailable; a **system** command instead attributes the proposing log
    /// leader, which apply stamps.
    pub actor: Option<NodeId>,
    /// The resource the action named, if that identity is itself valid.
    ///
    /// §10.1 rule 3: a malformed *prefix* does not erase a separately valid
    /// resource identity from the reply or the audit row. The two failures are
    /// independent -- a client id of the wrong width says nothing about the
    /// resource the request named.
    pub resource: Option<ResourceKey>,
}

/// Which arm of `CommandReply` a shapeless command must still produce.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplyShape {
    Client,
    Retirement,
    System,
}

/// The reply for a committed command: OpenRaft's `R` (§5.3).
///
/// One variant per command shape, because `disposition` is meaningful for
/// exactly one of them. A struct carrying fields that do not apply would have to
/// default them, and a defaulted disposition reads as a real answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandReply {
    Client(Box<ClientResult>),
    System(SystemResult),
    Retirement(RetirementResult),
}

/// `Expire` and `MembershipProgress` results. No disposition and no outcome
/// code: there is no client and no sequence, so nothing was consumed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SystemResult {
    pub system_result_code: i32,
    pub lease_revision: u64,
}

/// A `retire_client` result. It has a caller but no sequence, so no
/// disposition; terminality is a property of the reason alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetirementResult {
    pub outcome: Outcome,
    pub reason: Reason,
}

impl RetirementResult {
    pub fn is_terminal(&self) -> bool {
        self.outcome == Outcome::ClientRetired || self.reason.is_terminal_when_not_consumed()
    }
}

/// Apply one committed command -- the single top-level entry point.
///
/// Every `CommandV1` variant lands here, so the delivery guard, the audit row
/// and `last_applied` are enforced once rather than per command shape.
pub fn apply_command(
    state: &mut State,
    log_id: LogId,
    command: &Command,
) -> Result<CommandReply, Fault> {
    apply_command_recording(state, log_id, command, &mut ChangeSet::default())
}

/// [`apply_command`], additionally reporting which durable rows it touched.
///
/// The storage layer needs the row set to write §11.3's one transaction, and
/// this is the only place it can honestly come from. `apply_command` above is
/// the same call with the report discarded, which keeps every caller that does
/// not persist -- the model's own tests, the preflight paths -- unchanged.
///
/// `changes` is **added to**, not replaced: one redb transaction covers a whole
/// apply batch, so the caller accumulates across entries and commits once.
pub fn apply_command_recording(
    state: &mut State,
    log_id: LogId,
    command: &Command,
    changes: &mut ChangeSet,
) -> Result<CommandReply, Fault> {
    guard_delivery(state, log_id)?;

    let reply = match command {
        // Fatal before anything is written, and before `last_applied` moves:
        // a voter that cannot interpret an entry must not record having
        // applied it.
        Command::Unknown => return Err(Fault::UnknownCommand),
        Command::Client { client, action } => CommandReply::Client(Box::new(apply_client(
            state, log_id, client, action, changes,
        ))),
        Command::RejectedClient {
            client,
            operation_code,
            resource,
        } => CommandReply::Client(Box::new(apply_rejected_client(
            state,
            log_id,
            client,
            *operation_code,
            resource.clone(),
            changes,
        ))),
        Command::Shapeless(shapeless) => apply_shapeless(state, log_id, shapeless, changes),
        Command::Expire(expire) => {
            CommandReply::System(apply_expire(state, log_id, expire, changes))
        }
        Command::MembershipProgress(progress) => {
            CommandReply::System(apply_membership_progress(state, log_id, progress, changes))
        }
        Command::RetireClient(retire) => {
            CommandReply::Retirement(apply_retire(state, log_id, retire, changes))
        }
    };

    state.last_applied = Some(log_id);
    Ok(reply)
}

/// §10.1's forced apply for a client command whose action is shapeless.
///
/// The classifier runs first and unchanged, so this differs from an ordinary
/// command only in what it decides:
///
/// * a **replay** returns the saved reply verbatim -- a sequence already
///   answered is answered the same way whatever the new request looks like;
/// * a **classifier refusal** keeps its own reason code, because §5.2 has one
///   for that condition and `(100, 0)` is reserved for requests that are
///   shapeless to the protocol;
/// * an **unresolved** sequence is where the forced apply happens: `Consumed`,
///   `(100 Rejected, 0 None)`, no business-state mutation, and the reply saved
///   so the client's retry replays it rather than being reclassified.
fn apply_rejected_client(
    state: &mut State,
    log_id: LogId,
    client: &ClientRef,
    operation_code: i32,
    resource: Option<ResourceKey>,
    changes: &mut ChangeSet,
) -> ClientResult {
    let session = state.sessions.get(&(client.actor, client.client_id));
    let capacity = state.capacity_for(&client.actor);
    let classification = classify(session, client.sequence, &client.request_hash, capacity);

    let (result, writes_session) = match classification {
        Classification::Replay { saved, .. } => (
            ClientResult {
                disposition: Disposition::Consumed,
                reply: *saved,
                replayed: true,
            },
            false,
        ),
        Classification::Refuse { reason, .. } => (
            ClientResult {
                disposition: Disposition::NotConsumed,
                reply: rejection(state, resource.clone(), reason),
                replayed: false,
            },
            false,
        ),
        Classification::Unresolved { .. } => (
            ClientResult {
                disposition: Disposition::Consumed,
                reply: rejection(state, resource.clone(), Reason::None),
                replayed: false,
            },
            true,
        ),
    };

    if writes_session {
        write_session(state, client, Some(result.reply.clone()), changes);
    }
    write_audit_row(
        state,
        log_id,
        AuditRow {
            leader_node_id: log_id.leader_node_id,
            actor: Some(client.actor),
            operation_code,
            // The validated key or none at all -- never the malformed one. An
            // unvalidated resource in bounded metadata is what the §12.1 charge
            // is computed from.
            resource: result.reply.resource().cloned(),
            result: AuditResult::Client(result.reply.outcome),
            epoch: 0,
            value_revision: 0,
        },
        changes,
    );
    result
}

/// Apply a committed command with no usable session identity (§10.1).
///
/// It consumes no sequence and saves no result -- there is no session to key
/// either by -- but §11.3 requires **exactly one audit row for every committed
/// user or system command**, and a hostile malformed entry is precisely the one
/// that must not vanish from the record. The row is written with optional
/// attribution: an actor id that narrowed is recorded even when the client id or
/// request hash did not, and `None` marks the case where the actor id itself was
/// the malformed field.
fn apply_shapeless(
    state: &mut State,
    log_id: LogId,
    command: &ShapelessCommand,
    changes: &mut ChangeSet,
) -> CommandReply {
    let reply = match command.reply_shape {
        ReplyShape::Client => CommandReply::Client(Box::new(ClientResult {
            // Nothing was consumed, because there is no session to consume a
            // sequence in.
            disposition: Disposition::NotConsumed,
            reply: rejection(state, command.resource.clone(), Reason::None),
            replayed: false,
        })),
        ReplyShape::Retirement => CommandReply::Retirement(RetirementResult {
            outcome: Outcome::Rejected,
            reason: Reason::None,
        }),
        ReplyShape::System => CommandReply::System(SystemResult {
            system_result_code: command.system_result_code,
            lease_revision: 0,
        }),
    };

    write_audit_row(
        state,
        log_id,
        AuditRow {
            leader_node_id: log_id.leader_node_id,
            // A system command has no caller, so it attributes the proposing
            // leader -- the same rule ordinary `Expire` and
            // `MembershipProgress` rows follow. `None` is reserved for a client
            // or retirement command whose actor bytes would not narrow.
            actor: match command.reply_shape {
                ReplyShape::System => Some(log_id.leader_node_id),
                ReplyShape::Client | ReplyShape::Retirement => command.actor,
            },
            operation_code: command.operation_code,
            resource: command.resource.clone(),
            // A client-shaped entry audits as a **client** rejection: outcome
            // 100 in the client code space, matching its operation code. Only a
            // genuinely system command carries a system result, which is what
            // keeps the stored row's code space unambiguous without a second
            // field naming it.
            result: match command.reply_shape {
                ReplyShape::System => AuditResult::System(command.system_result_code),
                ReplyShape::Client | ReplyShape::Retirement => {
                    AuditResult::Client(Outcome::Rejected)
                }
            },
            epoch: 0,
            value_revision: 0,
        },
        changes,
    );
    reply
}

/// Apply a committed `retire_client` (§10).
fn apply_retire(
    state: &mut State,
    log_id: LogId,
    command: &RetireCommand,
    changes: &mut ChangeSet,
) -> RetirementResult {
    let decision = retire::decide(state, command);
    let result = match &decision {
        retire::Decision::Apply(effect) => {
            if let Some(row) = retire::effect_row(state, command, effect) {
                let key = (command.target_actor, command.target_client_id);
                let previous = state.sessions.get(&key).map(session_charge);
                state.reserve_bytes =
                    state.reserve_bytes + session_charge(&row) - previous.unwrap_or(0);
                changes.sessions.insert(key);
                state.sessions.insert(key, row);
            }
            RetirementResult {
                outcome: Outcome::ClientRetired,
                reason: Reason::None,
            }
        }
        retire::Decision::Refuse(reason) => RetirementResult {
            outcome: Outcome::Rejected,
            reason: reason.get(),
        },
    };

    // Sessionless, so there is no sequence to deduplicate on: a retry
    // legitimately produces another audit row. The state transition is
    // idempotent; the audit trail is not.
    write_audit_row(
        state,
        log_id,
        AuditRow {
            leader_node_id: log_id.leader_node_id,
            actor: Some(command.actor),
            operation_code: OPERATION_RETIRE_CLIENT,
            resource: None,
            result: AuditResult::Client(result.outcome),
            epoch: 0,
            value_revision: 0,
        },
        changes,
    );
    result
}

/// §5.2: "audit operation codes 1-11 are the mutation method codes".
///
/// The method table assigns 1-9 and 11, leaving **10** unused -- and
/// `retire_client` is the one mutation deliberately absent from that table
/// (§5.2), so 10 is the code the contract retains for it. An earlier revision
/// invented 12, which is outside the 1-11 range the contract declares.
pub const OPERATION_RETIRE_CLIENT: i32 = 10;
/// System `Expire` (§5.2).
pub const OPERATION_EXPIRE: i32 = 100;
/// `MembershipProgress` (§5.2).
pub const OPERATION_MEMBERSHIP_PROGRESS: i32 = 101;

/// Apply one committed client command.
///
/// `log_id` is the complete entry identity (§11.2). Delivery at or below
/// `last_applied` is a [`Fault`], checked **before anything is decided or
/// written**: a guard that runs after the business decision has already let the
/// mutation happen.
pub fn apply_client_command(
    state: &mut State,
    log_id: LogId,
    client: &ClientRef,
    action: &Action,
) -> Result<ClientResult, Fault> {
    match apply_command(
        state,
        log_id,
        &Command::Client {
            client: client.clone(),
            action: action.clone(),
        },
    )? {
        CommandReply::Client(result) => Ok(*result),
        other => unreachable!("a client command produces a client reply, got {other:?}"),
    }
}

/// The client-command body, after [`guard_delivery`] has run.
fn apply_client(
    state: &mut State,
    log_id: LogId,
    client: &ClientRef,
    action: &Action,
    changes: &mut ChangeSet,
) -> ClientResult {
    let session = state.sessions.get(&(client.actor, client.client_id));
    let capacity = state.capacity_for(&client.actor);
    let classification = classify(session, client.sequence, &client.request_hash, capacity);

    let (result, writes_session) = match classification {
        // A duplicate that was itself proposed and committed, at a new and
        // higher index. It re-applies nothing and returns the saved row
        // **verbatim** -- reconstructing it field by field is how the spike
        // lost a committed `19`, which replayed as "rejected, no cause" -- but
        // it is an applied entry, so it still audits below.
        Classification::Replay { saved, .. } => (
            ClientResult {
                disposition: Disposition::Consumed,
                reply: *saved,
                replayed: true,
            },
            false,
        ),

        // A classifier refusal writes no session row -- "creating nothing" in
        // the capacity case means no session or business row. It still writes
        // its audit row and advances `last_applied`.
        Classification::Refuse { reason, .. } => (
            ClientResult {
                disposition: Disposition::NotConsumed,
                reply: rejection(state, action.validated_resource(), reason),
                replayed: false,
            },
            false,
        ),

        Classification::Unresolved { .. } => {
            (apply_decided(state, log_id, client, action, changes), true)
        }
    };

    if writes_session {
        write_session(state, client, Some(result.reply.clone()), changes);
    }
    write_audit(state, log_id, client, &result.reply, action, changes);
    result
}

/// Apply a committed system `Expire` (§9.1).
fn apply_expire(
    state: &mut State,
    log_id: LogId,
    command: &ExpireCommand,
    changes: &mut ChangeSet,
) -> SystemResult {
    let decision = expire::decide(state, command);
    if let expire::Decision::Apply { row } = &decision {
        // Safety-reducing and value-preserving, so the growth charge is
        // unchanged -- expiry is neither refused for budget nor a way out of
        // exhaustion.
        state
            .resources
            .insert(command.resource.clone(), row.clone());
    }
    let row = state.resources.get(&command.resource);
    let lease_revision = row.map_or(0, |r| r.lease_revision);
    let epoch = row.map_or(0, |r| r.epoch);

    write_audit_row(
        state,
        log_id,
        AuditRow {
            leader_node_id: log_id.leader_node_id,
            // A system command has no caller. The proposing leader is already
            // the entry's identity, so the actor carries the same id rather
            // than a placeholder that would read as a real node.
            actor: Some(log_id.leader_node_id),
            operation_code: OPERATION_EXPIRE,
            resource: Some(command.resource.clone()),
            // §5.2: a system command's audit result is its **phase code**, not
            // a client outcome. Coercing it to `Rejected` recorded a successful
            // expiry as a rejection and put two disjoint code spaces in one
            // untagged field.
            result: AuditResult::System(decision.system_result_code()),
            epoch,
            value_revision: 0,
        },
        changes,
    );

    SystemResult {
        system_result_code: decision.system_result_code(),
        lease_revision,
    }
}

/// Apply a committed system `MembershipProgress` (§9).
fn apply_membership_progress(
    state: &mut State,
    log_id: LogId,
    command: &MembershipProgressCommand,
    changes: &mut ChangeSet,
) -> SystemResult {
    let code = match membership::decide_progress(state, command) {
        Some(effect) => {
            write_intent(state, log_id, effect, changes);
            crate::coordination::contract::SYS_MEMBERSHIP_ADVANCED
        }
        None => crate::coordination::contract::SYS_MEMBERSHIP_PHASE_MISMATCH,
    };

    write_audit_row(
        state,
        log_id,
        AuditRow {
            leader_node_id: log_id.leader_node_id,
            actor: Some(log_id.leader_node_id),
            operation_code: OPERATION_MEMBERSHIP_PROGRESS,
            resource: None,
            result: AuditResult::System(code),
            epoch: 0,
            value_revision: 0,
        },
        changes,
    );

    SystemResult {
        system_result_code: code,
        lease_revision: 0,
    }
}

/// Store an intent and evict whatever its acceptance displaced.
fn write_intent(
    state: &mut State,
    log_id: LogId,
    mut effect: membership::IntentEffect,
    changes: &mut ChangeSet,
) {
    for id in &effect.evict {
        changes.intents.insert(*id);
        if let Some(old) = state.intents.remove(id) {
            state.growth_bytes -= limits::charge_intent(old.charge_len(id));
        }
    }
    if effect.intent.accepted_log_id.index == 0 {
        effect.intent.accepted_log_id = log_id;
    }
    // Eviction orders by completion, so the terminal phase is where the
    // completion id is stamped.
    if membership::is_terminal_phase(effect.intent.status_code)
        && effect.intent.completed_log_id.is_none()
    {
        effect.intent.completed_log_id = Some(log_id);
    }

    let previous = state
        .intents
        .get(&effect.operation_id)
        .map(|i| limits::charge_intent(i.charge_len(&effect.operation_id)));
    state.growth_bytes = state.growth_bytes
        + limits::charge_intent(effect.intent.charge_len(&effect.operation_id))
        - previous.unwrap_or(0);
    changes.intents.insert(effect.operation_id);
    state.intents.insert(effect.operation_id, effect.intent);

    for node in &effect.evict_routes {
        changes.routes.insert(*node);
        if let Some(old) = state.routes.remove(node) {
            state.growth_bytes -= limits::charge_route(old.ticket.len());
        }
    }
    for (node, route) in effect.routes {
        let mut route = route;
        if route.introduced_log_id.index == 0 {
            route.introduced_log_id = log_id;
        }
        let previous = state
            .routes
            .get(&node)
            .map(|r| limits::charge_route(r.ticket.len()));
        state.growth_bytes =
            state.growth_bytes + limits::charge_route(route.ticket.len()) - previous.unwrap_or(0);
        changes.routes.insert(node);
        state.routes.insert(node, route);
    }

    // No membership authority is written here, deliberately. §7.3: OpenRaft
    // membership entries remain the authority, so `members` and
    // `membership_log_id` move only when the storage layer applies a native
    // `EntryPayload::Membership`. An intent records what a controller observed;
    // it never establishes it.
}

/// Reject a delivery that cannot belong to a committed history, before any
/// mutation.
///
/// Crate-visible because Slice 2's state machine shares it. OpenRaft's own
/// `Blank` and `Membership` entries are not `CommandV1`s and so do not reach
/// [`apply_command`], but §10.2's rule is about *delivery*, not about what was
/// delivered: a native entry at or below `last_applied` is the same broken
/// history a duplicate command would be. A second copy of the rule in the
/// storage layer would be a second place for it to drift.
///
/// Note it requires a **strictly greater** index, not a contiguous one. A
/// state machine legitimately receives an index above `last_applied + 1` --
/// OpenRaft's own suite does exactly that -- and the append-continuity rules
/// belong to the log store, not here.
///
/// Two independent conditions:
///
/// * `log_id.index <= last_applied.index` -- an already-applied entry, an older
///   entry arriving late, or a different leader's entry claiming a position
///   this node has already applied. None can occur in a committed history that
///   was applied transactionally, so each is a runtime or storage invariant
///   failure rather than a client-visible replay.
/// * an audit row ahead of `last_applied` -- §11.3's one-transaction rule
///   broken.
///
/// Note what is deliberately *not* consulted: whether an audit row already
/// exists for this identity. Audit is prunable and lossy, so its absence proves
/// nothing and its presence cannot identify a command.
pub fn guard_delivery(state: &State, log_id: LogId) -> Result<(), Fault> {
    if let Some(last) = state.last_applied {
        if log_id.index <= last.index {
            return Err(Fault::LogIdNotAdvancing {
                delivered: log_id,
                last_applied: last,
            });
        }
    }

    if let Some(tip) = state.audit_tip {
        let consistent = match state.last_applied {
            // An audit row with nothing applied at all.
            None => false,
            // Ahead of what was applied, or -- the case a maximum-index scan
            // cannot see -- at the same index but from a different term or
            // leader, which is a different entry.
            Some(last) => tip.index < last.index || tip == last,
        };
        if !consistent {
            return Err(Fault::AuditAheadOfLastApplied {
                audit_tip: tip,
                last_applied: state.last_applied,
            });
        }
    }

    Ok(())
}

/// The unresolved path: decide, then write whatever the decision says.
fn apply_decided(
    state: &mut State,
    log_id: LogId,
    client: &ClientRef,
    action: &Action,
    changes: &mut ChangeSet,
) -> ClientResult {
    match action.decide(state, &client.actor, &client.request_hash) {
        ActionDecision::Resource(Decision::Apply(effect)) => {
            charge_resource_row(state, &effect.key, effect.row.value.len());
            changes.resources.insert(effect.key.clone());
            state
                .resources
                .insert(effect.key.clone(), effect.row.clone());
            consumed_success(effect.outcome, effect.key, &effect.row)
        }

        ActionDecision::Grant(grant::Decision::Apply(effect)) => {
            // Both rows or neither: the resource row and the ACL row move in
            // the same step, so there is no shape in which a grant points at a
            // resource that does not exist.
            charge_resource_row(state, &effect.key, effect.row.value.len());
            changes.resources.insert(effect.key.clone());
            state
                .resources
                .insert(effect.key.clone(), effect.row.clone());

            let acl_key = (effect.key.clone(), effect.subject);
            changes.grants.insert(acl_key.clone());
            let (ns, k) = key_parts(&effect.key);
            match effect.rights {
                Some(rights) => {
                    if effect.creates_acl_row {
                        state.growth_bytes += limits::charge_acl(ns, k);
                    }
                    state.acl.insert(acl_key, rights);
                }
                None => {
                    if state.acl.remove(&acl_key).is_some() {
                        state.growth_bytes -= limits::charge_acl(ns, k);
                    }
                }
            }

            consumed_success(Outcome::GrantUpdated, effect.key, &effect.row)
        }

        ActionDecision::Admin(admin::Decision::Apply(effect)) => {
            changes.admins.insert(effect.target);
            if effect.enabled {
                if state.admins.insert(effect.target) {
                    state.growth_bytes += limits::charge_admin();
                }
            } else if state.admins.remove(&effect.target) {
                state.growth_bytes -= limits::charge_admin();
            }
            state.admin_revision = effect.admin_revision;
            ClientResult {
                disposition: Disposition::Consumed,
                reply: admin_reply(effect.admin_revision),
                replayed: false,
            }
        }

        ActionDecision::Admin(admin::Decision::NoOp { admin_revision }) => ClientResult {
            disposition: Disposition::Consumed,
            reply: admin_reply(admin_revision),
            replayed: false,
        },

        ActionDecision::Membership(membership::Decision::Apply(effect)) => {
            let (outcome, operation_id) = (effect.outcome, effect.operation_id);
            write_intent(state, log_id, *effect, changes);
            membership_reply(outcome, operation_id, state.membership_log_id)
        }

        // The identical request under an id already accepted: the existing
        // intent is the answer, and nothing is written a second time.
        ActionDecision::Membership(membership::Decision::Existing { .. }) => membership_reply(
            Outcome::MembershipAccepted,
            action.operation_id().expect("a membership action"),
            state.membership_log_id,
        ),

        // A legal command that changes nothing still commits a saved reply --
        // that is what consumes the sequence and lets a retry be answered
        // without re-deciding.
        ActionDecision::Grant(grant::Decision::NoOp { acl_revision }) => ClientResult {
            disposition: Disposition::Consumed,
            reply: no_op_grant_reply(state, action.validated_resource(), acl_revision),
            replayed: false,
        },

        // A reason-coded refusal that reached the log. Consumed, saved, and
        // terminal because it is saved -- even a `19` whose budget condition
        // later clears.
        ActionDecision::Resource(Decision::Refuse(reason))
        | ActionDecision::Grant(grant::Decision::Refuse(reason))
        | ActionDecision::Admin(admin::Decision::Refuse(reason))
        | ActionDecision::Membership(membership::Decision::Refuse(reason)) => ClientResult {
            disposition: Disposition::Consumed,
            reply: rejection(state, action.validated_resource(), reason.get()),
            replayed: false,
        },

        // The forced-apply case (§10.1). Business state is untouched.
        //
        // The saved reply keeps the resource when the identity itself was
        // valid -- an oversized value or an out-of-range TTL names a perfectly
        // good resource, and dropping it would lose information the client can
        // use. Only a **malformed identity** yields `None`, because there is no
        // validated key to store.
        ActionDecision::Resource(Decision::RefuseShapeless(_))
        | ActionDecision::Grant(grant::Decision::RefuseShapeless(_))
        | ActionDecision::Membership(membership::Decision::RefuseShapeless(_)) => ClientResult {
            disposition: Disposition::Consumed,
            reply: rejection(state, action.validated_resource(), Reason::None),
            replayed: false,
        },
    }
}

fn admin_reply(admin_revision: u64) -> SavedReply {
    SavedReply {
        outcome: Outcome::AdminUpdated,
        reason: Reason::None,
        snapshot: None,
        acl_revision: 0,
        admin_revision,
        operation_id: None,
        membership_log_id: None,
    }
}

/// A `set_grant` no-op still reports the resource's state, so a replay is exact.
fn no_op_grant_reply(
    state: &State,
    resource: Option<ResourceKey>,
    acl_revision: u64,
) -> SavedReply {
    let snapshot = resource.and_then(|key| {
        state
            .resources
            .get(&key)
            .map(|row| SavedSnapshot::of(key.clone(), row))
    });
    SavedReply {
        outcome: Outcome::GrantUpdated,
        reason: Reason::None,
        snapshot,
        acl_revision,
        admin_revision: 0,
        operation_id: None,
        membership_log_id: None,
    }
}

/// A membership reply carries the operation id and the membership log id --
/// both of which §5.2.1 gives dedicated fields, and both of which an earlier
/// revision discarded into an unrelated `admin_revision` slot.
fn membership_reply(
    outcome: Outcome,
    operation_id: OperationId,
    membership_log_id: Option<LogId>,
) -> ClientResult {
    ClientResult {
        replayed: false,
        disposition: Disposition::Consumed,
        reply: SavedReply {
            outcome,
            reason: Reason::None,
            snapshot: None,
            acl_revision: 0,
            admin_revision: 0,
            operation_id: Some(operation_id),
            membership_log_id,
        },
    }
}

/// Move the growth counter by a resource row's charge delta.
fn charge_resource_row(state: &mut State, key: &ResourceKey, new_value_len: usize) {
    let (ns, k) = key_parts(key);
    let old = state
        .resources
        .get(key)
        .map(|row| limits::charge_resource(ns, k, row.value.len()))
        .unwrap_or(0);
    let new = limits::charge_resource(ns, k, new_value_len);
    state.growth_bytes = state.growth_bytes + new - old;
}

/// A successful client result, snapshotting the row **as written**.
///
/// The snapshot is taken from the effect's row rather than from state at
/// projection time, so a replay after a later mutation still reports what this
/// command produced.
fn consumed_success(outcome: Outcome, key: ResourceKey, row: &ResourceRow) -> ClientResult {
    ClientResult {
        replayed: false,
        disposition: Disposition::Consumed,
        reply: SavedReply {
            outcome,
            reason: Reason::None,
            snapshot: Some(SavedSnapshot::of(key, row)),
            acl_revision: row.acl_revision,
            admin_revision: 0,
            operation_id: None,
            membership_log_id: None,
        },
    }
}

/// Create or update the session row, maintaining the reserve counter.
fn write_session(
    state: &mut State,
    client: &ClientRef,
    result: Option<SavedReply>,
    changes: &mut ChangeSet,
) {
    let key = (client.actor, client.client_id);
    let previous_charge = state.sessions.get(&key).map(session_charge);
    let row = Session {
        last_sequence: client.sequence,
        request_hash: client.request_hash,
        last_result: result,
        retired: state.sessions.get(&key).is_some_and(|s| s.retired),
    };
    state.reserve_bytes = state.reserve_bytes + session_charge(&row) - previous_charge.unwrap_or(0);
    changes.sessions.insert(key);
    state.sessions.insert(key, row);
}

fn session_charge(session: &Session) -> u64 {
    limits::charge_session(
        session
            .last_result
            .as_ref()
            .and_then(|r| r.resource())
            .map(key_parts),
    )
}

/// Append the audit row. Every applied entry writes one, whether it succeeded
/// or was rejected.
///
/// The key is fresh by construction: [`guard_delivery`] has already established
/// that this index is above `last_applied`, and audit never runs ahead of it.
fn write_audit(
    state: &mut State,
    log_id: LogId,
    client: &ClientRef,
    reply: &SavedReply,
    action: &Action,
    changes: &mut ChangeSet,
) {
    let resource = reply.resource().cloned();
    let row = AuditRow {
        // Stored even though term and index are the table key: together they
        // reconstruct this entry's full log id once the log has been purged.
        leader_node_id: log_id.leader_node_id,
        actor: Some(client.actor),
        operation_code: action.operation_code(),
        resource: resource.clone(),
        result: AuditResult::Client(reply.outcome),
        // Copied from the saved reply rather than re-read from state, so the
        // audit row and the client's answer cannot disagree about what the
        // entry produced.
        epoch: reply.snapshot.as_ref().map_or(0, |s| s.epoch),
        value_revision: reply.snapshot.as_ref().map_or(0, |s| s.value_revision),
    };
    write_audit_row(state, log_id, row, changes);
}

/// Insert a prebuilt audit row, advance the tip, and prune to the retention
/// limit.
fn write_audit_row(state: &mut State, log_id: LogId, row: AuditRow, changes: &mut ChangeSet) {
    state.reserve_bytes += limits::charge_audit(row.resource.as_ref().map(key_parts));
    changes.audit.insert((log_id.term, log_id.index));
    state.audit.insert((log_id.term, log_id.index), row);
    state.audit_tip = Some(log_id);
    prune_audit(state, changes);
}

/// Prune the oldest audit rows down to §12.1's retention limit.
///
/// Without this the audit table grows without bound and the 128 MiB reserve
/// proof -- which is `100,000 x 832` audit bytes plus `16,384 x 832` session
/// bytes -- is not a bound on anything. Pruning is deterministic: the table is
/// keyed `(term, index)`, so every voter drops the same rows in the same order.
///
/// `audit_floor` advances with it, so `audit_stream` can answer `OUT_OF_RANGE`
/// with the floor rather than silently returning a partial range (§5.2).
fn prune_audit(state: &mut State, changes: &mut ChangeSet) {
    while state.audit.len() as u64 > limits::MAX_AUDIT_ROWS {
        let Some((&key, row)) = state.audit.iter().next() else {
            break;
        };
        let charge = limits::charge_audit(row.resource.as_ref().map(key_parts));
        let leader = row.leader_node_id;
        changes.audit.insert(key);
        state.audit.remove(&key);
        state.reserve_bytes -= charge;
        // The **whole** log id of the oldest row still retained. The pruned
        // row's leader comes from the row itself, since the table key carries
        // only term and index -- which is exactly why the row stores it.
        state.audit_floor = state
            .audit
            .iter()
            .next()
            .map(|((term, index), oldest)| LogId::new(*term, oldest.leader_node_id, *index))
            .or(Some(LogId::new(key.0, leader, key.1)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::model::admin::AdminAction;
    use crate::coordination::model::grant::GrantAction;
    use crate::coordination::model::limits::{ALL_RIGHTS, MAX_VALUE, RIGHT_ACQUIRE};
    use crate::coordination::model::resource::{Fence, RawResourceKey};
    use crate::coordination::model::state::ResourceRow;

    const ACTOR: NodeId = [1u8; 32];
    const CLIENT: ClientId = [9u8; 16];
    const HASH: [u8; 32] = [0xAA; 32];
    const LEADER: NodeId = [0x11; 32];
    const OTHER_LEADER: NodeId = [0x22; 32];

    fn log(term: u64, index: u64) -> LogId {
        LogId::new(term, LEADER, index)
    }

    fn raw() -> RawResourceKey {
        RawResourceKey::new("ns", b"k".to_vec())
    }

    fn key() -> ResourceKey {
        raw().validate().expect("a valid fixture identity")
    }

    fn granted() -> State {
        let mut s = State::default();
        s.acl.insert((key(), ACTOR), ALL_RIGHTS);
        s.growth_bytes = s.recompute_growth();
        s.reserve_bytes = s.recompute_reserve();
        s
    }

    fn client(sequence: u64) -> ClientRef {
        ClientRef {
            actor: ACTOR,
            client_id: CLIENT,
            sequence,
            request_hash: HASH,
        }
    }

    fn acquire() -> Action {
        Action::Resource(ResourceAction::Acquire {
            resource: raw(),
            ttl_ms: 30_000,
        })
    }

    fn oversized_value() -> Action {
        Action::Resource(ResourceAction::CompareAndSet {
            fence: Fence {
                resource: raw(),
                epoch: 1,
                holder: ACTOR,
            },
            expected_value_revision: 0,
            new_value: vec![0u8; MAX_VALUE + 1],
        })
    }

    /// An action whose *identity* is malformed, so no validated key exists.
    fn malformed_identity() -> Action {
        Action::Resource(ResourceAction::Acquire {
            resource: RawResourceKey::new("NOT a namespace", vec![0u8; 2000]),
            ttl_ms: 30_000,
        })
    }

    fn held_state() -> State {
        let mut s = granted();
        s.resources.insert(
            key(),
            ResourceRow {
                epoch: 1,
                held: true,
                holder: Some(ACTOR),
                ttl_ms: 30_000,
                lease_revision: 1,
                ..Default::default()
            },
        );
        s.growth_bytes = s.recompute_growth();
        s
    }

    fn apply_ok(
        state: &mut State,
        log_id: LogId,
        client: &ClientRef,
        action: &Action,
    ) -> ClientResult {
        apply_client_command(state, log_id, client, action).expect("no fault")
    }

    /// Assert a replay reproduced the original answer exactly.
    ///
    /// Compares the saved reply and the disposition -- everything the client
    /// sees. The `replayed` flag is deliberately excluded: it is *supposed* to
    /// differ, since it is what tells projection to redact the value.
    fn assert_replays(first: &ClientResult, again: &ClientResult) {
        assert_eq!(
            again.reply, first.reply,
            "the saved reply must be identical"
        );
        assert_eq!(again.disposition, first.disposition);
        assert!(
            again.replayed,
            "the second answer came from the session table"
        );
        assert!(!first.replayed, "the first did not");
    }

    /// Assert that a faulting delivery changed **nothing**, over every table
    /// and counter rather than a sampled few -- the guard must run before any
    /// mutation, and a partial write is exactly what that ordering prevents.
    fn assert_state_unchanged(before: &State, after: &State) {
        assert_eq!(before.resources, after.resources, "resources");
        assert_eq!(before.acl, after.acl, "acl");
        assert_eq!(before.admins, after.admins, "admins");
        assert_eq!(before.sessions, after.sessions, "sessions");
        assert_eq!(before.intents, after.intents, "intents");
        assert_eq!(before.routes, after.routes, "routes");
        assert_eq!(before.audit, after.audit, "audit");
        assert_eq!(before.growth_bytes, after.growth_bytes, "growth");
        assert_eq!(before.reserve_bytes, after.reserve_bytes, "reserve");
        assert_eq!(before.last_applied, after.last_applied, "last_applied");
        assert_eq!(
            before.admin_revision, after.admin_revision,
            "admin_revision"
        );
    }

    // ── Stage 4: a conforming leader denies pre-proposal, status-only ───────

    /// Every request-shape limit a **resource action** can violate is denied
    /// pre-proposal as a status, never a typed reply -- there is no reason code
    /// to put in one.
    ///
    /// Scoped to resource actions deliberately: `EncodedCommand` is enforced at
    /// the ingest boundary where an encoded length exists, and the member-seed
    /// and membership-plan limits belong to `change_membership`, which is not
    /// modelled yet. Naming the scope keeps the test from implying it covers
    /// all nine.
    #[test]
    fn preflight_denies_every_resource_action_shape_limit_as_a_status_only() {
        let s = held_state();
        let cases: Vec<(ShapeLimit, Action)> = vec![
            (
                ShapeLimit::Namespace,
                Action::Resource(ResourceAction::Acquire {
                    resource: RawResourceKey::new("NOPE", b"k".to_vec()),
                    ttl_ms: 30_000,
                }),
            ),
            (
                ShapeLimit::ResourceKey,
                Action::Resource(ResourceAction::Acquire {
                    resource: RawResourceKey::new("ns", Vec::new()),
                    ttl_ms: 30_000,
                }),
            ),
            (ShapeLimit::Value, oversized_value()),
            (
                ShapeLimit::Ttl,
                Action::Resource(ResourceAction::Acquire {
                    resource: raw(),
                    ttl_ms: 1,
                }),
            ),
        ];

        for (limit, action) in cases {
            assert_eq!(
                preflight(&s, &client(1), &action),
                Admission::Status(StatusDenial::Shape(limit)),
                "{limit:?} must be a stage-4 status, not a typed reply"
            );
        }
    }

    #[test]
    fn a_stage_four_denial_writes_nothing() {
        let s = held_state();
        let before = s.clone();
        assert!(matches!(
            preflight(&s, &client(1), &oversized_value()),
            Admission::Status(_)
        ));
        assert_eq!(s.sessions.len(), before.sessions.len());
        assert_eq!(s.audit.len(), before.audit.len());
        assert_eq!(s.resources, before.resources);
    }

    #[test]
    fn an_unauthorized_request_is_a_stage_four_status() {
        let mut s = State::default();
        s.acl.insert((key(), ACTOR), ALL_RIGHTS & !RIGHT_ACQUIRE);
        assert_eq!(
            preflight(&s, &client(1), &acquire()),
            Admission::Status(StatusDenial::NotAuthorized)
        );
    }

    // ── Forced apply (§10.1) ────────────────────────────────────────────────

    #[test]
    fn a_committed_shape_violation_commits_rejected_with_no_reason() {
        let mut s = held_state();
        let result = apply_ok(&mut s, log(1, 7), &client(1), &oversized_value());

        assert_eq!(result.disposition, Disposition::Consumed);
        assert_eq!(result.reply.outcome, Outcome::Rejected);
        assert_eq!(result.reply.reason, Reason::None);
        assert!(result.is_forced_shape_rejection());
        assert!(result.is_terminal(), "Consumed is always terminal");
    }

    #[test]
    fn forced_apply_mutates_no_business_state_but_advances_its_metadata() {
        let mut s = held_state();
        let business_before = (
            s.resources.clone(),
            s.acl.clone(),
            s.admins.clone(),
            s.intents.clone(),
            s.routes.clone(),
            s.growth_bytes,
        );

        apply_ok(&mut s, log(1, 7), &client(1), &oversized_value());

        assert_eq!(
            (
                s.resources.clone(),
                s.acl.clone(),
                s.admins.clone(),
                s.intents.clone(),
                s.routes.clone(),
                s.growth_bytes,
            ),
            business_before,
            "no business row and no growth byte may move"
        );

        let session = s.sessions.get(&(ACTOR, CLIENT)).expect("session row");
        assert_eq!(session.last_sequence, 1);
        assert_eq!(session.request_hash, HASH);
        assert!(session.last_result.is_some(), "the rejection is saved");
        assert_eq!(s.audit.len(), 1, "an audit row is written");
        assert_eq!(s.last_applied, Some(log(1, 7)));
        assert!(s.counters_match_recomputation());
    }

    /// A malformed **identity** saves no resource at all, in the reply and in
    /// the audit row alike. Contrast
    /// [`a_shape_violation_with_a_valid_identity_keeps_its_resource`]. Copying the raw key would put an unvalidated -- and
    /// possibly oversized -- identity into reserve-funded metadata, blowing the
    /// 832-byte charge the budget proof rests on.
    #[test]
    fn a_malformed_identity_is_never_copied_into_durable_metadata() {
        let mut s = granted();
        let result = apply_ok(&mut s, log(1, 4), &client(1), &malformed_identity());

        assert!(result.is_forced_shape_rejection());
        assert_eq!(
            result.reply.resource(),
            None,
            "a shapeless identity has no place in a saved reply"
        );

        let session = s.sessions.get(&(ACTOR, CLIENT)).expect("session row");
        assert_eq!(session.last_result.as_ref().unwrap().resource(), None);

        let audit = s.audit.values().next().expect("audit row");
        assert_eq!(audit.resource, None, "nor in an audit row");

        // The charge is the bare row overhead, not the 2 KiB the request named.
        assert!(s.counters_match_recomputation());
        assert_eq!(s.reserve_bytes, 256 + 256, "one session row, one audit row");
    }

    /// Only a malformed *identity* drops the resource. An oversized value or an
    /// out-of-range TTL names a perfectly good resource, and dropping it would
    /// lose information the client can act on.
    #[test]
    fn a_shape_violation_with_a_valid_identity_keeps_its_resource() {
        let mut s = held_state();
        let result = apply_ok(&mut s, log(1, 4), &client(1), &oversized_value());

        assert!(result.is_forced_shape_rejection());
        assert_eq!(
            result.reply.resource().cloned(),
            Some(key()),
            "the identity was valid; only the value was not"
        );
        assert_eq!(s.audit.values().next().unwrap().resource, Some(key()));
    }

    /// A classifier refusal must not copy an unvalidated key either.
    #[test]
    fn a_classifier_refusal_carries_no_unvalidated_identity() {
        let mut s = granted();
        // Sequence 5 with no session: a gap, refused by the classifier before
        // any decision runs.
        let result = apply_ok(&mut s, log(1, 4), &client(5), &malformed_identity());
        assert_eq!(result.reply.reason, Reason::SequenceGap);
        assert_eq!(result.reply.resource(), None);
        assert_eq!(s.audit.values().next().unwrap().resource, None);
    }

    // ── The three replay cases ──────────────────────────────────────────────

    /// Case 1: a preflight replay commits nothing, so there is no audit row.
    #[test]
    fn a_preflight_replay_writes_no_audit_row() {
        let mut s = held_state();
        let first = apply_ok(&mut s, log(1, 7), &client(1), &oversized_value());
        let audit_before = s.audit.len();

        match preflight(&s, &client(1), &oversized_value()) {
            Admission::Resolved(replayed) => assert_replays(&first, &replayed),
            other => panic!("a consumed sequence must resolve, got {other:?}"),
        }
        assert_eq!(s.audit.len(), audit_before, "preflight commits nothing");
        assert_eq!(s.last_applied, Some(log(1, 7)));
    }

    /// Case 2: a duplicate that was itself proposed and committed, at a new and
    /// higher index. No business or session effect -- but it **is** an applied
    /// entry, so it audits and advances `last_applied`.
    ///
    /// This needs no faulty leader: two copies of one request can preflight
    /// before either applies, so both reach the log.
    #[test]
    fn a_committed_duplicate_at_a_higher_index_audits_and_advances_last_applied() {
        let mut s = granted();
        let first = apply_ok(&mut s, log(1, 7), &client(1), &acquire());
        let session_after_first = s.sessions.get(&(ACTOR, CLIENT)).cloned();
        let row_after_first = s.resources.get(&key()).cloned();

        let second = apply_ok(&mut s, log(1, 8), &client(1), &acquire());

        assert_replays(&first, &second);
        assert_eq!(
            s.resources.get(&key()).cloned(),
            row_after_first,
            "no business effect"
        );
        assert_eq!(
            s.sessions.get(&(ACTOR, CLIENT)).cloned(),
            session_after_first,
            "no session effect"
        );
        assert_eq!(s.audit.len(), 2, "an applied entry always leaves a trace");
        assert_eq!(s.last_applied, Some(log(1, 8)));
        assert!(s.counters_match_recomputation());
    }

    // ── Case 3: delivery at or below `last_applied` is fatal ────────────────

    /// An older index arriving after a newer one is an ordering violation, and
    /// it faults with the entire state unchanged -- the guard runs before the
    /// business decision, not after it.
    #[test]
    fn a_lower_index_faults_with_the_entire_state_unchanged() {
        let mut s = granted();
        apply_ok(&mut s, log(1, 9), &client(1), &acquire());
        let before = s.clone();

        let fault = apply_client_command(&mut s, log(1, 3), &client(2), &acquire())
            .expect_err("an older index cannot belong to a committed history");
        assert_eq!(
            fault,
            Fault::LogIdNotAdvancing {
                delivered: log(1, 3),
                last_applied: log(1, 9),
            }
        );
        assert_state_unchanged(&before, &s);
    }

    /// Re-delivering the entry just applied is equally fatal. §11.3's one
    /// transaction means OpenRaft cannot deliver it again; if it did, the store
    /// and the runtime disagree.
    #[test]
    fn redelivering_the_last_applied_entry_faults_unchanged() {
        let mut s = granted();
        apply_ok(&mut s, log(2, 7), &client(1), &acquire());
        let before = s.clone();

        assert!(apply_client_command(&mut s, log(2, 7), &client(1), &acquire()).is_err());
        assert_state_unchanged(&before, &s);
    }

    /// The same index under a different term is a *different* entry claiming a
    /// position this node has already applied -- two different entries at one
    /// index, which a committed history cannot contain.
    #[test]
    fn the_same_index_under_another_term_faults_unchanged() {
        let mut s = granted();
        apply_ok(&mut s, log(1, 7), &client(1), &acquire());
        let before = s.clone();

        let fault = apply_client_command(&mut s, log(2, 7), &client(1), &acquire())
            .expect_err("one index cannot hold two entries");
        assert_eq!(
            fault,
            Fault::LogIdNotAdvancing {
                delivered: log(2, 7),
                last_applied: log(1, 7),
            }
        );
        assert_state_unchanged(&before, &s);
    }

    /// The same term **and** index under a different leader is still a
    /// different entry. An index-only or `(term, index)` identity cannot
    /// express this, which is why the model's `LogId` carries all three fields
    /// the wire type declares.
    #[test]
    fn the_same_term_and_index_under_another_leader_faults_unchanged() {
        let mut s = granted();
        apply_ok(&mut s, LogId::new(1, LEADER, 7), &client(1), &acquire());
        let before = s.clone();

        let delivered = LogId::new(1, OTHER_LEADER, 7);
        let fault = apply_client_command(&mut s, delivered, &client(1), &acquire())
            .expect_err("a different leader's entry at an applied position");
        assert_eq!(
            fault,
            Fault::LogIdNotAdvancing {
                delivered,
                last_applied: LogId::new(1, LEADER, 7),
            }
        );
        assert_state_unchanged(&before, &s);
    }

    /// The corrupt-state check sees a tip at the *same* index as
    /// `last_applied` but from a different leader -- which a maximum-index scan
    /// cannot distinguish from the ordinary case.
    #[test]
    fn an_audit_tip_from_another_leader_at_the_applied_index_faults() {
        let mut s = granted();
        apply_ok(&mut s, LogId::new(1, LEADER, 7), &client(1), &acquire());
        // Same term and index, different leader: a different entry.
        s.audit_tip = Some(LogId::new(1, OTHER_LEADER, 7));

        assert!(matches!(
            apply_client_command(&mut s, log(1, 9), &client(2), &acquire()),
            Err(Fault::AuditAheadOfLastApplied { .. })
        ));
    }

    /// The stored audit row carries `leader_node_id`, so an entry's full log id
    /// is still reconstructible once the log itself has been purged -- the only
    /// place that information then survives.
    #[test]
    fn a_stored_audit_row_reconstructs_its_entrys_full_log_id() {
        let mut s = granted();
        let id = LogId::new(3, OTHER_LEADER, 11);
        apply_ok(&mut s, id, &client(1), &acquire());

        let ((term, index), row) = s.audit.iter().next().expect("audit row");
        assert_eq!(
            LogId::new(*term, row.leader_node_id, *index),
            id,
            "term and index are the key; the leader comes from the row"
        );
    }

    /// The audit row records the revisions the entry produced, taken from the
    /// saved reply so the two cannot disagree.
    #[test]
    fn the_audit_row_records_the_revisions_the_entry_produced() {
        let mut s = held_state();
        let result = apply_ok(
            &mut s,
            log(1, 5),
            &client(1),
            &Action::Resource(ResourceAction::CompareAndSet {
                fence: Fence {
                    resource: raw(),
                    epoch: 1,
                    holder: ACTOR,
                },
                expected_value_revision: 0,
                new_value: b"v".to_vec(),
            }),
        );

        let row = s.audit.get(&(1, 5)).expect("audit row");
        assert_eq!(
            row.epoch,
            result.reply.snapshot.as_ref().map_or(0, |s| s.epoch)
        );
        assert_eq!(
            row.value_revision,
            result
                .reply
                .snapshot
                .as_ref()
                .map_or(0, |s| s.value_revision)
        );
        assert_eq!(row.value_revision, 1, "the mutation advanced it");
        assert_eq!(row.result, AuditResult::Client(result.reply.outcome));
    }

    /// The case audit-equality could never catch: two **different**
    /// `compare_and_set` commands produce byte-identical audit rows, because the
    /// row carries no value. Under the old audit-as-oracle guard the second
    /// would have been mistaken for a reapplication and its value mutation
    /// would have survived silently.
    #[test]
    fn two_different_mutations_with_identical_audit_projections_cannot_both_apply() {
        let mut s = held_state();

        let first_cas = Action::Resource(ResourceAction::CompareAndSet {
            fence: Fence {
                resource: raw(),
                epoch: 1,
                holder: ACTOR,
            },
            expected_value_revision: 0,
            new_value: b"first".to_vec(),
        });
        let second_cas = Action::Resource(ResourceAction::CompareAndSet {
            fence: Fence {
                resource: raw(),
                epoch: 1,
                holder: ACTOR,
            },
            expected_value_revision: 0,
            new_value: b"second".to_vec(),
        });

        apply_ok(&mut s, log(1, 5), &client(1), &first_cas);
        let audited = s.audit.get(&(1, 5)).cloned().expect("audit row");
        let before = s.clone();

        // Redelivering the same log id with a different command: the audit
        // projections are identical, so equality would have called it a
        // duplicate.
        assert!(apply_client_command(&mut s, log(1, 5), &client(1), &second_cas).is_err());
        assert_state_unchanged(&before, &s);
        assert_eq!(
            s.resources.get(&key()).unwrap().value,
            b"first",
            "the second mutation must not survive"
        );

        // And the projections really are indistinguishable, which is the point.
        let mut probe = granted();
        probe.resources.insert(
            key(),
            ResourceRow {
                epoch: 1,
                held: true,
                holder: Some(ACTOR),
                lease_revision: 1,
                ..Default::default()
            },
        );
        probe.growth_bytes = probe.recompute_growth();
        apply_ok(&mut probe, log(1, 5), &client(1), &second_cas);
        assert_eq!(
            probe.audit.get(&(1, 5)).unwrap(),
            &audited,
            "the two commands audit identically; only the value differs"
        );
    }

    /// The retention limit is proved by exceeding it, not by asserting the
    /// constant. An earlier revision tested pruning with two rows, which cannot
    /// distinguish "prunes at 100,000" from "prunes at 2".
    #[test]
    fn the_audit_table_retains_exactly_the_declared_limit() {
        let mut s = granted();
        s.last_applied = Some(log(1, 0));

        // One row past the cap, each under a different leader so the pruned
        // floor can be checked for the whole identity rather than a term and an
        // index.
        let total = limits::MAX_AUDIT_ROWS + 1;
        // A **fresh** change set per row, which is what a batch is: the row
        // that gets evicted was written in an entry whose transaction committed
        // long ago, so the entry that evicts it has to record the deletion
        // itself. Accumulating one set across the whole run would have hidden
        // that -- the evicted key would still be present from its own
        // insertion, and the assertion below would pass with the eviction
        // record removed. Verified by mutation, twice: the generated histories
        // never reach this path at all, and the accumulating version of this
        // test did not distinguish it either.
        let mut changes = ChangeSet::default();
        for i in 1..=total {
            changes = ChangeSet::default();
            let mut leader = [0u8; 32];
            leader[0..8].copy_from_slice(&i.to_be_bytes());
            write_audit_row(
                &mut s,
                LogId::new(1, leader, i),
                AuditRow {
                    leader_node_id: leader,
                    actor: Some(ACTOR),
                    operation_code: OPERATION_RETIRE_CLIENT,
                    resource: None,
                    result: AuditResult::System(0),
                    epoch: 0,
                    value_revision: 0,
                },
                &mut changes,
            );
        }

        assert_eq!(
            s.audit.len() as u64,
            limits::MAX_AUDIT_ROWS,
            "retention holds exactly the declared limit, not one more"
        );
        assert!(
            !s.audit.contains_key(&(1, 1)),
            "the single oldest row is the one that went"
        );

        // §11.3 writes the pruned row's deletion in the same transaction as the
        // row that displaced it, so pruning has to be *recorded*. A change set
        // that named only insertions would leave the evicted row on disk while
        // memory and the reserve counter both moved on -- and every other
        // assertion in this test would still pass, because they all read
        // memory.
        assert_eq!(
            changes.audit,
            std::collections::BTreeSet::from([(1, 1), (1, total)]),
            "the entry that crosses the cap writes its own row and deletes the \
             one it displaced, and its transaction must name both"
        );
        assert!(s.audit.contains_key(&(1, 2)), "the new oldest is retained");
        assert!(s.audit.contains_key(&(1, total)), "the tip is never pruned");
        assert_eq!(s.audit_tip, Some(LogId::new(1, tip_leader(total), total)));

        // The floor is the whole log id of the oldest **retained** row. A floor
        // that kept only the term and index would name a position that two
        // leaders can both claim, so a client resuming from it cannot tell
        // which history it is resuming.
        assert_eq!(
            s.audit_floor,
            Some(LogId::new(1, tip_leader(2), 2)),
            "the floor carries the retained row's leader, not a zero"
        );
        assert_ne!(
            s.audit_floor.map(|f| f.leader_node_id),
            Some([0u8; 32]),
            "a zeroed leader would be a lost protocol identity, not a value"
        );

        // Reserve accounting unwinds with the rows: pruning that forgot the
        // charge would leave the 128 MiB proof bounding nothing.
        let expected: u64 = s
            .audit
            .values()
            .map(|r| limits::charge_audit(r.resource.as_ref().map(key_parts)))
            .sum();
        assert_eq!(s.reserve_bytes, expected);
    }

    /// A displaced route's deletion is recorded alongside the route that
    /// displaced it.
    ///
    /// Its own test because no history reaches it: the membership driver
    /// re-seeds the same three nodes every round, so the route table never
    /// approaches its 64-row cap, and mutation confirmed the change-set
    /// property passed with this record removed. §9 decides *which* routes are
    /// displaced -- this checks only that whatever it decides is written down.
    #[test]
    fn a_displaced_route_is_recorded_as_well_as_the_one_that_displaced_it() {
        use crate::coordination::model::state::Route;

        let displaced = [0x0Du8; 32];
        let introduced = [0x0Eu8; 32];

        let mut s = State::default();
        s.routes.insert(
            displaced,
            Route {
                ticket: b"old".to_vec(),
                source_operation_id: [1u8; 16],
                introduced_log_id: LogId::new(1, LEADER, 1),
            },
        );
        s.growth_bytes = limits::charge_route(3);

        let mut changes = ChangeSet::default();
        write_intent(
            &mut s,
            LogId::new(1, LEADER, 2),
            membership::IntentEffect {
                operation_id: [2u8; 16],
                intent: intent_fixture(),
                outcome: Outcome::MembershipAccepted,
                evict: Vec::new(),
                routes: vec![(
                    introduced,
                    Route {
                        ticket: b"new".to_vec(),
                        source_operation_id: [2u8; 16],
                        introduced_log_id: LogId::new(1, LEADER, 2),
                    },
                )],
                evict_routes: vec![displaced],
            },
            &mut changes,
        );

        assert!(
            !s.routes.contains_key(&displaced) && s.routes.contains_key(&introduced),
            "the effect must have replaced one route with the other"
        );
        assert_eq!(
            changes.routes,
            std::collections::BTreeSet::from([displaced, introduced]),
            "a change set naming only the new route would leave the displaced \
             one on disk while the growth counter says it is gone"
        );
    }

    /// A minimal active intent, for effects whose subject is not the intent.
    fn intent_fixture() -> crate::coordination::model::state::Intent {
        use crate::coordination::model::state::{Intent, MembershipPlan};
        Intent {
            actor: ACTOR,
            request_hash: [0u8; 32],
            expected_membership: None,
            plan: MembershipPlan::default(),
            status_code: membership::PHASE_PENDING,
            result_code: 0,
            accepted_log_id: LogId::new(1, LEADER, 2),
            completed_log_id: None,
            observed_membership_log_id: None,
        }
    }

    /// The leader stamped on row `i` by the retention fixture.
    fn tip_leader(i: u64) -> NodeId {
        let mut leader = [0u8; 32];
        leader[0..8].copy_from_slice(&i.to_be_bytes());
        leader
    }

    /// A floor observed by a client has to survive the wire unchanged: term,
    /// leader and index. Round-tripping through the §11 mirror is the only
    /// thing that proves the leader is carried rather than reconstructed.
    #[test]
    fn the_audit_floor_round_trips_with_its_full_identity() {
        use crate::coordination::contract::LogIdV1;

        let floor = LogId::new(7, [0x5Au8; 32], 4_294_967_296);
        let wire = LogIdV1::from(&floor);
        assert_eq!(wire.term, 7);
        assert_eq!(wire.leader_node_id, vec![0x5Au8; 32]);
        assert_eq!(wire.index, 4_294_967_296);
        assert_eq!(LogId::try_from(&wire).expect("a well-formed floor"), floor);

        // Same term and index, different leader: two distinct floors that a
        // (term, index) pair alone would conflate.
        let other = LogId::new(7, [0x5Bu8; 32], 4_294_967_296);
        assert_ne!(floor, other);
        assert_ne!(LogIdV1::from(&floor), LogIdV1::from(&other));
    }

    /// Pruning audit rows must not make an old entry deliverable again: the
    /// guard is `last_applied` and the retained tip, neither of which pruning
    /// touches. Prunes the **oldest** row, as audit retention does -- the tip is
    /// never the row that goes.
    #[test]
    fn pruning_audit_cannot_make_an_old_entry_reapplicable() {
        let mut s = granted();
        apply_ok(&mut s, log(1, 7), &client(1), &acquire());
        apply_ok(&mut s, log(1, 8), &client(2), &acquire());

        let oldest = *s.audit.keys().next().expect("an audit row");
        let row = s.audit.remove(&oldest).expect("the oldest row");
        s.reserve_bytes -= limits::charge_audit(row.resource.as_ref().map(key_parts));
        assert_eq!(s.audit.len(), 1, "the tip survives pruning");
        let before = s.clone();

        assert!(apply_client_command(&mut s, log(1, 7), &client(1), &acquire()).is_err());
        assert_state_unchanged(&before, &s);
    }

    /// An audit row for an entry `last_applied` says was never applied means
    /// §11.3's one-transaction rule was violated.
    #[test]
    fn audit_ahead_of_last_applied_faults_as_corrupt_state() {
        let mut s = granted();
        apply_ok(&mut s, log(1, 7), &client(1), &acquire());

        // Simulate a torn write: the audit table committed, `last_applied` did
        // not.
        s.last_applied = Some(log(1, 5));
        let before = s.clone();

        let fault = apply_client_command(&mut s, log(1, 9), &client(2), &acquire())
            .expect_err("audit ahead of last_applied is unreachable state");
        assert_eq!(
            fault,
            Fault::AuditAheadOfLastApplied {
                audit_tip: log(1, 7),
                last_applied: Some(log(1, 5)),
            }
        );
        assert_state_unchanged(&before, &s);
    }

    /// The same check with no applied entry at all: an audit row on a state
    /// that has applied nothing is equally impossible.
    #[test]
    fn audit_with_no_last_applied_faults_as_corrupt_state() {
        let mut s = granted();
        apply_ok(&mut s, log(1, 7), &client(1), &acquire());
        s.last_applied = None;

        assert!(matches!(
            apply_client_command(&mut s, log(1, 9), &client(2), &acquire()),
            Err(Fault::AuditAheadOfLastApplied { .. })
        ));
    }

    #[test]
    fn a_forced_rejection_replays_exactly_as_a_typed_reply() {
        let mut s = held_state();
        let first = apply_ok(&mut s, log(1, 7), &client(1), &oversized_value());

        match preflight(&s, &client(1), &oversized_value()) {
            Admission::Resolved(replayed) => assert_replays(&first, &replayed),
            other => panic!("a consumed sequence must resolve, got {other:?}"),
        }

        let second = apply_ok(&mut s, log(1, 8), &client(1), &oversized_value());
        assert_replays(&first, &second);
    }

    // ── (100, 0) uniqueness ─────────────────────────────────────────────────

    #[test]
    fn only_a_shape_violation_rejects_with_no_reason() {
        let cases: Vec<(&str, State, Action)> = vec![
            (
                "authorization",
                {
                    let mut s = State::default();
                    s.acl.insert((key(), ACTOR), ALL_RIGHTS & !RIGHT_ACQUIRE);
                    s
                },
                acquire(),
            ),
            (
                "already held",
                {
                    let mut s = granted();
                    s.resources.insert(
                        key(),
                        ResourceRow {
                            epoch: 1,
                            held: true,
                            holder: Some([2u8; 32]),
                            ..Default::default()
                        },
                    );
                    s
                },
                acquire(),
            ),
            (
                "value revision",
                held_state(),
                Action::Resource(ResourceAction::CompareAndSet {
                    fence: Fence {
                        resource: raw(),
                        epoch: 1,
                        holder: ACTOR,
                    },
                    expected_value_revision: 99,
                    new_value: b"v".to_vec(),
                }),
            ),
        ];

        for (name, mut s, action) in cases {
            let result = apply_ok(&mut s, log(1, 1), &client(1), &action);
            assert_eq!(result.reply.outcome, Outcome::Rejected, "{name}");
            assert_ne!(
                result.reply.reason,
                Reason::None,
                "{name} must retain its defined coordination reason"
            );
            assert!(!result.is_forced_shape_rejection(), "{name}");
        }
    }

    #[test]
    fn a_successful_outcome_carries_no_reason() {
        let mut s = granted();
        let result = apply_ok(&mut s, log(1, 1), &client(1), &acquire());
        assert_eq!(result.reply.outcome, Outcome::Granted);
        assert_eq!(result.reply.reason, Reason::None);
        assert!(!result.is_forced_shape_rejection());
    }

    // ── Projection ──────────────────────────────────────────────────────────

    #[test]
    fn only_the_four_admission_reasons_are_denied_before_proposal() {
        for refusal in RefusalReason::ALL {
            if *refusal == RefusalReason::PermissionChanged {
                continue;
            }
            let projected = project(ActionDecision::Resource(Decision::Refuse(*refusal)));
            if refusal.is_admission_denial() {
                assert_eq!(
                    projected,
                    Admission::Deny(*refusal),
                    "{refusal:?} is a stage-3 admission denial"
                );
            } else {
                assert_eq!(
                    projected,
                    Admission::Admit,
                    "{refusal:?} must be proposed so it can commit a saved result"
                );
            }
        }
    }

    /// The projection is keyed on the reason, not on the command, so both
    /// command kinds answer identically for the same refusal. Duplicating the
    /// mapping per command is how two of them would come to disagree.
    #[test]
    fn both_command_kinds_project_a_reason_identically() {
        for refusal in RefusalReason::ALL {
            assert_eq!(
                project(ActionDecision::Resource(Decision::Refuse(*refusal))),
                project(ActionDecision::Grant(grant::Decision::Refuse(*refusal))),
                "{refusal:?} must project the same way whichever command produced it"
            );
        }
    }

    /// A `set_grant` no-op is admitted, not answered pre-proposal: it still
    /// commits a saved reply, which is what consumes the sequence.
    #[test]
    fn a_grant_no_op_is_admitted() {
        assert_eq!(
            project(ActionDecision::Grant(grant::Decision::NoOp {
                acl_revision: 3
            })),
            Admission::Admit
        );
    }

    #[test]
    fn a_stale_revision_is_admitted_rather_than_denied() {
        assert_eq!(
            project(ActionDecision::Resource(Decision::Refuse(
                RefusalReason::AclRevisionMismatch
            ))),
            Admission::Admit
        );
    }

    #[test]
    fn a_committed_result_is_returned_even_after_the_grant_is_revoked() {
        let mut s = granted();
        let first = apply_ok(&mut s, log(1, 1), &client(1), &acquire());
        assert_eq!(first.reply.outcome, Outcome::Granted);

        s.acl.remove(&(key(), ACTOR));

        match preflight(&s, &client(1), &acquire()) {
            Admission::Resolved(replayed) => assert_replays(&first, &replayed),
            other => panic!("the committed result must still be reachable, got {other:?}"),
        }
    }

    // ── Ordinary apply ──────────────────────────────────────────────────────

    #[test]
    fn a_successful_acquire_writes_the_row_and_keeps_the_counters_honest() {
        let mut s = granted();
        let result = apply_ok(&mut s, log(1, 3), &client(1), &acquire());

        assert_eq!(result.disposition, Disposition::Consumed);
        assert_eq!(result.reply.outcome, Outcome::Granted);
        assert_eq!(result.reply.snapshot.as_ref().map_or(0, |s| s.epoch), 1);

        let row = s.resources.get(&key()).expect("the row was created");
        assert!(row.held);
        assert_eq!(row.holder, Some(ACTOR));
        assert_eq!(s.last_applied, Some(log(1, 3)));
        assert!(s.counters_match_recomputation());
    }

    #[test]
    fn a_classifier_refusal_writes_audit_but_no_session_row() {
        let mut s = granted();
        let result = apply_ok(&mut s, log(1, 4), &client(5), &acquire());

        assert_eq!(result.disposition, Disposition::NotConsumed);
        assert_eq!(result.reply.reason, Reason::SequenceGap);
        assert!(s.sessions.is_empty(), "no session row is created");
        assert_eq!(s.audit.len(), 1, "the applied entry still leaves a trace");
        assert_eq!(s.last_applied, Some(log(1, 4)));
        assert!(s.counters_match_recomputation());
    }

    /// §5.3.1: a committed `Unknown` is fatal on every voter, not skipped --
    /// and it must fault **before** `last_applied` moves, or this node would
    /// record having applied an entry it cannot interpret.
    #[test]
    fn a_committed_unknown_command_is_fatal_and_records_nothing() {
        let mut s = granted();
        apply_ok(&mut s, log(1, 1), &client(1), &acquire());
        let before = s.clone();

        assert_eq!(
            apply_command(&mut s, log(1, 2), &Command::Unknown),
            Err(Fault::UnknownCommand)
        );
        assert_state_unchanged(&before, &s);
        assert_eq!(
            s.last_applied,
            Some(log(1, 1)),
            "a voter must not record applying what it cannot interpret"
        );
    }

    /// The three command shapes each produce their own reply variant, so a
    /// `disposition` never appears where it has no meaning.
    #[test]
    fn each_command_shape_produces_its_own_reply_variant() {
        let mut s = admin_state();

        assert!(matches!(
            apply_command(
                &mut s,
                log(1, 1),
                &Command::Client {
                    client: client(1),
                    action: grant(ALL_RIGHTS, 0),
                }
            ),
            Ok(CommandReply::Client(_))
        ));

        assert!(matches!(
            apply_command(
                &mut s,
                log(1, 2),
                &Command::Expire(crate::coordination::model::expire::ExpireCommand {
                    resource: key(),
                    epoch: 1,
                    lease_revision: 1,
                })
            ),
            Ok(CommandReply::System(_))
        ));

        assert!(matches!(
            apply_command(
                &mut s,
                log(1, 3),
                &Command::RetireClient(RetireCommand {
                    actor: ACTOR,
                    target_actor: ACTOR,
                    target_client_id: CLIENT,
                })
            ),
            Ok(CommandReply::Retirement(_))
        ));

        assert!(s.counters_match_recomputation());
    }

    /// A retirement is a committed entry like any other: it audits and advances
    /// `last_applied`, even when it changed nothing.
    #[test]
    fn a_retirement_audits_and_advances_last_applied() {
        let mut s = granted();
        let reply = apply_command(
            &mut s,
            log(1, 1),
            &Command::RetireClient(RetireCommand {
                actor: ACTOR,
                target_actor: ACTOR,
                target_client_id: CLIENT,
            }),
        )
        .expect("no fault");

        match reply {
            CommandReply::Retirement(r) => {
                assert_eq!(r.outcome, Outcome::ClientRetired);
                assert!(r.is_terminal());
            }
            other => panic!("expected a retirement reply, got {other:?}"),
        }
        assert_eq!(s.audit.len(), 1);
        assert_eq!(s.last_applied, Some(log(1, 1)));
        assert!(s.counters_match_recomputation());

        // The tombstone closes the id: the classifier now refuses every
        // sequence for it.
        let refused = apply_ok(&mut s, log(1, 2), &client(1), &acquire());
        assert_eq!(refused.reply.reason, Reason::ClientRetired);
    }

    /// A committed retirement retains a saved result, so a retry of the
    /// sequence that already applied still gets its answer -- the one lie the
    /// journal cannot survive.
    #[test]
    fn retirement_preserves_a_committed_result() {
        let mut s = granted();
        let first = apply_ok(&mut s, log(1, 1), &client(1), &acquire());

        apply_command(
            &mut s,
            log(1, 2),
            &Command::RetireClient(RetireCommand {
                actor: ACTOR,
                target_actor: ACTOR,
                target_client_id: CLIENT,
            }),
        )
        .expect("no fault");

        let replayed = apply_ok(&mut s, log(1, 3), &client(1), &acquire());
        assert_replays(&first, &replayed);
    }

    /// An `Expire` is a committed entry with no client and no sequence: it
    /// audits, advances `last_applied`, and reports a system code.
    #[test]
    fn an_expire_applies_as_a_system_command() {
        let mut s = granted();
        apply_ok(&mut s, log(1, 1), &client(1), &acquire());
        let row = s.resources.get(&key()).unwrap().clone();

        let reply = apply_command(
            &mut s,
            log(1, 2),
            &Command::Expire(crate::coordination::model::expire::ExpireCommand {
                resource: key(),
                epoch: row.epoch,
                lease_revision: row.lease_revision,
            }),
        )
        .expect("no fault");

        match reply {
            CommandReply::System(r) => {
                assert_eq!(
                    r.system_result_code,
                    crate::coordination::contract::SYS_EXPIRED
                );
                assert_eq!(r.lease_revision, row.lease_revision + 1);
            }
            other => panic!("expected a system reply, got {other:?}"),
        }

        let after = s.resources.get(&key()).unwrap();
        assert!(!after.held);
        assert_eq!(after.epoch, row.epoch + 1);
        assert_eq!(s.audit.len(), 2);
        assert!(s.counters_match_recomputation());

        // The audit row records the **system phase code**, not a client
        // outcome. Coercing it to `Rejected` -- as an earlier revision did --
        // records a successful expiry as a rejection, and puts two disjoint
        // code spaces in one untagged field.
        let audited = s.audit.get(&(1, 2)).expect("the expiry's audit row");
        assert_eq!(
            audited.result,
            AuditResult::System(crate::coordination::contract::SYS_EXPIRED),
            "a system command audits its phase code"
        );
        assert!(!audited.result.is_client());
        assert_eq!(audited.operation_code, OPERATION_EXPIRE);
    }

    /// A stale expiry audits `2 ExpiryStale`, so the two system results are
    /// distinguishable in the audit trail rather than both reading as failures.
    #[test]
    fn a_stale_expire_audits_its_own_system_code() {
        let mut s = granted();
        apply_ok(&mut s, log(1, 1), &client(1), &acquire());

        apply_command(
            &mut s,
            log(1, 2),
            &Command::Expire(crate::coordination::model::expire::ExpireCommand {
                resource: key(),
                epoch: 99,
                lease_revision: 99,
            }),
        )
        .expect("no fault");

        let audited = s.audit.get(&(1, 2)).expect("the expiry's audit row");
        assert_eq!(
            audited.result,
            AuditResult::System(crate::coordination::contract::SYS_EXPIRY_STALE)
        );
    }

    /// A membership progress report audits its system code too, and a phase
    /// mismatch is distinguishable from an advance.
    #[test]
    fn membership_progress_audits_its_system_code() {
        let mut s = admin_state();
        apply_command(
            &mut s,
            log(1, 1),
            &Command::MembershipProgress(
                crate::coordination::model::membership::MembershipProgressCommand {
                    operation_id: [1u8; 16],
                    expected_phase: 1,
                    new_phase: 2,
                    observed_membership_log_id: None,
                    result_code: crate::coordination::contract::SYS_MEMBERSHIP_ADVANCED,
                },
            ),
        )
        .expect("no fault");

        let audited = s.audit.get(&(1, 1)).expect("the progress audit row");
        assert_eq!(
            audited.result,
            AuditResult::System(crate::coordination::contract::SYS_MEMBERSHIP_PHASE_MISMATCH),
            "no such intent, so the report did not advance anything"
        );
        assert_eq!(audited.operation_code, OPERATION_MEMBERSHIP_PROGRESS);
    }

    /// The audit operation codes, pinned against §5.2's literal declaration
    /// rather than against the constants themselves.
    ///
    /// The history coverage test walks `OPERATION_*`, which moves with any
    /// change to them -- a self-referential oracle. These are the numbers the
    /// contract fixes: "audit operation codes 1-11 are the mutation method
    /// codes", `100` system `Expire`, `101` `MembershipProgress`.
    #[test]
    fn the_audit_operation_codes_match_the_spec_literals() {
        assert_eq!(OPERATION_EXPIRE, 100);
        assert_eq!(OPERATION_MEMBERSHIP_PROGRESS, 101);
        assert_eq!(
            OPERATION_RETIRE_CLIENT, 10,
            "the method table assigns 1-9 and 11, so 10 is what the contract \
             retains for the one mutation deliberately absent from it"
        );
        assert!(
            (1..=11).contains(&OPERATION_RETIRE_CLIENT),
            "codes outside 1-11 are not mutation method codes at all"
        );
    }

    /// And it collides with none of the ten declared methods.
    #[test]
    fn the_retire_code_collides_with_no_declared_method() {
        // §5.2's method table.
        let methods = [1, 2, 3, 4, 5, 6, 7, 8, 9, 11];
        assert!(
            !methods.contains(&OPERATION_RETIRE_CLIENT),
            "retire_client must not reuse a method's code"
        );

        // Every action's operation code is one of those methods.
        let mut seen: Vec<i32> = Vec::new();
        for action in [
            acquire(),
            grant(ALL_RIGHTS, 0),
            Action::Admin(AdminAction {
                target: ACTOR,
                enabled: true,
                expected_admin_revision: 1,
            }),
        ] {
            seen.push(action.operation_code());
        }
        for code in seen {
            assert!(methods.contains(&code), "{code} is not a declared method");
        }
    }

    // ── set_grant through the full apply path ───────────────────────────────

    fn grant(rights: u64, expected_acl_revision: u64) -> Action {
        Action::Grant(GrantAction {
            resource: raw(),
            subject: ACTOR,
            rights,
            expected_acl_revision,
        })
    }

    fn admin_state() -> State {
        let mut s = State::default();
        s.admins.insert(ACTOR);
        s.admin_revision = 1;
        s.growth_bytes = s.recompute_growth();
        s.reserve_bytes = s.recompute_reserve();
        s
    }

    /// A grant on a missing resource writes **both** rows in one applied entry,
    /// and both counters stay honest.
    #[test]
    fn an_applied_grant_writes_both_rows_and_keeps_the_counters_honest() {
        let mut s = admin_state();
        let result = apply_ok(&mut s, log(1, 1), &client(1), &grant(ALL_RIGHTS, 0));

        assert_eq!(result.reply.outcome, Outcome::GrantUpdated);
        assert_eq!(result.reply.acl_revision, 1);
        assert!(s.resources.contains_key(&key()), "the resource row");
        assert_eq!(s.acl.get(&(key(), ACTOR)), Some(&ALL_RIGHTS), "the ACL row");
        assert!(s.counters_match_recomputation());
        assert_eq!(s.audit.get(&(1, 1)).unwrap().operation_code, 7);
    }

    /// Removing a grant frees its growth bytes.
    #[test]
    fn removing_a_grant_frees_its_growth_bytes() {
        let mut s = admin_state();
        apply_ok(&mut s, log(1, 1), &client(1), &grant(ALL_RIGHTS, 0));
        let with_grant = s.growth_bytes;

        apply_ok(&mut s, log(1, 2), &client(2), &grant(0, 1));
        assert!(s.acl.is_empty(), "the grant row is gone");
        assert!(s.resources.contains_key(&key()), "the resource survives");
        assert!(s.growth_bytes < with_grant);
        assert!(s.counters_match_recomputation());
    }

    /// A no-op commits its saved reply -- consuming the sequence -- while
    /// writing no business row and leaving the revision where it was.
    #[test]
    fn an_applied_grant_no_op_consumes_its_sequence_without_changing_state() {
        let mut s = admin_state();
        apply_ok(&mut s, log(1, 1), &client(1), &grant(ALL_RIGHTS, 0));
        let business = (s.resources.clone(), s.acl.clone(), s.growth_bytes);

        let result = apply_ok(&mut s, log(1, 2), &client(2), &grant(ALL_RIGHTS, 1));
        assert_eq!(result.disposition, Disposition::Consumed);
        assert_eq!(result.reply.outcome, Outcome::GrantUpdated);
        assert_eq!(result.reply.acl_revision, 1, "the revision does not move");
        assert_eq!(
            (s.resources.clone(), s.acl.clone(), s.growth_bytes),
            business,
            "a no-op writes no business row"
        );
        // But the sequence is consumed and replays.
        let replayed = apply_ok(&mut s, log(1, 3), &client(2), &grant(ALL_RIGHTS, 1));
        assert_replays(&result, &replayed);
    }

    /// The §8.3 fence lands through the full apply path, not merely in the
    /// decision: a holder stripped of a sustaining right loses its lease in the
    /// same committed entry.
    #[test]
    fn an_applied_grant_fences_the_holder_in_the_same_entry() {
        let mut s = admin_state();
        apply_ok(&mut s, log(1, 1), &client(1), &grant(ALL_RIGHTS, 0));
        apply_ok(&mut s, log(1, 2), &client(2), &acquire());

        let held = s.resources.get(&key()).unwrap().clone();
        assert!(held.held && held.holder == Some(ACTOR));

        // Strip RENEW from the holder.
        let stripped = ALL_RIGHTS & !crate::coordination::model::limits::RIGHT_RENEW;
        apply_ok(&mut s, log(1, 3), &client(3), &grant(stripped, 1));

        let after = s.resources.get(&key()).unwrap();
        assert!(!after.held, "the holder is fenced");
        assert_eq!(after.holder, None);
        assert_eq!(after.epoch, held.epoch + 1);
        assert_eq!(after.lease_revision, held.lease_revision + 1);
        assert_eq!(after.acl_revision, 2);
        assert!(s.counters_match_recomputation());
    }

    /// A non-administrator is a stage-4 status, exactly as for a resource
    /// command -- the projection is shared.
    #[test]
    fn a_non_administrator_grant_is_a_stage_four_status() {
        let s = State::default();
        assert_eq!(
            preflight(&s, &client(1), &grant(ALL_RIGHTS, 0)),
            Admission::Status(StatusDenial::NotAuthorized)
        );
    }

    /// A stale revision is admitted and commits a consumed conflict, rather
    /// than being denied pre-proposal.
    #[test]
    fn a_stale_grant_revision_commits_a_consumed_conflict() {
        let mut s = admin_state();
        apply_ok(&mut s, log(1, 1), &client(1), &grant(ALL_RIGHTS, 0));

        assert_eq!(
            preflight(&s, &client(2), &grant(RIGHT_ACQUIRE, 0)),
            Admission::Admit,
            "a stale revision must reach the log"
        );
        let result = apply_ok(&mut s, log(1, 2), &client(2), &grant(RIGHT_ACQUIRE, 0));
        assert_eq!(result.disposition, Disposition::Consumed);
        assert_eq!(result.reply.reason, Reason::AclRevisionMismatch);
        assert!(result.is_terminal(), "Consumed is terminal");
    }

    #[test]
    fn counters_equal_recomputation_after_a_mixed_history() {
        let mut s = granted();
        let mut index = 0u64;
        let mut sequence = 1u64;

        for step in 0..40u64 {
            index += 1;
            let epoch = s.resources.get(&key()).map_or(0, |r| r.epoch);
            let value_revision = s.resources.get(&key()).map_or(0, |r| r.value_revision);
            let action = match step % 4 {
                0 => acquire(),
                1 => Action::Resource(ResourceAction::CompareAndSet {
                    fence: Fence {
                        resource: raw(),
                        epoch,
                        holder: ACTOR,
                    },
                    expected_value_revision: value_revision,
                    new_value: vec![step as u8; (step % 7) as usize],
                }),
                2 => Action::Resource(ResourceAction::Renew {
                    fence: Fence {
                        resource: raw(),
                        epoch,
                        holder: ACTOR,
                    },
                }),
                _ => Action::Resource(ResourceAction::Release {
                    fence: Fence {
                        resource: raw(),
                        epoch,
                        holder: ACTOR,
                    },
                }),
            };
            let mut c = client(sequence);
            c.request_hash = [step as u8; 32];
            let result = apply_ok(&mut s, log(1, index), &c, &action);
            if result.disposition == Disposition::Consumed {
                sequence += 1;
            }
            assert!(
                s.counters_match_recomputation(),
                "counters drifted at step {step}"
            );
        }
    }
}
