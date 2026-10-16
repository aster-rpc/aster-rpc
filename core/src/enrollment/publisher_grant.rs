//! The issuer-key publisher's write grant (gateway identity, slice 3; design:
//! `docs/_internal/web_gateway/gateway-identity.md`, "Issuer keys").
//!
//! Issuer key sets live in the fleet namespace, which only the root can write.
//! A publisher (a node whose credential grants `aster.identity-publisher`)
//! gets the fleet namespace's **write capability** the way an observer gets
//! the observations namespace's: the root seals it to the publisher's
//! identity-derived HPKE key and writes the sealed grant into the fleet
//! namespace at `aster/identity-grants/v1/publishers/grants/<node id>`.
//! Only the exact root author's record there counts, and it must open, under
//! the context below, to the write secret of the fleet namespace itself.
//!
//! The capability lets a publisher write entries, not authority: cores read
//! enrollment records from the root author only, and issuer key sets only
//! from authors whose credential grants the publisher role. Withdrawing the
//! grant (a revoked record at the same key) stops an honest publisher; a
//! leaked secret is only unlearned by rotating the root (and so the fleet
//! namespace).

use crate::grant_record::GrantRecord;
use crate::grants::{grant_key, reconcile_namespace_grant, GrantContext, GrantOutcome};
use crate::namespace::CoreNamespaceCapability;
use crate::CoreDoc;

/// Where the root writes publisher grants in the fleet namespace. Outside
/// the policy record prefix, like the observer grants: they are Fory grant
/// records, not JSON policy.
pub const PUBLISHER_GRANT_PREFIX: &str = "aster/identity-grants/v1/publishers";
/// Binds every sealed publisher grant.
pub const PUBLISHER_GRANT_APP: &str = "aster-identity/publisher-grant/v1";
const ROLE: &str = "write";
const MAX_GRANT_RECORD_BYTES: u64 = 64 * 1024;

/// The state of a publisher's sealed write grant in the fleet namespace.
#[derive(Debug)]
pub enum PublisherGrant {
    /// The fleet namespace's write secret.
    Active([u8; 32]),
    /// The root withdrew the grant: stop writing.
    Revoked,
    /// No grant yet, or its content has not arrived.
    Absent,
}

/// The fleet-namespace key of `publisher`'s grant.
pub fn publisher_grant_key(publisher: &[u8; 32]) -> String {
    grant_key(PUBLISHER_GRANT_PREFIX, &hex::encode(publisher))
}

fn context<'a>(
    granter: &'a str,
    recipient: &'a str,
    fleet: &'a [u8; 32],
    path: &'a str,
) -> GrantContext<'a> {
    GrantContext {
        app: PUBLISHER_GRANT_APP,
        granter,
        recipient,
        resource: fleet,
        path,
        role: ROLE,
    }
}

/// The root's side: the sealed grant of `fleet_secret` (the fleet namespace's
/// write secret) to `publisher`, and the key to write it at, as the root
/// author, in that fleet namespace.
pub fn publisher_grant_record(
    root: &[u8; 32],
    publisher: &[u8; 32],
    fleet_secret: &[u8; 32],
) -> anyhow::Result<(String, Vec<u8>)> {
    let fleet = CoreNamespaceCapability::Write(*fleet_secret).namespace_id();
    let granter = hex::encode(root);
    let recipient = hex::encode(publisher);
    let key = publisher_grant_key(publisher);
    let envelope = crate::grants::seal_namespace_grant(
        &context(&granter, &recipient, &fleet, &key),
        &CoreNamespaceCapability::Write(*fleet_secret),
    )?;
    Ok((key, GrantRecord::active(ROLE, 1, envelope).encode_fory()))
}

/// The root's side: the record that withdraws `publisher`'s grant.
pub fn publisher_grant_withdrawal(reason: &str) -> Vec<u8> {
    GrantRecord::revoked(ROLE, reason).encode_fory()
}

/// The publisher's side: open the root's grant to `publisher_secret`'s node
/// from its copy of `fleet`. Only the exact root author's record counts, and
/// it must open to `fleet`'s own write secret.
pub async fn open_publisher_grant(
    fleet: &CoreDoc,
    root: &[u8; 32],
    publisher_secret: &[u8; 32],
) -> anyhow::Result<PublisherGrant> {
    let namespace: [u8; 32] = *fleet.doc.id().as_bytes();
    let publisher = crate::delegation::jws::public_key(publisher_secret)
        .map_err(|_| anyhow::anyhow!("invalid publisher key"))?;
    let key = publisher_grant_key(&publisher);
    let view = super::RootAuthoredDocView::new(namespace, *root, MAX_GRANT_RECORD_BYTES, 1);
    let record = match view.read_exact(fleet, key.clone().into_bytes()).await? {
        super::RootRecordRead::Available(bytes) => bytes,
        super::RootRecordRead::Absent | super::RootRecordRead::Pending => {
            return Ok(PublisherGrant::Absent)
        }
    };
    let granter = hex::encode(root);
    let recipient = hex::encode(publisher);
    let context = context(&granter, &recipient, &namespace, &key);
    Ok(
        match reconcile_namespace_grant(publisher_secret, &context, Some(&record)) {
            GrantOutcome::Active(CoreNamespaceCapability::Write(secret))
                if CoreNamespaceCapability::Write(secret).namespace_id() == namespace =>
            {
                PublisherGrant::Active(secret)
            }
            GrantOutcome::Revoked { .. } => PublisherGrant::Revoked,
            GrantOutcome::Absent => PublisherGrant::Absent,
            GrantOutcome::Active(_) | GrantOutcome::Unopenable(_) => anyhow::bail!(
                "publisher grant does not open to the fleet namespace's write capability"
            ),
        },
    )
}
