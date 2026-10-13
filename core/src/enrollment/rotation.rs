//! Root pre-rotation for an installed enrollment service. A consumer pins its
//! root together with a commitment to the next root key ([`RotationPin`]).
//! A rotation statement signed by the committed key replaces the root; the
//! statement is self-certifying, so any replica can deliver it. Applied
//! statements are journaled beside the fleet store so a restart cannot
//! revert them (see [`crate::delegation::verify_root_rotation`]).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::store::write_atomic;
use super::RecordError;
use crate::delegation::{
    claimed_rotation_seq, verify_applied_rotation, verify_root_rotation, RootRotation, RootState,
};

/// Applied statements kept in the journal and offered to peers in the
/// enrollment handshake. A peer further behind than this re-pins out of
/// band.
pub const MAX_FORWARDED_ROTATIONS: usize = 8;

/// What a consumer pins about its root beyond the root key itself, which the
/// policy store and verifier already carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RotationPin {
    /// `next_root_commitment(R1)` for the next root key R1.
    pub next_commitment: [u8; 32],
    /// Rotations already applied to the pinned root; 0 for an original root.
    pub seq: u64,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct JournalFile {
    version: u32,
    /// Applied statements, oldest first.
    statements: Vec<String>,
}

/// The applied statements for one fleet store path.
pub(super) struct RotationJournal {
    path: PathBuf,
    statements: Vec<String>,
}

