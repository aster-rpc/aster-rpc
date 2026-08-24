//! The deterministic seeded fault schedule (§6.2's first harness).
//!
//! ## What this is, and what it is not
//!
//! §6.2 asks for "an in-process deterministic network with recorded seeds that
//! drops, delays, duplicates, reorders, partitions, and loses leadership at
//! every controller phase". This module is the **schedule** half of that: a
//! reproducible decision function from `(seed, peer, message ordinal)` to a
//! fault, with no clock, no RNG state shared between peers, and no dependence
//! on which order the futures happened to poll.
//!
//! That last property is what makes a seed worth recording. A schedule driven
//! by a shared `rand::rngs::ThreadRng` reproduces nothing: replaying the seed
//! replays the *sequence* of draws, and two runs of an async cluster do not
//! draw in the same order. Keying every decision on facts the caller already
//! has — which peer, which message — makes the schedule a pure function, so a
//! failing seed is a failing seed on any machine.
//!
//! [`ChaosNetwork`] is the other half: the live handle a Raft client consults
//! before every send. The schedule stays a pure function; the handle owns only
//! what a pure function cannot have — the per-peer message ordinal, the current
//! partition epoch, and a tally of what was actually applied. That tally is not
//! decoration: a chaos test whose schedule never fired is a test of a healthy
//! network wearing a costume, and only the counts can tell the two apart.
//!
//! ## Why a hand-rolled hash rather than a crate
//!
//! `rand` is not in the coordination graph and `scripts/check-coordination-deps.sh`
//! exists to keep that graph exact. A splitmix64 over BLAKE3 is deterministic
//! across platforms and architectures — which a `HashMap`'s `DefaultHasher`
//! explicitly is not, and a recorded seed that reproduced only on one machine
//! would be worse than no seed at all.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use crate::coordination::ids::CoordinationNodeId;

/// What the schedule does to one message.
///
/// Every variant is something a real network does. `Duplicate` and `Reorder`
/// are separate from `Delay` because they break different assumptions: a
/// duplicate tests idempotence, a reorder tests that nothing depends on arrival
/// order, and a delay tests only patience.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Fault {
    /// Delivered unchanged. The common case, and the schedule is weighted so it
    /// stays common — a network that drops most messages tests a partition,
    /// not a network.
    Deliver,
    /// Silently lost. The sender learns nothing and must recover by retry.
    Drop,
    /// Delivered late, after `ms`.
    Delay { ms: u64 },
    /// Delivered twice.
    Duplicate,
    /// Held back until a **later** message to the same peer has been sent, so
    /// the two arrive in the opposite order to the one they were issued in.
    Reorder,
}

/// A reproducible fault schedule.
///
/// Cloneable and cheap: every decision is a pure function of the seed and the
/// caller's own facts, so clones agree by construction rather than by sharing
/// state.
#[derive(Clone, Copy, Debug)]
pub struct Schedule {
    seed: u64,
    /// Out of 100. `0` is a perfect network, which is worth being able to
    /// express: a test that fails at zero fault rate has a bug that is not
    /// about the network.
    fault_percent: u8,
    /// Every fault is a [`Fault::Delay`]. See [`Schedule::slow`].
    delays_only: bool,
}

impl Schedule {
    /// A schedule with a recorded seed and fault rate.
    pub fn new(seed: u64, fault_percent: u8) -> Self {
        Self {
            seed,
            fault_percent: fault_percent.min(100),
            delays_only: false,
        }
    }

    /// A perfect network. Every message is delivered unchanged.
    pub fn perfect() -> Self {
        Self::new(0, 0)
    }

    /// A **slow** network: every message is delayed, none is lost.
    ///
    /// For a gate that has to *observe* a state the group passes through rather
    /// than survive a faulty network. On loopback the interval between two
    /// consecutive committed entries can be under a millisecond, which is not
    /// long enough to sample reliably — and a gate that missed it would time
    /// out claiming the group never reached a state it passed straight through.
    ///
    /// Delays and nothing else, on purpose: a drop rate would produce elections
    /// this kind of gate has not asked for, and a gate about a *placed* leader
    /// loss cannot tell its own failure from the schedule's. Every delay stays
    /// under `election_timeout_min`, so a slow network does not become a
    /// partition and leadership does not move.
    pub fn slow(seed: u64) -> Self {
        Self {
            seed,
            fault_percent: 100,
            delays_only: true,
        }
    }

    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// What happens to the `ordinal`-th message this node sends to `peer`.
    ///
    /// Deterministic in all three inputs and nothing else — no clock, no
    /// counter shared between peers, no dependence on poll order. Two runs of
    /// the same seed make the same decisions even if the futures interleave
    /// differently, which is the only way a recorded seed is worth recording.
    pub(crate) fn fault_for(&self, peer: &CoordinationNodeId, ordinal: u64) -> Fault {
        if self.fault_percent == 0 {
            return Fault::Deliver;
        }
        let roll = self.draw(peer, ordinal, 0) % 100;
        if roll >= u64::from(self.fault_percent) {
            return Fault::Deliver;
        }
        if self.delays_only {
            return Fault::Delay {
                ms: 1 + self.draw(peer, ordinal, 2) % 200,
            };
        }
        // Within the faulty share, spread across the four kinds. Not uniform by
        // accident: a drop is the fault a real network produces most, and
        // over-weighting reorder would make the schedule mostly a shuffle.
        match self.draw(peer, ordinal, 1) % 8 {
            0..=3 => Fault::Drop,
            4..=5 => Fault::Delay {
                // Bounded well under `election_timeout_min` so a delay is a
                // delay rather than a disguised partition — those are different
                // faults and the schedule should not conflate them.
                ms: 1 + self.draw(peer, ordinal, 2) % 200,
            },
            6 => Fault::Duplicate,
            _ => Fault::Reorder,
        }
    }

