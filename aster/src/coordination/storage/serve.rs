//! Targeted reads over one [`GuardedRead`] transaction, for the Store service
//! (§9.3, S4-STATUS-CUT).
//!
//! Every function here takes the read **transaction**, so what it observes is
//! the pinned version the caller already proved is at or past its barrier —
//! never the actor's in-memory state, which can be newer than any outstanding
//! transaction and is the one substitution the guarded-read API exists to
//! prevent.
//!
//! These are point and bounded-range lookups, not loads: [`super::rows::load`]
//! materializes the whole state and exists for startup. A read that scanned
//! every session row to answer one probe would make the hot path pay for the
//! cold one.

use std::collections::BTreeMap;

use redb::ReadableTable as _;

use super::codec::decode_persisted_bounded;
use super::error::{Db, StorageError, StorageResult};
use super::rows::{
    read_meta, MAX_AUDIT_ROW_BYTES, MAX_INTENT_ROW_BYTES, MAX_RESOURCE_ROW_BYTES,
    MAX_SESSION_ROW_BYTES,
};
use super::schema::{ACL, ADMINS, AUDIT, INTENTS, RESOURCES, SESSIONS};
use crate::coordination::model::limits;
use crate::coordination::model::state::{
    key_parts, AuditRow, Intent, LogId, OperationId, ResourceKey, ResourceRow, Session,
};
use crate::coordination::model::wire::{intent_from_wire, session_from_wire};
use crate::coordination::raft::CoordinationMetaV1;

fn state_err(source: impl std::error::Error + Send + Sync + 'static) -> StorageError {
    StorageError::redb(Db::State, source)
}

fn corrupt(what: &'static str, detail: String) -> StorageError {
    StorageError::corrupt(Db::State, what, detail)
}

/// The transaction's `CoordinationMetaV1` — profile, membership, revisions,
/// audit floor, budgets — decoded exactly as the loader decodes it.
pub(crate) fn meta(read: &redb::ReadTransaction) -> StorageResult<CoordinationMetaV1> {
    read_meta(read)
}

/// Is `actor` a group administrator at this version (§8.1)?
///
/// The one predicate, evaluated against the transaction:
/// `admin_revision > 0 && (actor == ROOT || actor ∈ group_admins)`. The meta is
/// passed in rather than re-read so a handler that already holds it cannot
/// accidentally pair the admin table of one version with the meta of another.
pub(crate) fn is_administrator(
    read: &redb::ReadTransaction,
    meta: &CoordinationMetaV1,
    actor: &[u8; 32],
) -> StorageResult<bool> {
    if meta.admin_revision == 0 {
        return Ok(false);
    }
    if meta.authority_public_key.as_slice() == actor {
        return Ok(true);
    }
    let table = read.open_table(ADMINS).map_err(state_err)?;
    Ok(table.get(*actor).map_err(state_err)?.is_some())
}

/// The exact delegated administrator set, sorted by key order.
pub(crate) fn administrators(read: &redb::ReadTransaction) -> StorageResult<Vec<[u8; 32]>> {
    let table = read.open_table(ADMINS).map_err(state_err)?;
    let mut out = Vec::new();
    for entry in table.iter().map_err(state_err)? {
        let (key, _) = entry.map_err(state_err)?;
        out.push(key.value());
    }
    Ok(out)
}

/// One resource row, or `None` when it has never been written.
pub(crate) fn resource(
    read: &redb::ReadTransaction,
    key: &ResourceKey,
) -> StorageResult<Option<ResourceRow>> {
    let (namespace, raw) = key_parts(key);
    let table = read.open_table(RESOURCES).map_err(state_err)?;
    let Some(value) = table.get((namespace, raw)).map_err(state_err)? else {
        return Ok(None);
    };
    let wire: crate::coordination::raft::ResourceRowV1 = decode_persisted_bounded(
        Db::State,
        "ResourceRowV1",
        value.value(),
        MAX_RESOURCE_ROW_BYTES,
    )?;
    let row =
        ResourceRow::try_from(&wire).map_err(|err| corrupt("ResourceRowV1", format!("{err:?}")))?;
    Ok(Some(row))
}

/// The rights `subject` holds on `key`, or `None` when no grant row exists.
pub(crate) fn rights_of(
    read: &redb::ReadTransaction,
    key: &ResourceKey,
    subject: &[u8; 32],
) -> StorageResult<Option<u64>> {
    let (namespace, raw) = key_parts(key);
    let table = read.open_table(ACL).map_err(state_err)?;
    let Some(value) = table.get((namespace, raw, *subject)).map_err(state_err)? else {
        return Ok(None);
    };
    let rights = value.value();
    if !limits::rights_are_known(rights) {
        return Err(corrupt(
            "resource_acl.rights",
            format!("rights {rights:#x} include a bit v1 does not define"),
        ));
    }
    Ok(Some(rights))
}

/// Every grant on `key`, in subject order.
pub(crate) fn grants_of(
    read: &redb::ReadTransaction,
    key: &ResourceKey,
) -> StorageResult<Vec<([u8; 32], u64)>> {
    let (namespace, raw) = key_parts(key);
    let table = read.open_table(ACL).map_err(state_err)?;
    let mut out = Vec::new();
    let range = table
        .range((namespace, raw, [0u8; 32])..=(namespace, raw, [0xFF; 32]))
        .map_err(state_err)?;
    for entry in range {
        let (row_key, value) = entry.map_err(state_err)?;
        let (_, _, subject) = row_key.value();
        let rights = value.value();
        if !limits::rights_are_known(rights) {
            return Err(corrupt(
                "resource_acl.rights",
                format!("rights {rights:#x} include a bit v1 does not define"),
            ));
        }
        out.push((subject, rights));
    }
    Ok(out)
}

