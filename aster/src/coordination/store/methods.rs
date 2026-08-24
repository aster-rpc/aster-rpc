//! The §5.2 method classification — one declaration (S4-A).
//!
//! Every fact the runtime needs about a Store method is a column here: its
//! §5.2 method/audit code, whether it consumes a sequence, which consistency
//! class serves it, and the committed permission it requires. The router, the
//! permission gate, the reservation check and the client read this table;
//! none of them keeps a method list of its own.
//!
//! The §5.2 table is frozen. Adding a method is a protocol change, not an
//! edit to this file.

use crate::coordination::model::limits::{
    RIGHT_ACQUIRE, RIGHT_MUTATE, RIGHT_READ, RIGHT_RELEASE, RIGHT_RENEW, RIGHT_REVOKE,
};

/// How a method is served — the S4-C consistency class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// Shared session classifier, preflight projection, then a committed
    /// `ClientCommandV1`. Answers `MutationReplyWireV1` or a defined
    /// stage-only `RpcStatus`.
    SequencedMutation,
    /// Sessionless committed `RetireClientCommandV1` (§5.2, §10). Answers
    /// `RetireClientReplyWireV1`.
    SessionlessMutation,
    /// Leader barrier, one guarded read transaction, transaction-local applied
    /// witness at or past the barrier, authorize and project from that same
    /// transaction. Answers a typed bounded unary reply.
    LinearizableRead,
    /// The reviewed §S4-STATUS-CUT mixed cut: replicated fields from one
    /// guarded transaction after the barrier, local fields sampled afterwards
    /// and advisory. Answers a typed bounded unary reply.
    StatusCut,
    /// Barrier, fixed upper LogId, then a bounded server stream.
    Stream,
}

/// The committed permission a method requires (§5.2's permission column).
///
/// **Group administrator** always means the shared §8.1 predicate — the
/// immutable deployment ROOT identity or a current delegated administrator row.
/// A handler must not spell a narrower `admins.contains(actor)` check of its
/// own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Permission {
    /// The exact resource rights bits, checked against the committed ACL.
    Resource(u64),
    /// Current holder of the fence **plus** the exact resource rights bits.
    HolderPlus(u64),
    /// The shared §8.1 group-administrator predicate.
    Administrator,
    /// The target actor itself, or a group administrator.
    ActorOrAdministrator,
}

/// One row of the §5.2 method table.
#[derive(Clone, Copy, Debug)]
pub struct MethodSpec {
    /// The literal §5.2 method name.
    pub name: &'static str,
    /// The §5.2 method/audit operation code. `0` for reads and streams, which
    /// have none — audit codes 1–11 are mutation codes.
    pub code: i32,
    /// How the method is served.
    pub class: Class,
    /// The committed permission it requires.
    pub permission: Permission,
}

/// The nineteen §5.2 methods, in the frozen table's order.
pub const METHODS: &[MethodSpec] = &[
    MethodSpec {
        name: "acquire",
        code: 1,
        class: Class::SequencedMutation,
        permission: Permission::Resource(RIGHT_ACQUIRE),
    },
    MethodSpec {
        name: "renew",
        code: 2,
        class: Class::SequencedMutation,
        permission: Permission::HolderPlus(RIGHT_RENEW),
    },
    MethodSpec {
        name: "read_linearizable",
        code: 0,
        class: Class::LinearizableRead,
        permission: Permission::Resource(RIGHT_READ),
    },
    MethodSpec {
        name: "compare_and_set",
        code: 3,
        class: Class::SequencedMutation,
        permission: Permission::HolderPlus(RIGHT_MUTATE),
    },
    MethodSpec {
        name: "release",
        code: 4,
        class: Class::SequencedMutation,
        permission: Permission::HolderPlus(RIGHT_RELEASE),
    },
    MethodSpec {
        name: "release_with_value",
        code: 5,
        class: Class::SequencedMutation,
        permission: Permission::HolderPlus(RIGHT_RELEASE | RIGHT_MUTATE),
    },
    MethodSpec {
        name: "revoke",
        code: 6,
        class: Class::SequencedMutation,
        permission: Permission::Resource(RIGHT_REVOKE),
    },
    MethodSpec {
        name: "set_grant",
        code: 7,
        class: Class::SequencedMutation,
        permission: Permission::Administrator,
    },
    MethodSpec {
        name: "set_admin",
        code: 8,
        class: Class::SequencedMutation,
        permission: Permission::Administrator,
    },
    MethodSpec {
        name: "change_membership",
        code: 9,
        class: Class::SequencedMutation,
        permission: Permission::Administrator,
    },
    MethodSpec {
        name: "cancel_membership",
        code: 11,
        class: Class::SequencedMutation,
        permission: Permission::Administrator,
    },
    MethodSpec {
        name: "membership_status",
        code: 0,
        class: Class::LinearizableRead,
        permission: Permission::Administrator,
    },
    MethodSpec {
        name: "inspect_acl",
        code: 0,
        class: Class::LinearizableRead,
        permission: Permission::Administrator,
    },
    MethodSpec {
        name: "inspect_admins",
        code: 0,
        class: Class::LinearizableRead,
        permission: Permission::Administrator,
    },
    MethodSpec {
        name: "retire_client",
        code: 10,
        class: Class::SessionlessMutation,
        permission: Permission::ActorOrAdministrator,
    },
    MethodSpec {
        name: "probe_client",
        code: 0,
        class: Class::LinearizableRead,
        permission: Permission::ActorOrAdministrator,
    },
    MethodSpec {
        name: "group_status",
        code: 0,
        class: Class::StatusCut,
        permission: Permission::Administrator,
    },
    MethodSpec {
        name: "audit_stream",
        code: 0,
        class: Class::Stream,
        permission: Permission::Administrator,
    },
    MethodSpec {
        name: "recovery_export",
        code: 0,
        class: Class::Stream,
        permission: Permission::Administrator,
    },
];

