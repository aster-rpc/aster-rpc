#![cfg(feature = "docs")]
//! Sealed grants: AAD-bound seal/open, typed namespace-capability grants,
//! role consistency, and pinned-author PolicyDoc reads.

use aster::grants::{
    grant_key, open_grant, open_namespace_grant, seal_grant, seal_namespace_grant, GrantContext,
    PolicyDoc,
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
