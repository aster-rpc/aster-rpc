#![cfg(feature = "docs")]
//! Sealed grants: AAD-bound seal/open, typed namespace-capability grants,
//! role consistency, and pinned-author PolicyDoc reads.

use aster::grants::{
    grant_key, open_grant, open_namespace_grant, reconcile_grant, reconcile_namespace_grant,
    seal_grant, seal_namespace_grant, GrantContext, GrantOutcome, GrantRecord, PolicyDoc,
};
use aster::{AsterConfig, NamespaceCapability, NamespaceSecret, Node, RelayMode, SecretKey};

fn cfg() -> AsterConfig {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .build()
}

fn node_identity() -> (SecretKey, aster::NodeId) {
    let secret = SecretKey::generate();
    let node_id = aster::NodeId::from_hex(aster::attestation::public_key(&secret).to_hex());
    (secret, node_id)
}

fn fresh_namespace() -> NamespaceSecret {
    NamespaceSecret::from_bytes(SecretKey::generate().to_bytes())
}

#[test]
fn seal_open_roundtrip_and_aad_binding() {
    let (root_secret, root_id) = node_identity();
    let (recipient_secret, recipient_id) = node_identity();
    let _ = root_secret;

    let resource = [7u8; 32];
    let ctx = GrantContext {
        app: "test/grant/v1",
        granter: &root_id,
        recipient: &recipient_id,
        resource: &resource,
        path: "/test/v1/trees/x/grants/y",
        role: "read",
    };

    let sealed = seal_grant(&ctx, b"the-secret").unwrap();
    let opened = open_grant(&recipient_secret, &ctx, &sealed).unwrap();
    assert_eq!(opened.expose_secret(), b"the-secret");

    // Any context mutation breaks the AAD → open fails.
    let other = node_identity().1;
    let wrong_recipient = GrantContext {
        recipient: &other,
        ..ctx.clone()
    };
    assert!(open_grant(&recipient_secret, &wrong_recipient, &sealed).is_err());

    let wrong_role = GrantContext {
        role: "write",
        ..ctx.clone()
    };
    assert!(open_grant(&recipient_secret, &wrong_role, &sealed).is_err());

    let wrong_path = GrantContext {
        path: "/test/v1/trees/x/grants/z",
        ..ctx.clone()
    };
    assert!(open_grant(&recipient_secret, &wrong_path, &sealed).is_err());

    // The wrong node's identity secret cannot open it either.
    let (mallory_secret, _) = node_identity();
    assert!(open_grant(&mallory_secret, &ctx, &sealed).is_err());
}

#[test]
fn namespace_grant_roundtrip_and_role_consistency() {
    let (_, root_id) = node_identity();
    let (recipient_secret, recipient_id) = node_identity();

    let namespace = fresh_namespace();
    let namespace_id = namespace.id();
    let capability = NamespaceCapability::write(namespace.clone());

    let ctx = GrantContext {
        app: "test/grant/v1",
        granter: &root_id,
        recipient: &recipient_id,
        resource: &namespace_id.to_bytes(),
        path: "/test/v1/grants/n",
        role: "write",
    };

    let sealed = seal_namespace_grant(&ctx, &capability).unwrap();
    let opened = open_namespace_grant(&recipient_secret, &ctx, &sealed).unwrap();
    assert!(opened.can_write());
    assert_eq!(opened.namespace_id(), namespace_id);

    // Sealing a write capability under a "read" role is rejected up front.
    let read_ctx = GrantContext {
        role: "read",
        ..ctx.clone()
    };
    assert!(seal_namespace_grant(&read_ctx, &capability).is_err());

    // And a read capability under "read" works.
    let read_cap = NamespaceCapability::read(namespace_id);
    let sealed_read = seal_namespace_grant(&read_ctx, &read_cap).unwrap();
    let opened_read = open_namespace_grant(&recipient_secret, &read_ctx, &sealed_read).unwrap();
    assert!(!opened_read.can_write());
}

#[test]
fn grant_key_convention() {
    let (_, node) = node_identity();
    let key = grant_key("/portal/v1/trees/abc", &node);
    assert_eq!(
        key,
        format!("/portal/v1/trees/abc/grants/{}", node.as_str())
    );
    // Trailing slash on the prefix doesn't double up.
    assert_eq!(
        grant_key("/p/", &node),
        format!("/p/grants/{}", node.as_str())
    );
}