    /// Whether `a` and `b` are on the same side of a partition at `epoch`.
    ///
    /// Partition is separate from per-message faults because it is a property
    /// of a *pair* over a *period*, not of a message: dropping each message
    /// independently at 50% is not a partition, and a test that used one for
    /// the other would never see a stable minority.
    /// **Epoch 0 is a whole network.** Partitions are opt-in: a harness that
    /// merely installed a schedule would otherwise start every run with the
    /// group already split, which conflates the two faults it is trying to
    /// tell apart — and a gate about drops and delays would silently be a gate
    /// about partitions. Advancing the epoch is what asks for one.
    pub(crate) fn same_side(
        &self,
        a: &CoordinationNodeId,
        b: &CoordinationNodeId,
        epoch: u64,
    ) -> bool {
        if self.fault_percent == 0 || epoch == 0 {
            return true;
        }
        // Order-independent: a partition that separated A from B but not B from
        // A would be a one-way link, which is a different fault.
        let (low, high) = if a <= b { (a, b) } else { (b, a) };
        self.side(low, epoch) == self.side(high, epoch)
    }

    /// Whether `a` and `b` can reach each other at `epoch`, for a caller
    /// outside the crate.
    ///
    /// A gate that means to prove "a partition healed" has to be able to *ask*
    /// which epochs split and which do not. Choosing an epoch and hoping it
    /// splits is how the first version of that gate came to advance into one
    /// epoch, commit on whichever side led, and call it a heal.
    pub fn reaches(&self, a: &CoordinationNodeId, b: &CoordinationNodeId, epoch: u64) -> bool {
        self.same_side(a, b, epoch)
    }

    fn side(&self, node: &CoordinationNodeId, epoch: u64) -> bool {
        self.draw(node, epoch, 3).is_multiple_of(2)
    }

    /// splitmix64 over BLAKE3 of the inputs.
    ///
    /// BLAKE3 because it is already in the graph and is stable across
    /// platforms; splitmix64 to spread the digest's low bits, since the modulo
    /// above reads only those.
    fn draw(&self, node: &CoordinationNodeId, ordinal: u64, salt: u8) -> u64 {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&self.seed.to_le_bytes());
        hasher.update(node.as_bytes());
        hasher.update(&ordinal.to_le_bytes());
        hasher.update(&[salt]);
        let digest = hasher.finalize();
        let mut x = u64::from_le_bytes(digest.as_bytes()[..8].try_into().expect("8 bytes"));
        x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// How many times each kind of fault was actually applied.
///
/// The non-vacuity oracle. A seed and a fault rate describe what *should*
/// happen; this records what did, so a test can require that the run it just
/// passed contained real drops and real delays rather than a lucky sequence of
/// `Deliver`s.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FaultTally {
    pub delivered: u64,
    pub dropped: u64,
    pub delayed: u64,
    pub duplicated: u64,
    /// Messages that were held and **actually overtaken**: a later message to
    /// the same peer was sent, and completed, while this one was still waiting.
    ///
    /// Only a genuine overtake counts. The first version of this injector slept
    /// for a fixed interval and incremented this before the sleep, so the number
    /// recorded how often the schedule had said the word "reorder" — not once
    /// did it record two messages changing places, which is the only thing a
    /// reorder is.
    pub reordered: u64,
    /// Messages held for a reorder that no later message ever overtook, and
    /// which were therefore delivered late instead.
    ///
    /// Counted apart from [`delayed`](Self::delayed) so the two are never
    /// confused, and reported so a run cannot claim reordering it did not do:
    /// a request/response client sends to one peer from one task at a time, so
    /// an overtake needs a *second* concurrent sender to that peer — an
    /// election running alongside replication, or a replication stream being
    /// replaced across terms. A run with a high count here and a low
    /// `reordered` met few such moments.
    pub reorder_expired: u64,
    pub partitioned: u64,
}

