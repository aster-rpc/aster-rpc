//! The only writers of the replicated tables (§11.3).
//!
//! ## Why these exist rather than a review rule
//!
//! Every mutation of a replicated table has to be recorded in the entry's
//! [`ChangeSet`], because §11.3 writes exactly the recorded rows in that
//! entry's transaction. A row mutated in memory and left out of the set is
//! never written: memory moves on, disk does not, and a restart resurrects the
//! old value.
//!
//! That is not a failure any local test notices, because the model is right
//! either way — and it happened. `apply_expire` cleared an expired lease and
//! recorded nothing, and the generated histories did not catch it for two
//! reasons worth writing down: the alphabet issued its administrative commands
//! as a non-administrator, so those branches were refusal-only and no expiry
//! ever matched a live lease; and the search for unrecorded sites was a
//! line-oriented grep, which `state\n.resources\n.insert(..)` walks straight
//! past.
//!
//! So the mutation and the record are one call. There is no way to perform one
//! without the other, and `the_replicated_tables_are_written_only_through_this_module`
//! keeps it that way for tables added later.

use super::state::{
    AuditRow, ChangeSet, ClientId, GrantKey, Intent, NodeId, OperationId, ResourceKey, ResourceRow,
    Route, Session, State,
};

/// Write a resource row.
pub fn put_resource(
    state: &mut State,
    changes: &mut ChangeSet,
    key: ResourceKey,
    row: ResourceRow,
) {
    changes.resources.insert(key.clone());
    state.resources.insert(key, row);
}

/// Write a grant. Returns whether the row is new, which is what the §12.1
/// growth charge turns on.
pub fn put_grant(state: &mut State, changes: &mut ChangeSet, key: GrantKey, rights: u64) -> bool {
    changes.grants.insert(key.clone());
    state.acl.insert(key, rights).is_none()
}

/// Remove a grant. Returns whether one was there.
pub fn remove_grant(state: &mut State, changes: &mut ChangeSet, key: &GrantKey) -> bool {
    changes.grants.insert(key.clone());
    state.acl.remove(key).is_some()
}

/// Add an administrator. Returns whether the row is new.
pub fn put_admin(state: &mut State, changes: &mut ChangeSet, target: NodeId) -> bool {
    changes.admins.insert(target);
    state.admins.insert(target)
}

/// Remove an administrator. Returns whether one was there.
pub fn remove_admin(state: &mut State, changes: &mut ChangeSet, target: &NodeId) -> bool {
    changes.admins.insert(*target);
    state.admins.remove(target)
}

/// Write a client session row.
pub fn put_session(
    state: &mut State,
    changes: &mut ChangeSet,
    key: (NodeId, ClientId),
    row: Session,
) {
    changes.sessions.insert(key);
    state.sessions.insert(key, row);
}

/// Write a membership intent.
pub fn put_intent(
    state: &mut State,
    changes: &mut ChangeSet,
    operation_id: OperationId,
    intent: Intent,
) {
    changes.intents.insert(operation_id);
    state.intents.insert(operation_id, intent);
}

/// Evict a membership intent, returning what was there.
pub fn remove_intent(
    state: &mut State,
    changes: &mut ChangeSet,
    operation_id: &OperationId,
) -> Option<Intent> {
    changes.intents.insert(*operation_id);
    state.intents.remove(operation_id)
}

/// Write a node route.
pub fn put_route(state: &mut State, changes: &mut ChangeSet, node: NodeId, route: Route) {
    changes.routes.insert(node);
    state.routes.insert(node, route);
}

/// Evict a node route, returning what was there.
pub fn remove_route(state: &mut State, changes: &mut ChangeSet, node: &NodeId) -> Option<Route> {
    changes.routes.insert(*node);
    state.routes.remove(node)
}

/// Append an audit row.
pub fn put_audit(state: &mut State, changes: &mut ChangeSet, key: (u64, u64), row: AuditRow) {
    changes.audit.insert(key);
    state.audit.insert(key, row);
}