#[tokio::test]
async fn policy_doc_pins_the_author() {
    let node = Node::start(cfg()).await.unwrap();
    let docs = node.docs();

    let doc = docs.create().await.unwrap();
    let root_author = docs.default_author().await.unwrap();
    let other_author = docs.create_author().await.unwrap();

    doc.set_bytes(&root_author, b"/p/policy".to_vec(), b"root-value".to_vec())
        .await
        .unwrap();
    // A different author squatting the same key must be invisible.
    doc.set_bytes(&other_author, b"/p/policy".to_vec(), b"evil-value".to_vec())
        .await
        .unwrap();
    doc.set_bytes(&other_author, b"/p/only-evil".to_vec(), b"x".to_vec())
        .await
        .unwrap();

    let policy = PolicyDoc::wrap(doc, root_author);
    assert_eq!(
        policy.get(b"/p/policy".to_vec()).await.unwrap().as_deref(),
        Some(b"root-value".as_slice()),
        "pinned-author read must ignore other authors"
    );
    assert_eq!(
        policy.get(b"/p/only-evil".to_vec()).await.unwrap(),
        None,
        "records only other authors wrote are invisible"
    );

    let listed = policy.list_prefix(b"/p/".to_vec()).await.unwrap();
    assert_eq!(
        listed.len(),
        1,
        "prefix listing filters to the pinned author"
    );
    assert_eq!(listed[0].key, b"/p/policy".to_vec());

    node.shutdown().await;
}

#[tokio::test]
async fn grant_flow_over_policy_doc() {
    // Root node writes a sealed namespace grant into its policy doc; the
    // recipient (same replica here — sync transport is covered elsewhere)
    // fetches it by convention key and opens it with its identity secret.
    let root = Node::start(cfg()).await.unwrap();
    let docs = root.docs();
    let doc = docs.create().await.unwrap();
    let root_author = docs.default_author().await.unwrap();

    let (recipient_secret, recipient_id) = node_identity();

    let shared_namespace = fresh_namespace();
    let namespace_id = shared_namespace.id();
    let prefix = "/app/v1";
    let key = grant_key(prefix, &recipient_id);
    let ctx = GrantContext {
        app: "app/grant/v1",
        granter: &root.id(),
        recipient: &recipient_id,
        resource: &namespace_id.to_bytes(),
        path: &key,
        role: "write",
    };
    let sealed = seal_namespace_grant(&ctx, &NamespaceCapability::write(shared_namespace)).unwrap();
    doc.set_bytes(&root_author, key.clone().into_bytes(), sealed)
        .await
        .unwrap();

    let policy = PolicyDoc::wrap(doc, root_author);
    let fetched = policy
        .fetch_grant(prefix, &recipient_id)
        .await
        .unwrap()
        .expect("grant record present");
    let capability = open_namespace_grant(&recipient_secret, &ctx, &fetched).unwrap();
    assert!(capability.can_write());
    assert_eq!(capability.namespace_id(), namespace_id);

    root.shutdown().await;
}

#[test]
fn reconcile_distinguishes_absent_from_revoked() {
    // The rule this API exists to make unmissable: absence is not revocation.
    let (_, root_id) = node_identity();
    let (recipient_secret, recipient_id) = node_identity();

    let namespace = fresh_namespace();
    let namespace_id = namespace.id();
    let key = grant_key("/app/v1", &recipient_id);
    let ctx = GrantContext {
        app: "app/data-grant/v1",
        granter: &root_id,
        recipient: &recipient_id,
        resource: &namespace_id.to_bytes(),
        path: &key,
        role: "write",
    };

    // Nothing published yet — a node in this state must keep waiting, not tear
    // itself down. This is the initial-sync case a delete-based scheme breaks.
    let absent = reconcile_namespace_grant(&recipient_secret, &ctx, None);
    assert!(matches!(absent, GrantOutcome::Absent));
    assert!(!absent.drops_capability());

    // Active.
    let sealed = seal_namespace_grant(&ctx, &NamespaceCapability::write(namespace)).unwrap();
    let active_record = GrantRecord::active("write", 1, sealed).encode_fory();
    let active = reconcile_namespace_grant(&recipient_secret, &ctx, Some(&active_record));
    assert!(!active.drops_capability());
    let capability = active.active().expect("active grant opens");
    assert!(capability.can_write());
    assert_eq!(capability.namespace_id(), namespace_id);

    // Revoked — an explicit value written at the same key, carrying a reason.
    let revoked_record = GrantRecord::revoked("write", "node decommissioned").encode_fory();
    let revoked = reconcile_namespace_grant(&recipient_secret, &ctx, Some(&revoked_record));
    assert!(revoked.drops_capability());
    match revoked {
        GrantOutcome::Revoked { reason, role } => {
            assert_eq!(reason, "node decommissioned");
            assert_eq!(role, "write");
        }
        other => panic!("expected Revoked, got {other:?}"),
    }
}