impl FaultTally {
    /// Every message the schedule touched, however it ended up.
    pub fn total(&self) -> u64 {
        self.delivered
            + self.dropped
            + self.delayed
            + self.duplicated
            + self.reordered
            + self.reorder_expired
            + self.partitioned
    }

    /// Whether every kind the schedule can produce was produced at least once.
    ///
    /// What a test asserts instead of trusting the fault rate: the rate is a
    /// property of the schedule, this is a property of the run. `reordered`
    /// here means an *observed overtake*, so this cannot be satisfied by a
    /// schedule that merely selected the verdict.
    pub fn every_kind_fired(&self) -> bool {
        self.dropped > 0 && self.delayed > 0 && self.duplicated > 0 && self.reordered > 0
    }
}

/// One message held back, waiting to be overtaken.
struct Held {
    /// The send token this hold was issued at. Only a send taken out **after**
    /// this one may release it — a message that started earlier and merely
    /// finished later is this one's predecessor, not its successor, and
    /// counting that as an overtake would inflate the tally with orderings that
    /// never changed.
    token: u64,
    wake: Arc<Notify>,
}

#[derive(Default)]
struct ChaosState {
    /// Next message ordinal per peer. The schedule's second input, and the one
    /// thing it cannot derive: "which message is this" is inherently stateful.
    ordinals: BTreeMap<CoordinationNodeId, u64>,
    /// The current partition epoch. A test advances it to re-draw the sides;
    /// within one epoch the split is stable, which is what makes a minority
    /// stay a minority long enough to observe.
    epoch: u64,
    /// Nodes cut off from every peer by an explicit instruction rather than by
    /// the seed.
    ///
    /// Separate from the epoch partition because it answers a different
    /// question. The epoch asks "what does the group do when the network splits
    /// somewhere"; this asks "what does the group do when *this* node is lost,
    /// *now*" — which is the only way to place a leader loss at a chosen
    /// boundary rather than wherever the schedule happened to put one.
    isolated: BTreeSet<CoordinationNodeId>,
    /// Messages held for a reorder, per peer.
    held: BTreeMap<CoordinationNodeId, Vec<Held>>,
    /// Monotonic send token, issued by `before_send` and quoted back by
    /// `after_send`.
    next_token: u64,
    tally: FaultTally,
}

/// The live fault injector a Raft client consults before each send.
///
/// Shared by every per-peer client of one voter, so ordinals and the epoch are
/// per-*voter* rather than per-connection: a client rebuilt mid-run must not
/// restart the schedule, or a flapping peer would silently get a fresh draw.
#[derive(Debug)]
pub struct ChaosNetwork {
    schedule: Schedule,
    state: Mutex<ChaosState>,
}

impl std::fmt::Debug for ChaosState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChaosState")
            .field("epoch", &self.epoch)
            .field("isolated", &self.isolated.len())
            .field("tally", &self.tally)
            .finish()
    }
}

/// What the client should do with the message it is about to send.
///
/// Deliberately not [`Fault`]: this is the *action*, after the partition check
/// has been folded in and after a delay has already been waited out. The client
/// should not have to re-derive policy from a schedule verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChaosAction {
    /// Send once, normally.
    Send,
    /// Send twice and keep the first answer.
    SendTwice,
    /// Do not send. The caller reports an unreachable peer, which is what a
    /// dropped request looks like from the sender's side.
    Swallow,
    /// Send this message **later**, detached, once a message issued after it
    /// has already gone — and answer the caller as though it were lost.
    ///
    /// This is what a reorder has to be for a request/response client. Holding
    /// the caller cannot produce one: the caller *is* the peer's only sender,
    /// so a held message blocks its own successor and the two can never change
    /// places. Detaching it does produce one — the peer really receives this
    /// message after the next, which is the arrival order the receiver has to
    /// tolerate — and the sender sees what a sender always sees when a reply
    /// never comes.
    ///
    /// The caller must arrange the late send and then report it with
    /// [`after_send`](ChaosNetwork::after_send), or abandon it with
    /// [`abandon`](ChaosNetwork::abandon).
    Defer,
}

/// What to do with one message, and the token that identifies this send.
///
/// The token is not decoration: [`after_send`](ChaosNetwork::after_send) quotes
/// it back so the injector can tell a message's *successor* from a message that
/// merely started earlier and finished late. Only the former is an overtake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ChaosSend {
    pub(crate) action: ChaosAction,
    pub(crate) token: u64,
}

impl ChaosNetwork {
    pub fn new(schedule: Schedule) -> Arc<Self> {
        Arc::new(Self {
            schedule,
            state: Mutex::new(ChaosState::default()),
        })
    }

    pub fn seed(&self) -> u64 {
        self.schedule.seed()
    }