impl RotationJournal {
    pub(super) fn load(path: PathBuf) -> Result<Self, RecordError> {
        let statements = match std::fs::read(&path) {
            Ok(bytes) => {
                if bytes.len() > MAX_FORWARDED_ROTATIONS * 16 * 1024 {
                    return Err(RecordError::TooManyRecords);
                }
                let file: JournalFile = serde_json::from_slice(&bytes)
                    .map_err(|error| RecordError::Storage(error.to_string()))?;
                if file.version != 1 {
                    return Err(RecordError::UnsupportedVersion);
                }
                file.statements
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(RecordError::Storage(error.to_string())),
        };
        Ok(Self { path, statements })
    }

    /// The statements a peer may need, oldest first.
    pub(super) fn statements(&self) -> &[String] {
        &self.statements
    }

    /// Apply every journaled statement that continues `state`, in sequence
    /// order. Statements at or below the pinned sequence are already part of
    /// the pin; one that does not verify ends the replay (it belongs to
    /// another trust history, such as one the consumer has since re-pinned
    /// away from).
    pub(super) fn replay(&self, mut state: RootState, now: u64, leeway: u64) -> Vec<RootRotation> {
        let mut applied = Vec::new();
        for statement in &self.statements {
            if claimed_rotation_seq(statement).is_none_or(|seq| seq <= state.seq) {
                continue;
            }
            match verify_root_rotation(statement, &state, now, leeway) {
                Ok(rotation) => {
                    state = rotation.apply();
                    applied.push(rotation);
                }
                Err(error) => {
                    tracing::warn!(
                        ?error,
                        "enrollment: journaled root rotation does not continue the pinned root"
                    );
                    break;
                }
            }
        }
        applied
    }

    /// Record an applied statement durably before it takes effect.
    pub(super) fn append(&mut self, statement: &str) -> Result<(), RecordError> {
        self.merge(std::slice::from_ref(&statement.to_owned()))
    }

    /// Add verified statements of the pinned history that this node applied
    /// out of band, so it relays them to peers that are further behind.
    /// Statements stay ordered by sequence and capped at the latest
    /// [`MAX_FORWARDED_ROTATIONS`].
    pub(super) fn seed(&mut self, history: &[String]) -> Result<(), RecordError> {
        self.merge(history)
    }

    fn merge(&mut self, added: &[String]) -> Result<(), RecordError> {
        let mut statements = self.statements.clone();
        for statement in added {
            if !statements.iter().any(|known| known == statement) {
                statements.push(statement.clone());
            }
        }
        if statements == self.statements {
            return Ok(());
        }
        // Journaled statements were verified when applied or seeded, so
        // their claimed sequence is the verified one.
        statements.sort_by_key(|statement| claimed_rotation_seq(statement).unwrap_or(0));
        if statements.len() > MAX_FORWARDED_ROTATIONS {
            statements.drain(..statements.len() - MAX_FORWARDED_ROTATIONS);
        }
        let bytes = serde_json::to_vec(&JournalFile {
            version: 1,
            statements: statements.clone(),
        })
        .map_err(|error| RecordError::Storage(error.to_string()))?;
        write_atomic(&self.path, &bytes)?;
        self.statements = statements;
        Ok(())
    }
}

/// Verify statements a consumer says produced its pinned root, walking back
/// from the pin: the statement at the pinned sequence must yield the pin,
/// the one before it that statement's predecessor, and so on. The walk ends
/// at the first missing sequence (older history may be omitted). Any
/// supplied statement that is not part of the verified walk is an error,
/// because an operator handed this node something that is not its history.
pub(super) fn verified_history(
    history: &[String],
    pinned: RootState,
    now: u64,
    leeway: u64,
) -> Result<Vec<String>, RecordError> {
    let mut remaining: Vec<&String> = history.iter().collect();
    let mut verified = Vec::new();
    let mut after = pinned;
    while after.seq > 0 {
        let Some(index) = remaining
            .iter()
            .position(|statement| claimed_rotation_seq(statement) == Some(after.seq))
        else {
            break;
        };
        let statement = remaining.remove(index);
        let rotation = verify_applied_rotation(statement, &after, now, leeway)
            .map_err(|_| RecordError::InvalidRecord)?;
        after = RootState {
            root: *rotation.previous_root(),
            // The state before a rotation committed to the root it produced.
            next_commitment: crate::delegation::next_root_commitment(&after.root),
            seq: after.seq - 1,
        };
        verified.push(statement.clone());
    }
    if !remaining.is_empty() {
        return Err(RecordError::InvalidRecord);
    }
    verified.reverse();
    Ok(verified)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delegation::{issue_root_rotation, next_root_commitment, RotationReason};

    fn public(seed: u8) -> [u8; 32] {
        crate::keys::public_key(&[seed; 32])
    }
    fn statement(signer: u8, prev: u8, seq: u64, next: u8) -> String {
        issue_root_rotation(
            &[signer; 32],
            &public(prev),
            seq,
            &next_root_commitment(&public(next)),
            &[[seq as u8; 32]],
            1,
            RotationReason::Routine,
        )
        .unwrap()
    }
    fn scratch(name: &str) -> PathBuf {
        let mut random = [0; 8];
        getrandom::getrandom(&mut random).unwrap();
        std::env::temp_dir().join(format!("aster-rotation-{}-{name}", hex::encode(random)))
    }
    fn pinned(root: u8, next: u8, seq: u64) -> RootState {
        RootState {
            root: public(root),
            next_commitment: next_root_commitment(&public(next)),
            seq,
        }
    }

    /// Journaled rotations survive reopening and replay from any pin on the
    /// same history: earlier statements are part of the pin, and a statement
    /// from another history ends the replay.
    #[test]
    fn journal_replays_from_any_pin_on_its_history() {
        let path = scratch("fleet.json.rotations");
        let mut journal = RotationJournal::load(path.clone()).unwrap();
        journal.append(&statement(2, 1, 1, 3)).unwrap();
        journal.append(&statement(3, 2, 2, 4)).unwrap();
        journal.append(&statement(3, 2, 2, 4)).unwrap();
        let journal = RotationJournal::load(path.clone()).unwrap();
        let _ = std::fs::remove_file(path);
        assert_eq!(journal.statements().len(), 2);

        let from_original = journal.replay(pinned(1, 2, 0), 10, 0);
        assert_eq!(from_original.len(), 2);
        assert_eq!(from_original[1].apply(), pinned(3, 4, 2));
        // A consumer that re-pinned to the first rotation replays the rest.
        let from_first = journal.replay(pinned(2, 3, 1), 10, 0);
        assert_eq!(from_first.len(), 1);
        assert_eq!(from_first[0].apply(), pinned(3, 4, 2));
        // An unrelated root replays nothing.
        assert!(journal.replay(pinned(9, 8, 0), 10, 0).is_empty());
    }

    #[test]
    fn journal_keeps_only_the_latest_forwarded_rotations() {
        let path = scratch("journal");
        let mut journal = RotationJournal::load(path.clone()).unwrap();
        for seq in 1..=(MAX_FORWARDED_ROTATIONS as u64 + 2) {
            let signer = seq as u8 + 1;
            journal
                .append(&statement(signer, signer - 1, seq, signer + 1))
                .unwrap();
        }
        assert_eq!(journal.statements().len(), MAX_FORWARDED_ROTATIONS);
        assert_eq!(claimed_rotation_seq(&journal.statements()[0]), Some(3));
        let _ = std::fs::remove_file(path);
    }

    /// Out-of-band history verifies back from the pin: full or truncated
    /// history is accepted in sequence order, and anything that is not the
    /// pinned root's history is refused.
    #[test]
    fn applied_history_is_verified_back_from_the_pin() {
        // R1 -> R2 (seq 1, signed by 2), R2 -> R3 (seq 2, signed by 3).
        let first = statement(2, 1, 1, 3);
        let second = statement(3, 2, 2, 4);
        let pin = pinned(3, 4, 2);
        assert_eq!(
            verified_history(&[second.clone(), first.clone()], pin, 10, 0).unwrap(),
            vec![first.clone(), second.clone()]
        );
        // Older history may be omitted.
        assert_eq!(
            verified_history(std::slice::from_ref(&second), pin, 10, 0).unwrap(),
            vec![second.clone()]
        );
        // A statement that did not produce this pin is an error, as is one
        // left over below a gap.
        assert!(verified_history(std::slice::from_ref(&first), pin, 10, 0).is_err());
        assert!(verified_history(&[statement(9, 1, 2, 4)], pin, 10, 0).is_err());
        // Seeding keeps the journal ordered by sequence.
        let path = scratch("seeded");
        let mut journal = RotationJournal::load(path.clone()).unwrap();
        journal.append(&second).unwrap();
        journal.seed(std::slice::from_ref(&first)).unwrap();
        assert_eq!(journal.statements(), [first, second]);
        let _ = std::fs::remove_file(path);
    }
}