#[test]
fn reconcile_fails_closed_on_unopenable_records() {
    let (_, root_id) = node_identity();
    let (recipient_secret, recipient_id) = node_identity();
    let key = grant_key("/app/v1", &recipient_id);
    let ctx = GrantContext {
        app: "app/data-grant/v1",
        granter: &root_id,
        recipient: &recipient_id,
        resource: b"resource",
        path: &key,
        role: "write",
    };

    // Garbage framing.
    let garbage = reconcile_grant(&recipient_secret, &ctx, Some(b"not a grant record"));
    assert!(garbage.drops_capability());
    assert!(matches!(garbage, GrantOutcome::Unopenable(_)));

    // Well-framed, but sealed to somebody else — the AAD binding fails.
    let (_, other_id) = node_identity();
    let other_ctx = GrantContext {
        recipient: &other_id,
        ..ctx.clone()
    };
    let sealed_elsewhere = seal_grant(&other_ctx, b"secret").unwrap();
    let record = GrantRecord::active("write", 1, sealed_elsewhere).encode_fory();
    let wrong = reconcile_grant(&recipient_secret, &ctx, Some(&record));
    assert!(wrong.drops_capability());
    assert!(matches!(wrong, GrantOutcome::Unopenable(_)));

    // A role the context doesn't expect is reported as a role mismatch rather
    // than an opaque decrypt failure.
    let sealed = seal_grant(&ctx, b"secret").unwrap();
    let mislabelled = GrantRecord::active("read", 1, sealed).encode_fory();
    match reconcile_grant(&recipient_secret, &ctx, Some(&mislabelled)) {
        GrantOutcome::Unopenable(e) => {
            assert!(e.to_string().contains("role"), "got: {e}");
        }
        other => panic!("expected Unopenable, got {other:?}"),
    }
}

#[test]
fn context_version_selects_the_aad_without_trial_decryption() {
    // Two AAD generations coexist during a migration window. The plaintext
    // context_version picks the right one; nothing is trial-decrypted.
    let (_, root_id) = node_identity();
    let (recipient_secret, recipient_id) = node_identity();
    let key = grant_key("/app/v1", &recipient_id);

    let ctx_for = |app| GrantContext {
        app,
        granter: &root_id,
        recipient: &recipient_id,
        resource: b"resource",
        path: &key,
        role: "write",
    };
    let v1 = ctx_for("app/data-grant/v1");
    let v2 = ctx_for("app/data-grant/v2");

    let record = GrantRecord::active("write", 2, seal_grant(&v2, b"secret").unwrap()).encode_fory();

    // A reader that never inspects the version and guesses v1 fails closed.
    assert!(reconcile_grant(&recipient_secret, &v1, Some(&record)).drops_capability());

    // Reading the version first selects the context that works.
    let decoded = GrantRecord::decode_fory(&record).unwrap();
    let ctx = match decoded.context_version {
        1 => &v1,
        2 => &v2,
        other => panic!("unknown context version {other}"),
    };
    let opened = reconcile_grant(&recipient_secret, ctx, Some(&record))
        .active()
        .expect("versioned context opens");
    assert_eq!(opened.expose_secret(), b"secret");
}