    /// The schedule this injector runs, so a gate can ask it which epochs split
    /// and which do not before choosing one.
    pub fn schedule(&self) -> Schedule {
        self.schedule
    }

    pub fn tally(&self) -> FaultTally {
        self.locked().tally
    }

    /// Re-draw the partition. Within an epoch the sides are fixed; advancing it
    /// heals one split and creates another.
    pub fn advance_epoch(&self) {
        self.locked().epoch += 1;
    }

    /// Move to a **named** epoch.
    ///
    /// A gate proving a heal has to reach an epoch that genuinely connects
    /// every pair, and the schedule can say which those are — stepping forward
    /// one at a time and hoping is what made the earlier partition gate never
    /// actually heal.
    pub fn set_epoch(&self, epoch: u64) {
        self.locked().epoch = epoch;
    }

    pub fn epoch(&self) -> u64 {
        self.locked().epoch
    }

    /// Cut `node` off from every peer, in both directions, until
    /// [`rejoin`](Self::rejoin).
    ///
    /// Set on **every** voter's injector or the cut is one-way, which is a
    /// different fault from a lost node and one no consensus protocol promises
    /// anything about. Unlike the epoch partition this is not drawn from the
    /// seed: it exists so a gate can place a leader loss at a boundary it
    /// chose, and a chosen boundary cannot come out of a hash.
    pub fn isolate(&self, node: CoordinationNodeId) {
        self.locked().isolated.insert(node);
    }

    /// Undo [`isolate`](Self::isolate), so the node can be shown to rejoin and
    /// catch up.
    pub fn rejoin(&self, node: &CoordinationNodeId) {
        self.locked().isolated.remove(node);
    }

    /// Decide what happens to the next message from `local` to `peer`, living
    /// out a delay or a reorder hold before returning.
    ///
    /// `async` because a delay must be *lived*, not reported: returning "delay
    /// by 40ms" and trusting the caller to honour it is one more place for the
    /// harness and the schedule to disagree.
    pub(crate) async fn before_send(
        &self,
        local: &CoordinationNodeId,
        peer: &CoordinationNodeId,
    ) -> ChaosSend {
        enum Verdict {
            Send,
            SendTwice,
            Swallow,
            Delay(u64),
            Defer,
        }

        let (verdict, token) = {
            let mut state = self.locked();
            let token = state.next_token;
            state.next_token += 1;
            let ordinal = {
                let slot = state.ordinals.entry(*peer).or_insert(0);
                let current = *slot;
                *slot += 1;
                current
            };
            let epoch = state.epoch;
            // An explicit isolation and the epoch partition both win over the
            // per-message draw. A pair that cannot reach each other drops
            // *everything* between them, which is the difference between a
            // partition and a high drop rate.
            let cut = state.isolated.contains(local) || state.isolated.contains(peer);
            let partitioned = cut || !self.schedule.same_side(local, peer, epoch);
            let verdict = if partitioned {
                state.tally.partitioned += 1;
                Verdict::Swallow
            } else {
                match self.schedule.fault_for(peer, ordinal) {
                    Fault::Deliver => {
                        state.tally.delivered += 1;
                        Verdict::Send
                    }
                    Fault::Drop => {
                        state.tally.dropped += 1;
                        Verdict::Swallow
                    }
                    Fault::Delay { ms } => {
                        state.tally.delayed += 1;
                        Verdict::Delay(ms)
                    }
                    Fault::Duplicate => {
                        state.tally.duplicated += 1;
                        Verdict::SendTwice
                    }
                    // Counted in `await_release`, not here: whether this became
                    // a reorder or a late delivery is not known until something
                    // either overtakes it or does not.
                    Fault::Reorder => {
                        state.held.entry(*peer).or_default().push(Held {
                            token,
                            wake: Arc::new(Notify::new()),
                        });
                        Verdict::Defer
                    }
                }
            };
            (verdict, token)
        };

        let action = match verdict {
            Verdict::Send => ChaosAction::Send,
            Verdict::SendTwice => ChaosAction::SendTwice,
            Verdict::Swallow => ChaosAction::Swallow,
            Verdict::Delay(ms) => {
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                ChaosAction::Send
            }
            Verdict::Defer => ChaosAction::Defer,
        };
        ChaosSend { action, token }
    }

