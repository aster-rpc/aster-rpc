//! The §9.2 leader expiry loop (S4-F).
//!
//! ## Timers are leader-local; expiry is a committed command
//!
//! `apply` reads no clock, so a row does not know when it was granted. The
//! **current leader** owns the timers: it reconstructs them from the held
//! rows, arms each with its row's **full committed TTL**, and on fire proposes
//! `Expire(resource, epoch, lease_revision)` — an ordinary committed command
//! every voter applies identically. A stale expiry (the row moved since the
//! timer armed) commits as a harmless applied rejection; §9.2 says so, and
//! that is what lets this loop be conservative rather than clever.
//!
//! ## Election, restart, or a gap resets every timer to a full TTL
//!
//! A new generation starts with an empty timer map, so every held row gets a
//! fresh full TTL. That may delay a takeover — never overlap one: the
//! holder's self-limit expires from *its* send instant, so a holder that
//! cannot renew stops presenting its fence before this conservative timer
//! could hand the resource to a successor.
//!
//! ## The timer identity is `(epoch, lease_revision)`
//!
//! A renew advances `lease_revision`, so the row's identity changes and the
//! timer re-arms for the full TTL — which is exactly what a renewal means. An
//! unchanged identity keeps its deadline; a vanished row drops its timer.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use openraft::Raft;

use crate::coordination::contract::{CommandV1, ExpireCommandV1, ResourceKeyV1};
use crate::coordination::ids::ResourceKey;
use crate::coordination::storage::machine::StateHandle;
use crate::coordination::storage::serve;
use crate::coordination::typeconfig::CoordinationTypeConfig;

/// How often the leader sweeps. Comfortably under the §12.1 minimum TTL of
/// 5,000 ms, so no lease can expire between sweeps by more than one interval.
const SWEEP_INTERVAL: Duration = Duration::from_millis(1_000);

/// One leader generation's expiry loop. Ends when the generation counter
/// moves, when a proposal answers `ForwardToLeader`, or when storage stops.
pub(crate) async fn run_generation(
    raft: Raft<CoordinationTypeConfig>,
    state: StateHandle,
    my_generation: u64,
    generation: Arc<AtomicU64>,
) {
    let mut deadlines: BTreeMap<ResourceKey, (u64, u64, Instant)> = BTreeMap::new();
    loop {
        tokio::time::sleep(SWEEP_INTERVAL).await;
        if generation.load(Ordering::SeqCst) != my_generation {
            return;
        }

        // The held set, from a snapshot-consistent transaction.
        let Ok(guard) = state.read_transaction().await else {
            // The store is stopping or fatal; there is nothing safe to expire.
            return;
        };
        let Ok(held) = serve::held_rows(&guard) else {
            return;
        };
        drop(guard);

        let now = Instant::now();
        // Timers for rows that are gone or no longer held are dropped, so a
        // released resource cannot fire a stale timer forever.
        let held_keys: std::collections::BTreeSet<&ResourceKey> =
            held.iter().map(|(key, ..)| key).collect();
        deadlines.retain(|key, _| held_keys.contains(key));

        for (key, epoch, lease_revision, ttl_ms) in &held {
            let fresh = (
                *epoch,
                *lease_revision,
                now + Duration::from_millis(*ttl_ms),
            );
            match deadlines.get(key) {
                // The identity is unchanged: the armed timer stands.
                Some((armed_epoch, armed_revision, _))
                    if armed_epoch == epoch && armed_revision == lease_revision => {}
                // New, renewed, or re-acquired: a full fresh TTL.
                _ => {
                    deadlines.insert(key.clone(), fresh);
                }
            }
        }

        for (key, (epoch, lease_revision, deadline)) in deadlines.clone() {
            if now < deadline {
                continue;
            }
            if generation.load(Ordering::SeqCst) != my_generation {
                return;
            }
            let command = CommandV1::Expire(ExpireCommandV1 {
                resource: ResourceKeyV1 {
                    namespace: key.namespace().to_string(),
                    key: key.key().to_vec(),
                },
                epoch,
                lease_revision,
            });
            match raft.client_write(command).await {
                Ok(_) => {
                    // Applied — as an expiry or as a harmless stale rejection;
                    // either way this timer's work is done and the next sweep
                    // re-reads the row.
                    deadlines.remove(&key);
                }
                Err(err) => {
                    // A forward means this generation no longer leads; anything
                    // else is retried by the next sweep with the same timer.
                    use openraft::error::ClientWriteError;
                    if matches!(err.api_error(), Some(ClientWriteError::ForwardToLeader(_))) {
                        return;
                    }
                }
            }
        }
    }
}
