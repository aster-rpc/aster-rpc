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

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

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
    /// Held back and delivered after the next message to the same peer.
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
}

impl Schedule {
    /// A schedule with a recorded seed and fault rate.
    pub fn new(seed: u64, fault_percent: u8) -> Self {
        Self {
            seed,
            fault_percent: fault_percent.min(100),
        }
    }

    /// A perfect network. Every message is delivered unchanged.
    pub fn perfect() -> Self {
        Self::new(0, 0)
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
    pub reordered: u64,
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
            + self.partitioned
    }

    /// Whether every kind the schedule can produce was produced at least once.
    ///
    /// What a test asserts instead of trusting the fault rate: the rate is a
    /// property of the schedule, this is a property of the run.
    pub fn every_kind_fired(&self) -> bool {
        self.dropped > 0 && self.delayed > 0 && self.duplicated > 0 && self.reordered > 0
    }
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

    pub fn tally(&self) -> FaultTally {
        self.locked().tally
    }

    /// Re-draw the partition. Within an epoch the sides are fixed; advancing it
    /// heals one split and creates another.
    pub fn advance_epoch(&self) {
        self.locked().epoch += 1;
    }

    pub fn epoch(&self) -> u64 {
        self.locked().epoch
    }

    /// Decide what happens to the next message from `local` to `peer`, waiting
    /// out a delay before returning if the schedule called for one.
    ///
    /// `async` because a delay must be *lived*, not reported: returning "delay
    /// by 40ms" and trusting the caller to honour it is one more place for the
    /// harness and the schedule to disagree.
    pub(crate) async fn before_send(
        &self,
        local: &CoordinationNodeId,
        peer: &CoordinationNodeId,
    ) -> ChaosAction {
        let (fault, partitioned) = {
            let mut state = self.locked();
            let ordinal = {
                let slot = state.ordinals.entry(*peer).or_insert(0);
                let current = *slot;
                *slot += 1;
                current
            };
            let epoch = state.epoch;
            // Partition wins over the per-message draw. A pair on opposite
            // sides drops *everything* between them for the whole epoch, which
            // is the difference between a partition and a high drop rate.
            let partitioned = !self.schedule.same_side(local, peer, epoch);
            let fault = if partitioned {
                Fault::Drop
            } else {
                self.schedule.fault_for(peer, ordinal)
            };
            match (partitioned, fault) {
                (true, _) => state.tally.partitioned += 1,
                (false, Fault::Deliver) => state.tally.delivered += 1,
                (false, Fault::Drop) => state.tally.dropped += 1,
                (false, Fault::Delay { .. }) => state.tally.delayed += 1,
                (false, Fault::Duplicate) => state.tally.duplicated += 1,
                (false, Fault::Reorder) => state.tally.reordered += 1,
            }
            (fault, partitioned)
        };

        if partitioned {
            return ChaosAction::Swallow;
        }
        match fault {
            Fault::Deliver => ChaosAction::Send,
            Fault::Drop => ChaosAction::Swallow,
            Fault::Delay { ms } => {
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                ChaosAction::Send
            }
            Fault::Duplicate => ChaosAction::SendTwice,
            // A request/response client has no queue to shuffle, so "arrives
            // after the next one" is expressed the only way it can be: hold
            // this message longer than the next one will take. The bound is the
            // schedule's own delay ceiling, so a reorder cannot become a
            // disguised partition any more than a delay can.
            Fault::Reorder => {
                tokio::time::sleep(std::time::Duration::from_millis(REORDER_HOLD_MS)).await;
                ChaosAction::Send
            }
        }
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, ChaosState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// How long a reordered message is held. Above the delay ceiling so it lands
/// after a normally-delayed successor, and still well under
/// `election_timeout_min`.
const REORDER_HOLD_MS: u64 = 250;

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