    /// Wait until the message held at `token` for `peer` has been overtaken, or
    /// the ceiling expires — then tally which of the two happened.
    ///
    /// Called from the caller's *detached* send, never from the path that owns
    /// the peer's turn. Holding the owning caller here is what made the first
    /// implementation impossible to satisfy: the held message blocked the only
    /// task that could have produced its successor, so it was always the
    /// ceiling that fired and never an overtake.
    pub(crate) async fn await_release(&self, peer: &CoordinationNodeId, token: u64) {
        let wake = {
            let state = self.locked();
            match state
                .held
                .get(peer)
                .and_then(|waiting| waiting.iter().find(|held| held.token == token))
            {
                Some(held) => Arc::clone(&held.wake),
                // Already released or abandoned; nothing to wait for.
                None => return,
            }
        };
        let overtaken = tokio::time::timeout(
            std::time::Duration::from_millis(REORDER_CEILING_MS),
            wake.notified(),
        )
        .await
        .is_ok();

        let mut state = self.locked();
        if let Some(waiting) = state.held.get_mut(peer) {
            waiting.retain(|held| held.token != token);
        }
        if overtaken {
            state.tally.reordered += 1;
        } else {
            state.tally.reorder_expired += 1;
        }
    }

    /// Give up on a held message: it will not be sent late after all.
    ///
    /// For a caller that cannot detach one — a snapshot stream, whose receiver
    /// refuses a second concurrent transfer by design, so a late second copy
    /// would be testing that refusal rather than arrival order. Counted as a
    /// drop, which is what it becomes.
    pub(crate) fn abandon(&self, peer: &CoordinationNodeId, token: u64) {
        let mut state = self.locked();
        if let Some(waiting) = state.held.get_mut(peer) {
            waiting.retain(|held| held.token != token);
        }
        state.tally.dropped += 1;
    }

