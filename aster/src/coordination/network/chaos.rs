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
//! It is **not** a network. Wiring it into a live cluster needs an injection
//! point in the Raft client, which is where the remaining §6.2 work is; the
//! schedule is separable and is what a reader has to trust either way.
//!
//! ## Why a hand-rolled hash rather than a crate
//!
//! `rand` is not in the coordination graph and `scripts/check-coordination-deps.sh`
//! exists to keep that graph exact. A splitmix64 over BLAKE3 is deterministic
//! across platforms and architectures — which a `HashMap`'s `DefaultHasher`
//! explicitly is not, and a recorded seed that reproduced only on one machine
//! would be worse than no seed at all.

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
pub(crate) struct Schedule {
    seed: u64,
    /// Out of 100. `0` is a perfect network, which is worth being able to
    /// express: a test that fails at zero fault rate has a bug that is not
    /// about the network.
    fault_percent: u8,
}

impl Schedule {
    /// A schedule with a recorded seed and fault rate.
    pub(crate) fn new(seed: u64, fault_percent: u8) -> Self {
        Self {
            seed,
            fault_percent: fault_percent.min(100),
        }
    }

    /// A perfect network. Every message is delivered unchanged.
    pub(crate) fn perfect() -> Self {
        Self::new(0, 0)
    }

    pub(crate) fn seed(&self) -> u64 {
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
    pub(crate) fn same_side(
        &self,
        a: &CoordinationNodeId,
        b: &CoordinationNodeId,
        epoch: u64,
    ) -> bool {
        if self.fault_percent == 0 {
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
        for epoch in 0..50 {
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

        let split = (0..200).any(|epoch| {
            (1..6u8).any(|a| (1..6u8).any(|b| !schedule.same_side(&node(a), &node(b), epoch)))
        });
        assert!(split, "the schedule never separated any pair");

        // And the split is not permanent: a partition that never healed would
        // be a dead group rather than a network under fault.
        let healed = (0..200).any(|epoch| {
            (1..6u8).all(|a| (1..6u8).all(|b| schedule.same_side(&node(a), &node(b), epoch)))
        });
        assert!(healed, "the schedule never healed a partition");
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