/// Look one method up by its literal name.
pub fn spec(name: &str) -> Option<&'static MethodSpec> {
    METHODS.iter().find(|m| m.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// The S4-A gate: the classification's method set equals
    /// `service::STORE_METHODS` **as a set** — and both equal the nineteen
    /// §5.2 literal names, spelled here independently rather than copied from
    /// either table. A twentieth reserved method, a dropped row, or a
    /// duplicate fails before any handler runs.
    #[test]
    fn the_classification_set_equals_the_reservation_and_the_frozen_names() {
        // The frozen §5.2 table, transcribed by hand from the document — not
        // from `METHODS`, not from `STORE_METHODS`.
        let frozen: BTreeSet<&str> = [
            "acquire",
            "renew",
            "read_linearizable",
            "compare_and_set",
            "release",
            "release_with_value",
            "revoke",
            "set_grant",
            "set_admin",
            "change_membership",
            "cancel_membership",
            "membership_status",
            "inspect_acl",
            "inspect_admins",
            "retire_client",
            "probe_client",
            "group_status",
            "audit_stream",
            "recovery_export",
        ]
        .into_iter()
        .collect();
        assert_eq!(frozen.len(), 19, "§5.2 declares nineteen methods");

        let classified: BTreeSet<&str> = METHODS.iter().map(|m| m.name).collect();
        assert_eq!(
            classified.len(),
            METHODS.len(),
            "a duplicate row would silently drop a method from the set"
        );
        assert_eq!(classified, frozen, "the classification table drifted");

        let reserved: BTreeSet<&str> = crate::coordination::service::STORE_METHODS
            .iter()
            .copied()
            .collect();
        assert_eq!(
            reserved.len(),
            crate::coordination::service::STORE_METHODS.len(),
            "a duplicate reservation would silently drop a method from the set"
        );
        assert_eq!(reserved, frozen, "the service reservation drifted");
    }

    /// The mutation codes are §5.2's: 1–9 and 11 for the sequenced table,
    /// 10 for sessionless retirement, 0 for everything that has none.
    #[test]
    fn mutation_codes_are_the_frozen_ones_and_only_mutations_carry_one() {
        let mut codes: Vec<i32> = METHODS
            .iter()
            .filter(|m| m.code != 0)
            .map(|m| m.code)
            .collect();
        codes.sort_unstable();
        assert_eq!(codes, vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]);

        for method in METHODS {
            let is_mutation = matches!(
                method.class,
                Class::SequencedMutation | Class::SessionlessMutation
            );
            assert_eq!(
                method.code != 0,
                is_mutation,
                "{}: only mutations have a §5.2 method/audit code",
                method.name
            );
        }

        assert_eq!(
            spec("retire_client").expect("in the table").code,
            10,
            "the one code the 1–11 range leaves free belongs to the sessionless mutation"
        );
        assert_eq!(
            spec("retire_client").expect("in the table").class,
            Class::SessionlessMutation
        );
    }

    /// Every method's §5.2 permission, transcribed independently. Tests that
    /// prove each permission end to end live with the handlers; this pins the
    /// *table* so a drifted row is caught even before those run.
    #[test]
    fn the_permission_column_matches_the_frozen_table() {
        use Permission::*;
        let expected: &[(&str, Permission)] = &[
            ("acquire", Resource(RIGHT_ACQUIRE)),
            ("renew", HolderPlus(RIGHT_RENEW)),
            ("read_linearizable", Resource(RIGHT_READ)),
            ("compare_and_set", HolderPlus(RIGHT_MUTATE)),
            ("release", HolderPlus(RIGHT_RELEASE)),
            (
                "release_with_value",
                HolderPlus(RIGHT_RELEASE | RIGHT_MUTATE),
            ),
            ("revoke", Resource(RIGHT_REVOKE)),
            ("set_grant", Administrator),
            ("set_admin", Administrator),
            ("change_membership", Administrator),
            ("cancel_membership", Administrator),
            ("membership_status", Administrator),
            ("inspect_acl", Administrator),
            ("inspect_admins", Administrator),
            ("retire_client", ActorOrAdministrator),
            ("probe_client", ActorOrAdministrator),
            ("group_status", Administrator),
            ("audit_stream", Administrator),
            ("recovery_export", Administrator),
        ];
        assert_eq!(expected.len(), METHODS.len());
        for (name, permission) in expected {
            assert_eq!(
                &spec(name)
                    .unwrap_or_else(|| panic!("{name} missing"))
                    .permission,
                permission,
                "{name}: permission drifted from the frozen table"
            );
        }
    }
}