    /// Report that the send taken out at `token` has finished.
    ///
    /// Releases every message held for `peer` that was issued **before** this
    /// one, because each of those has now been overtaken: a later message was
    /// sent and answered while they waited.
    ///
    /// Called on the real send only. A swallowed message never reached the
    /// peer, so it cannot have overtaken anything.
    pub(crate) fn after_send(&self, peer: &CoordinationNodeId, token: u64) {
        let released: Vec<Arc<Notify>> = {
            let mut state = self.locked();
            let Some(waiting) = state.held.get_mut(peer) else {
                return;
            };
            let mut released = Vec::new();
            waiting.retain(|held| {
                if held.token < token {
                    released.push(Arc::clone(&held.wake));
                    false
                } else {
                    true
                }
            });
            released
        };
        for wake in released {
            wake.notify_one();
        }
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, ChaosState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// How long a deferred message waits to be overtaken before it is delivered
/// late instead.
///
/// Bounded on **both** sides by facts about the pinned OpenRaft, and neither
/// bound is cosmetic:
///
/// - It must exceed OpenRaft's `Unreachable` backoff, which
///   `RaftNetwork::backoff`'s default fixes at 500 ms. A deferred message is
///   answered as unreachable, so its peer's next message is that retry — and a
///   ceiling under the backoff means the successor arrives *after* the ceiling
///   has already expired, so nothing can ever overtake anything. At 250 ms this
///   is exactly what happened: live runs produced `reordered: 0` and
///   `reorder_expired` for every single deferral.
/// - It must stay under `election_timeout_min` (1500 ms), or a deferral becomes
///   a disguised partition rather than a reorder.
///
/// Both bounds are asserted **at compile time** below, because a constant whose
/// value is load-bearing and whose reason lives only in a comment is a constant
/// somebody will "tidy" — and the tidied version does not fail a test, it
/// quietly stops observing reorders.
const REORDER_CEILING_MS: u64 = 1_200;

/// OpenRaft's default `RaftNetwork::backoff`, which this crate does not
/// override: `Backoff::new(std::iter::repeat(Duration::from_millis(500)))`.
const OPENRAFT_UNREACHABLE_BACKOFF_MS: u64 = 500;

// A ceiling at or under the unreachable backoff can never see an overtake: the
// successor is the retry, and the retry is what the backoff is delaying. A
// ceiling past the minimum election timeout makes a deferral a partition rather
// than a reorder. Neither is a runtime property, so neither is a test.
const _: () = assert!(REORDER_CEILING_MS > OPENRAFT_UNREACHABLE_BACKOFF_MS);
const _: () =
    assert!(REORDER_CEILING_MS < crate::coordination::runtime::config::ELECTION_TIMEOUT_MIN_MS);

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn node(n: u8) -> CoordinationNodeId {
        CoordinationNodeId::from_array([n; 32])
    }

    /// **A recorded seed reproduces**, and does so independently of order.
    ///
    /// The property that makes a seed worth recording at all. Asked in a
    /// different order the second time, so a schedule carrying hidden
    /// per-call state — a counter, an RNG cursor — would diverge.
    #[test]
    fn a_seed_reproduces_regardless_of_the_order_it_is_asked() {
        let schedule = Schedule::new(0xDEAD_BEEF, 30);

        let forward: Vec<Fault> = (0..200)
            .map(|i| schedule.fault_for(&node((i % 5) as u8), i))
            .collect();

        // The same questions, backwards, on a fresh clone.
        let clone = schedule;
        let mut backward: Vec<Fault> = (0..200)
            .rev()
            .map(|i| clone.fault_for(&node((i % 5) as u8), i))
            .collect();
        backward.reverse();

        assert_eq!(
            forward, backward,
            "a schedule with hidden per-call state would answer differently when asked \
             in another order, and its seed would reproduce nothing"
        );
    }

    /// A different seed gives a different schedule, so the seed is doing work.
    #[test]
    fn a_different_seed_gives_a_different_schedule() {
        let a = Schedule::new(1, 30);
        let b = Schedule::new(2, 30);
        let differ = (0..200)
            .filter(|i| a.fault_for(&node(1), *i) != b.fault_for(&node(1), *i))
            .count();
        assert!(
            differ > 20,
            "two seeds must disagree often; they differed on {differ}/200"
        );
    }

    /// A zero fault rate is a perfect network.
    ///
    /// Worth being able to express: a test that fails at zero has a bug that is
    /// not about the network, and being unable to say so would make every
    /// failure ambiguous.
    #[test]
    fn a_zero_fault_rate_delivers_everything() {
        let perfect = Schedule::perfect();
        for i in 0..500 {
            assert_eq!(perfect.fault_for(&node((i % 7) as u8), i), Fault::Deliver);
        }
        assert!(perfect.same_side(&node(1), &node(2), 0));
    }

    /// The fault rate is honoured, and every kind is actually produced.
    ///
    /// A schedule that only ever dropped would satisfy "faults happen" while
    /// testing one third of what §6.2 asks for.
    #[test]
    fn the_schedule_produces_every_kind_at_about_the_declared_rate() {
        let schedule = Schedule::new(0x5EED, 40);
        let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
        const N: u64 = 4_000;
        for i in 0..N {
            let kind = match schedule.fault_for(&node((i % 5) as u8), i) {
                Fault::Deliver => "deliver",
                Fault::Drop => "drop",
                Fault::Delay { ms } => {
                    assert!((1..=200).contains(&ms), "a delay must stay bounded: {ms}");
                    "delay"
                }
                Fault::Duplicate => "duplicate",
                Fault::Reorder => "reorder",
            };
            *counts.entry(kind).or_default() += 1;
        }

        for kind in ["deliver", "drop", "delay", "duplicate", "reorder"] {
            assert!(
                counts.get(kind).copied().unwrap_or(0) > 0,
                "the schedule never produced {kind}: {counts:?}"
            );
        }

        // The declared rate, within a wide band — this is a hash, not a proof
        // of uniformity, and a tight bound would be a flaky test about BLAKE3.
        let faulty = N as usize - counts["deliver"];
        let rate = faulty * 100 / N as usize;
        assert!(
            (25..=55).contains(&rate),
            "a declared 40% fault rate produced {rate}%: {counts:?}"
        );
    }

    /// **A partition is symmetric**, because a one-way link is a different
    /// fault.
    #[test]
    fn a_partition_separates_pairs_symmetrically() {
        let schedule = Schedule::new(0xBEEF, 50);
        for epoch in 1..51 {
            for a in 1..6u8 {
                for b in 1..6u8 {
                    assert_eq!(
                        schedule.same_side(&node(a), &node(b), epoch),
                        schedule.same_side(&node(b), &node(a), epoch),
                        "partition must be symmetric for {a}/{b} at {epoch}"
                    );
                }
            }
        }
    }

    /// A node is always on its own side, and a partition really does split.
    ///
    /// The second half matters: a `same_side` that always answered `true` would
    /// pass the symmetry test above and describe no partition at all.
    #[test]
    fn a_partition_actually_splits_the_group() {
        let schedule = Schedule::new(0xC0FFEE, 50);
        for epoch in 0..20 {
            for n in 1..6u8 {
                assert!(schedule.same_side(&node(n), &node(n), epoch));
            }
        }

        let split = (1..200).any(|epoch| {
            (1..6u8).any(|a| (1..6u8).any(|b| !schedule.same_side(&node(a), &node(b), epoch)))
        });
        assert!(split, "the schedule never separated any pair");

        // And the split is not permanent: a partition that never healed would
        // be a dead group rather than a network under fault.
        let healed = (1..200).any(|epoch| {
            (1..6u8).all(|a| (1..6u8).all(|b| schedule.same_side(&node(a), &node(b), epoch)))
        });
        assert!(healed, "the schedule never healed a partition");
    }

    /// **Epoch 0 partitions nobody.**
    ///
    /// Asserted rather than assumed, because the first chaos run built on this
    /// module started every group already split and its message-level gate was
    /// therefore measuring a partition.
    #[test]
    fn epoch_zero_is_a_whole_network() {
        let schedule = Schedule::new(0xC0FFEE, 50);
        for a in 1..8u8 {
            for b in 1..8u8 {
                assert!(
                    schedule.same_side(&node(a), &node(b), 0),
                    "epoch 0 must connect every pair; {a}/{b} was split"
                );
            }
        }
    }

    /// A schedule whose first message to `peer` is a reorder, whose second is
    /// one that gets sent, and whose first message to `other` is too.
    ///
    /// Searched rather than hard-coded: the draw is a hash, so the seed that
    /// produces this shape is not something to guess at, and a hard-coded one
    /// would silently stop meaning this if the mixing ever changed.
    fn reordering_schedule(peer: &CoordinationNodeId, other: &CoordinationNodeId) -> Schedule {
        let sends = |fault: Fault| !matches!(fault, Fault::Drop | Fault::Reorder);
        (0..100_000u64)
            .map(|seed| Schedule::new(seed, 100))
            .find(|s| {
                s.fault_for(peer, 0) == Fault::Reorder
                    && sends(s.fault_for(peer, 1))
                    && sends(s.fault_for(other, 0))
            })
            .expect("some seed holds the first message and sends the next")
    }

    /// **A deferred message is released by a message issued after it**, and
    /// only then is it counted as a reorder.
    ///
    /// The property the tally's `reordered` field claims. Its first
    /// implementation slept for a fixed interval and counted every hold, so the
    /// number said how often the schedule had *chosen* to reorder and never
    /// once said that two messages had changed places. Its second held the
    /// caller, which cannot work: the caller is the peer's only sender, so the
    /// held message blocked its own successor and a live run produced
    /// `reordered: 0` every time.
    #[tokio::test(start_paused = true)]
    async fn a_deferred_message_is_released_by_a_later_send_to_the_same_peer() {
        let (local, peer) = (node(1), node(2));
        let chaos = ChaosNetwork::new(reordering_schedule(&peer, &node(3)));

        let deferred = chaos.before_send(&local, &peer).await;
        assert_eq!(
            deferred.action,
            ChaosAction::Defer,
            "the caller is answered at once and sends this one late"
        );
        let waiting = tokio::spawn({
            let chaos = Arc::clone(&chaos);
            async move { chaos.await_release(&peer, deferred.token).await }
        });
        tokio::task::yield_now().await;
        assert_eq!(
            chaos.tally().reordered,
            0,
            "nothing has overtaken it yet, so nothing is a reorder yet"
        );

        // A later send to the same peer, which goes while the first waits.
        let successor = chaos.before_send(&local, &peer).await;
        assert_ne!(
            successor.action,
            ChaosAction::Swallow,
            "the successor has to actually go, or it overtakes nothing"
        );
        assert!(
            deferred.token < successor.token,
            "the deferred message was issued first, which is what makes this an overtake"
        );
        chaos.after_send(&peer, successor.token);

        waiting.await.expect("the deferred send is released");
        let tally = chaos.tally();
        assert_eq!(
            (tally.reordered, tally.reorder_expired),
            (1, 0),
            "an overtaken message is a reorder and not a late delivery: {tally:?}"
        );
    }

    /// A deferred message nothing overtakes is a **late delivery**, and is
    /// counted as one.
    ///
    /// The honest half of the pair: a peer that never gets another message from
    /// this node cannot have anything change places, and a tally that called
    /// that a reorder would be claiming an ordering that never changed.
    #[tokio::test(start_paused = true)]
    async fn a_deferred_message_that_nothing_overtakes_is_delivered_late() {
        let (local, peer) = (node(1), node(2));
        let chaos = ChaosNetwork::new(reordering_schedule(&peer, &node(3)));

        let deferred = chaos.before_send(&local, &peer).await;
        assert_eq!(deferred.action, ChaosAction::Defer);

        let began = tokio::time::Instant::now();
        chaos.await_release(&peer, deferred.token).await;
        assert!(
            began.elapsed() >= std::time::Duration::from_millis(REORDER_CEILING_MS),
            "an unovertaken message waits out the ceiling before giving up"
        );

        let tally = chaos.tally();
        assert_eq!(
            (tally.reordered, tally.reorder_expired),
            (0, 1),
            "nothing overtook it, so it is a late delivery: {tally:?}"
        );
    }

    /// **A message that started earlier does not overtake one deferred later.**
    ///
    /// Without the token comparison any completing send would release a
    /// deferred message, including its own predecessor — and the tally would
    /// then count orderings that never changed, which is the exact failure the
    /// whole mechanism exists to avoid.
    #[tokio::test(start_paused = true)]
    async fn a_send_issued_before_the_deferral_does_not_overtake_it() {
        let (local, peer, other) = (node(1), node(2), node(3));
        let chaos = ChaosNetwork::new(reordering_schedule(&peer, &other));

        // Taken out first, so its token is lower than the deferred one's.
        let earlier = chaos.before_send(&local, &other).await;
        assert_ne!(earlier.action, ChaosAction::Swallow);

        let deferred = chaos.before_send(&local, &peer).await;
        assert_eq!(deferred.action, ChaosAction::Defer);
        let waiting = tokio::spawn({
            let chaos = Arc::clone(&chaos);
            async move { chaos.await_release(&peer, deferred.token).await }
        });
        tokio::task::yield_now().await;

        // The earlier send finishes late, and aims at the deferred peer.
        chaos.after_send(&peer, earlier.token);
        tokio::task::yield_now().await;
        assert_eq!(
            chaos.tally().reordered,
            0,
            "a predecessor finishing late is not an overtake"
        );

        // And it still resolves, as a late delivery.
        waiting.await.expect("the deferred send resolves");
        let tally = chaos.tally();
        assert_eq!(
            (tally.reordered, tally.reorder_expired),
            (0, 1),
            "{tally:?}"
        );
    }

    /// **No delay the schedule can draw outruns the reorder ceiling.**
    ///
    /// The third bound on `REORDER_CEILING_MS`, and the only one of the three
    /// that is not a comparison of constants — the other two are asserted where
    /// the constant is defined, at compile time, because a relationship that
    /// can be checked by the compiler should not be able to reach a test run.
    /// The delay is drawn from a hash, so this one has to be sampled: a delay
    /// longer than the hold would land a "delayed" message after a "reordered"
    /// one, and the two kinds would stop being distinguishable in the tally.
    #[test]
    fn no_scheduled_delay_outruns_the_reorder_ceiling() {
        let schedule = Schedule::new(0xD1A1, 100);
        for i in 0..4_000u64 {
            if let Fault::Delay { ms } = schedule.fault_for(&node((i % 7) as u8), i) {
                assert!(
                    ms < REORDER_CEILING_MS,
                    "a delay of {ms}ms outruns the hold"
                );
            }
        }
    }

    /// An abandoned deferral is a drop, and is counted as one.
    #[tokio::test(start_paused = true)]
    async fn an_abandoned_deferral_is_a_drop() {
        let (local, peer) = (node(1), node(2));
        let chaos = ChaosNetwork::new(reordering_schedule(&peer, &node(3)));

        let deferred = chaos.before_send(&local, &peer).await;
        assert_eq!(deferred.action, ChaosAction::Defer);
        chaos.abandon(&peer, deferred.token);

        // And waiting on it afterwards finds nothing, rather than blocking for
        // the ceiling and counting the same message a second time.
        chaos.await_release(&peer, deferred.token).await;
        let tally = chaos.tally();
        assert_eq!(
            (tally.dropped, tally.reordered, tally.reorder_expired),
            (1, 0, 0),
            "{tally:?}"
        );
    }

    /// **An isolated node reaches nobody, and nobody reaches it.**
    ///
    /// Symmetric by construction when every voter is told, which is what makes
    /// it a lost node rather than a one-way link. Proven against a *perfect*
    /// schedule so nothing but the isolation can swallow a message.
    #[tokio::test(start_paused = true)]
    async fn an_isolated_node_reaches_nobody_and_nobody_reaches_it() {
        let (lost, a, b) = (node(1), node(2), node(3));
        let chaos = ChaosNetwork::new(Schedule::perfect());

        assert_eq!(
            chaos.before_send(&lost, &a).await.action,
            ChaosAction::Send,
            "a perfect schedule delivers until something is isolated"
        );

        chaos.isolate(lost);
        assert_eq!(
            chaos.before_send(&lost, &a).await.action,
            ChaosAction::Swallow,
            "an isolated node cannot send"
        );
        assert_eq!(
            chaos.before_send(&a, &lost).await.action,
            ChaosAction::Swallow,
            "and cannot be sent to"
        );
        assert_eq!(
            chaos.before_send(&a, &b).await.action,
            ChaosAction::Send,
            "everyone else is untouched"
        );

        chaos.rejoin(&lost);
        assert_eq!(
            chaos.before_send(&a, &lost).await.action,
            ChaosAction::Send,
            "and the cut is reversible, so a rejoin can be observed"
        );
        let tally = chaos.tally();
        assert_eq!(
            tally.partitioned, 2,
            "an isolated pair is counted as partitioned, not dropped: {tally:?}"
        );
    }

    /// Two peers at the same ordinal get independent decisions.
    ///
    /// A schedule keyed only on the ordinal would fault every peer in lockstep,
    /// which is a global outage rather than a network.
    #[test]
    fn peers_are_scheduled_independently() {
        let schedule = Schedule::new(0xF00D, 50);
        let differ = (0..200)
            .filter(|i| schedule.fault_for(&node(1), *i) != schedule.fault_for(&node(2), *i))
            .count();
        assert!(
            differ > 40,
            "two peers at the same ordinal must not fault in lockstep; they differed on \
             {differ}/200"
        );
    }
}