#[tokio::test]
async fn revocation_over_policy_doc_overwrites_the_key() {
    // End-to-end: the root revokes by overwriting the grant key with a revoked
    // record — the key stays present, so the recipient's next reconcile reads a
    // decision rather than an absence it would have to guess about.
    let root = Node::start(cfg()).await.unwrap();
    let docs = root.docs();
    let doc = docs.create().await.unwrap();
    let root_author = docs.default_author().await.unwrap();

    let (recipient_secret, recipient_id) = node_identity();
    let shared_namespace = fresh_namespace();
    let namespace_id = shared_namespace.id();
    let prefix = "/app/v1";
    let key = grant_key(prefix, &recipient_id);
    let ctx = GrantContext {
        app: "app/grant/v1",
        granter: &root.id(),
        recipient: &recipient_id,
        resource: &namespace_id.to_bytes(),
        path: &key,
        role: "write",
    };

    let sealed = seal_namespace_grant(&ctx, &NamespaceCapability::write(shared_namespace)).unwrap();
    doc.set_bytes(
        &root_author,
        key.clone().into_bytes(),
        GrantRecord::active("write", 1, sealed).encode_fory(),
    )
    .await
    .unwrap();

    let policy = PolicyDoc::wrap(doc.clone(), root_author.clone());
    let fetched = policy.fetch_grant(prefix, &recipient_id).await.unwrap();
    assert!(
        reconcile_namespace_grant(&recipient_secret, &ctx, fetched.as_deref())
            .active()
            .is_some()
    );

    // Revoke in place.
    doc.set_bytes(
        &root_author,
        key.clone().into_bytes(),
        GrantRecord::revoked("write", "rotated").encode_fory(),
    )
    .await
    .unwrap();

    let fetched = policy
        .fetch_grant(prefix, &recipient_id)
        .await
        .unwrap()
        .expect("revocation is a record, so the key is still present");
    let outcome = reconcile_namespace_grant(&recipient_secret, &ctx, Some(&fetched));
    assert!(outcome.drops_capability());
    assert!(matches!(outcome, GrantOutcome::Revoked { .. }));

    root.shutdown().await;
}

#[test]
fn group_identity_grant_one_seal_many_openers() {
    // Group pattern: a grant sealed ONCE to a group keypair's id opens for
    // every holder of the group secret — no per-member sealing per grant.
    let (_, root_id) = node_identity();

    // The "group" is just an Ed25519 keypair used as an encryption address.
    let (group_secret, group_id) = node_identity();

    // Bootstrap (once per membership change): each member receives the group
    // secret via an ordinary per-member sealed grant.
    let (member_a_secret, member_a_id) = node_identity();
    let (member_b_secret, member_b_id) = node_identity();
    for (member_secret, member_id) in [
        (&member_a_secret, &member_a_id),
        (&member_b_secret, &member_b_id),
    ] {
        let boot_key = grant_key("/app/v1/groups/ops", member_id);
        let boot_ctx = GrantContext {
            app: "app/group-membership/v1",
            granter: &root_id,
            recipient: member_id,
            resource: group_id.as_str().as_bytes(),
            path: &boot_key,
            role: "member",
        };
        let sealed_secret = seal_grant(&boot_ctx, &group_secret.to_bytes()).unwrap();
        let opened = open_grant(member_secret, &boot_ctx, &sealed_secret).unwrap();
        assert_eq!(opened.expose_secret(), &group_secret.to_bytes());
    }

    // Steady state: ONE seal per grant, addressed to the group id.
    let namespace = fresh_namespace();
    let namespace_id = namespace.id();
    let record_key = grant_key("/app/v1", &group_id);
    let ctx = GrantContext {
        app: "app/data-grant/v1",
        granter: &root_id,
        recipient: &group_id,
        resource: &namespace_id.to_bytes(),
        path: &record_key,
        role: "write",
    };
    let sealed = seal_namespace_grant(&ctx, &NamespaceCapability::write(namespace)).unwrap();

    // Both members open the SAME record with the group secret.
    let group_secret_a = SecretKey::from_bytes(group_secret.to_bytes()); // as member A holds it
    let group_secret_b = SecretKey::from_bytes(group_secret.to_bytes()); // as member B holds it
    for holder in [&group_secret_a, &group_secret_b] {
        let cap = open_namespace_grant(holder, &ctx, &sealed).unwrap();
        assert!(cap.can_write());
        assert_eq!(cap.namespace_id(), namespace_id);
    }

    // Non-members (their own identity secrets) cannot open the group grant.
    assert!(open_namespace_grant(&member_a_secret, &ctx, &sealed).is_err());
    assert!(open_namespace_grant(&member_b_secret, &ctx, &sealed).is_err());
}