/// One client session row, or `None`.
pub(crate) fn session(
    read: &redb::ReadTransaction,
    meta: &CoordinationMetaV1,
    actor: &[u8; 32],
    client_id: &[u8; 16],
) -> StorageResult<Option<Session>> {
    let table = read.open_table(SESSIONS).map_err(state_err)?;
    let Some(value) = table.get((*actor, *client_id)).map_err(state_err)? else {
        return Ok(None);
    };
    let wire: crate::coordination::raft::ClientSessionV1 = decode_persisted_bounded(
        Db::State,
        "ClientSessionV1",
        value.value(),
        MAX_SESSION_ROW_BYTES,
    )?;
    let row = session_from_wire(&meta.group_id, &wire)
        .map_err(|err| corrupt("ClientSessionV1", format!("{err:?}")))?;
    Ok(Some(row))
}

/// One membership intent by operation id, or `None`.
pub(crate) fn intent(
    read: &redb::ReadTransaction,
    operation_id: &OperationId,
) -> StorageResult<Option<Intent>> {
    let table = read.open_table(INTENTS).map_err(state_err)?;
    let Some(value) = table.get(*operation_id).map_err(state_err)? else {
        return Ok(None);
    };
    decode_intent(operation_id, value.value()).map(Some)
}

/// The one active (non-terminal) intent at this version, or `None`.
pub(crate) fn active_intent(
    read: &redb::ReadTransaction,
) -> StorageResult<Option<(OperationId, Intent)>> {
    let table = read.open_table(INTENTS).map_err(state_err)?;
    for entry in table.iter().map_err(state_err)? {
        let (key, value) = entry.map_err(state_err)?;
        let operation_id = key.value();
        let row = decode_intent(&operation_id, value.value())?;
        if !crate::coordination::model::membership::is_terminal_phase(row.status_code) {
            return Ok(Some((operation_id, row)));
        }
    }
    Ok(None)
}

fn decode_intent(operation_id: &OperationId, bytes: &[u8]) -> StorageResult<Intent> {
    let wire: crate::coordination::raft::MembershipIntentV1 =
        decode_persisted_bounded(Db::State, "MembershipIntentV1", bytes, MAX_INTENT_ROW_BYTES)?;
    intent_from_wire(operation_id, &wire)
        .map_err(|err| corrupt("MembershipIntentV1", format!("{err:?}")))
}

/// Retained audit rows strictly after `after`, ascending by index, at most
/// `limit`.
///
/// The table is keyed `(term, index)` — redb orders by term first — so the
/// rows are collected and ordered by **index**, which is log order in any
/// valid history; `check_audit_key_order` refuses a store where the two
/// orderings disagree.
pub(crate) fn audit_after(
    read: &redb::ReadTransaction,
    after: Option<&LogId>,
    limit: usize,
) -> StorageResult<Vec<((u64, u64), AuditRow)>> {
    let table = read.open_table(AUDIT).map_err(state_err)?;
    let mut rows: BTreeMap<u64, ((u64, u64), AuditRow)> = BTreeMap::new();
    let floor_index = after.map(|id| id.index);
    for entry in table.iter().map_err(state_err)? {
        let (key, value) = entry.map_err(state_err)?;
        let (term, index) = key.value();
        if let Some(floor) = floor_index {
            if index <= floor {
                continue;
            }
        }
        let wire: crate::coordination::rowstream::AuditRowV1 =
            decode_persisted_bounded(Db::State, "AuditRowV1", value.value(), MAX_AUDIT_ROW_BYTES)?;
        let row =
            AuditRow::try_from(&wire).map_err(|err| corrupt("AuditRowV1", format!("{err:?}")))?;
        rows.insert(index, ((term, index), row));
        // Keep the map bounded: only the lowest `limit` indexes can be in the
        // answer, so anything beyond that is dropped as soon as it is proven
        // not to be among them.
        while rows.len() > limit {
            rows.pop_last();
        }
    }
    Ok(rows.into_values().collect())
}

/// Every **held** resource row: `(key, epoch, lease_revision, ttl_ms)`.
///
/// The §9.2 expiry sweep's input. A full-table scan per sweep is a measured
/// cost, not an accident: rows do not persist a grant instant — apply reads no
/// clock — so the leader's timers are reconstructed from the held set, and the
/// sweep interval bounds how often this runs. Slice 5's benchmark pass owns
/// the numbers.
pub(crate) fn held_rows(
    read: &redb::ReadTransaction,
) -> StorageResult<Vec<(ResourceKey, u64, u64, u64)>> {
    let table = read.open_table(RESOURCES).map_err(state_err)?;
    let mut out = Vec::new();
    for entry in table.iter().map_err(state_err)? {
        let (key, value) = entry.map_err(state_err)?;
        let (namespace, raw) = key.value();
        let wire: crate::coordination::raft::ResourceRowV1 = decode_persisted_bounded(
            Db::State,
            "ResourceRowV1",
            value.value(),
            super::rows::MAX_RESOURCE_ROW_BYTES,
        )?;
        if !wire.held {
            continue;
        }
        let resource = ResourceKey::new(namespace, raw.to_vec())
            .map_err(|err| corrupt("resources.key", format!("{err:?}")))?;
        out.push((resource, wire.epoch, wire.lease_revision, wire.ttl_ms));
    }
    Ok(out)
}