/// Prune an audit row, returning what was there.
pub fn remove_audit(
    state: &mut State,
    changes: &mut ChangeSet,
    key: (u64, u64),
) -> Option<AuditRow> {
    changes.audit.insert(key);
    state.audit.remove(&key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// The replicated tables are written only through this module.
    ///
    /// The guard is over **whitespace-flattened** source, because the site this
    /// exists to prevent was invisible to a line-oriented search: rustfmt had
    /// split `state.resources.insert(..)` across three lines, so a grep for the
    /// whole expression found fourteen of the fifteen mutation sites and the
    /// fifteenth wrote an expired lease that was never persisted.
    ///
    /// Reading a table is untouched. Only mutation has to be paired with a
    /// record, and pairing it here is what makes the pairing unforgettable.
    #[test]
    fn the_replicated_tables_are_written_only_through_this_module() {
        const SOURCES: &[(&str, &str)] = &[
            ("apply.rs", include_str!("apply.rs")),
            ("admin.rs", include_str!("admin.rs")),
            ("grant.rs", include_str!("grant.rs")),
            ("membership.rs", include_str!("membership.rs")),
            ("resource.rs", include_str!("resource.rs")),
            ("retire.rs", include_str!("retire.rs")),
            ("expire.rs", include_str!("expire.rs")),
            ("classify.rs", include_str!("classify.rs")),
            ("boundary.rs", include_str!("boundary.rs")),
        ];
        const TABLES: &[&str] = &[
            "resources",
            "acl",
            "admins",
            "sessions",
            "intents",
            "routes",
            "audit",
        ];
        const MUTATORS: &[&str] = &["insert", "remove", "retain", "clear"];

        let mut sites = Vec::new();
        for (name, source) in SOURCES {
            // Tests legitimately build states directly; the rule is about the
            // code that applies entries.
            let end = source.find("\n#[cfg(test)]\n").unwrap_or(source.len());
            let flat: String = source[..end]
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            for table in TABLES {
                for mutator in MUTATORS {
                    let needle = format!("state . {table} . {mutator}");
                    let spaced = flat.replace('.', " . ");
                    if spaced.contains(&needle) {
                        sites.push(format!("{name}: state.{table}.{mutator}"));
                    }
                }
            }
        }
        assert!(
            sites.is_empty(),
            "a replicated table must be mutated through `model::tables`, which records the \
             row in the entry's change set at the same time; found: {sites:?}"
        );
    }

    fn empty() -> (State, ChangeSet) {
        (State::default(), ChangeSet::default())
    }

    /// Every writer records the row it touched, in both directions.
    ///
    /// Table-driven so a writer added without a record fails here rather than
    /// waiting for a history to reach it.
    #[test]
    fn every_writer_records_the_row_it_touched() {
        let key = ResourceKey::new("ns", b"k".to_vec()).expect("valid");
        let node = [1u8; 32];
        let client = [2u8; 16];
        let operation = [3u8; 16];
        let log_id = super::super::state::LogId::new(1, node, 1);

        let (mut s, mut c) = empty();
        put_resource(&mut s, &mut c, key.clone(), ResourceRow::default());
        assert_eq!(c.resources, BTreeSet::from([key.clone()]));

        let (mut s, mut c) = empty();
        assert!(put_grant(&mut s, &mut c, (key.clone(), node), 1));
        assert!(
            !put_grant(&mut s, &mut c, (key.clone(), node), 2),
            "not new"
        );
        assert!(remove_grant(&mut s, &mut c, &(key.clone(), node)));
        assert!(!remove_grant(&mut s, &mut c, &(key.clone(), node)), "gone");
        assert_eq!(c.grants, BTreeSet::from([(key.clone(), node)]));

        let (mut s, mut c) = empty();
        assert!(put_admin(&mut s, &mut c, node));
        assert!(!put_admin(&mut s, &mut c, node), "already an admin");
        assert!(remove_admin(&mut s, &mut c, &node));
        assert!(!remove_admin(&mut s, &mut c, &node), "gone");
        assert_eq!(c.admins, BTreeSet::from([node]));

        let (mut s, mut c) = empty();
        put_session(
            &mut s,
            &mut c,
            (node, client),
            Session {
                last_sequence: 0,
                request_hash: [0u8; 32],
                last_result: None,
                retired: true,
            },
        );
        assert_eq!(c.sessions, BTreeSet::from([(node, client)]));

        let (mut s, mut c) = empty();
        assert!(remove_intent(&mut s, &mut c, &operation).is_none());
        assert_eq!(
            c.intents,
            BTreeSet::from([operation]),
            "an absent row is still a write"
        );

        let (mut s, mut c) = empty();
        put_route(
            &mut s,
            &mut c,
            node,
            Route {
                ticket: b"t".to_vec(),
                source_operation_id: operation,
                introduced_log_id: log_id,
            },
        );
        assert!(remove_route(&mut s, &mut c, &node).is_some());
        assert_eq!(c.routes, BTreeSet::from([node]));

        let (mut s, mut c) = empty();
        put_audit(
            &mut s,
            &mut c,
            (1, 1),
            AuditRow {
                leader_node_id: node,
                actor: None,
                operation_code: 1,
                resource: None,
                result: super::super::state::AuditResult::System(1),
                epoch: 0,
                value_revision: 0,
            },
        );
        assert!(remove_audit(&mut s, &mut c, (1, 1)).is_some());
        assert_eq!(c.audit, BTreeSet::from([(1, 1)]));
    }
}
