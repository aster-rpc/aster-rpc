//! Link claim profiles: one common claim set plus exactly one extension per
//! link kind. These are untrusted wire types; only `verify_chain` turns them
//! into authority.

use std::collections::{BTreeMap, BTreeSet};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};

use super::{ChainError, MAX_LINK_BYTES, MAX_LIST_ENTRIES, MAX_STRING_BYTES};
use crate::topology::locality::{
    CoreDeclaredLocality, MalformedLocality, REGION_ATTRIBUTE, ZONE_ATTRIBUTE,
};

pub const DELEGATION_JWT_TYPE: &str = "aster-delegation-v1+jwt";
pub const ACTION_JWT_TYPE: &str = "aster-action-v1+jwt";
/// A node's declared region and zone, issued alongside its enrollment.
pub const LOCALITY_JWT_TYPE: &str = "aster-locality-v1+jwt";
/// Unchanged from the enrollment profile, so a one-link enrollment chain is an
/// enrollment credential.
pub const ENROLLMENT_LINK_TYPE: &str = "aster-enrollment-v1+jwt";

/// One JWT audience or the standard array representation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum Audience {
    One(String),
    Many(Vec<String>),
}

impl Audience {
    pub fn contains(&self, expected: &str) -> bool {
        self.values().any(|value| value == expected)
    }

    pub fn values(&self) -> impl Iterator<Item = &str> {
        let values: &[String] = match self {
            Self::One(value) => std::slice::from_ref(value),
            Self::Many(values) => values,
        };
        values.iter().map(String::as_str)
    }
}

/// Explicit type tags preserve Portal's primitive vocabulary without coercion.
/// Integers are signed 64-bit; floats must be finite IEEE-754 doubles.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum TypedAttribute {
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
}

/// A complete set is `Some`, including `Some(empty)`; absent data is unknown.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AdmissionClaims {
    pub scopes: Option<BTreeSet<String>>,
    pub tags: Option<BTreeSet<String>>,
    pub attributes: Option<BTreeMap<String, TypedAttribute>>,
}

impl AdmissionClaims {
    /// The declared locality in the reserved `aster.locality.region` /
    /// `aster.locality.zone` attributes. `Ok(None)` when neither is present;
    /// [`MalformedLocality`] when either is present but malformed (not a
    /// string, an invalid value, or a zone without a region).
    pub fn declared_locality(&self) -> Result<Option<CoreDeclaredLocality>, MalformedLocality> {
        let Some(attributes) = &self.attributes else {
            return Ok(None);
        };
        let value = |key: &str| match attributes.get(key) {
            None => Ok(None),
            Some(TypedAttribute::String(value)) => Ok(Some(value.as_str())),
            Some(_) => Err(MalformedLocality),
        };
        match (value(REGION_ATTRIBUTE)?, value(ZONE_ATTRIBUTE)?) {
            (None, None) => Ok(None),
            (region, zone) => CoreDeclaredLocality::from_values(region, zone)
                .map(Some)
                .ok_or(MalformedLocality),
        }
    }
}

/// What an enrolled node expects from its credential.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Registration {
    /// The credential is the node's whole authority for one job. It is never
    /// observed or promoted into a root-authored record.
    #[default]
    Ephemeral,
    /// The credential is a registration token: it admits the node until the
    /// root promotes it to a long-lived registration record.
    Persistent,
}

/// The `aster_enrollment_v1` extension of an enrollment leaf.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentExtensionV1 {
    #[serde(flatten)]
    pub claims: AdmissionClaims,
    /// Required even when empty: omission must not discard restrictions.
    pub mandatory: Vec<String>,
    /// Omitted means ephemeral.
    #[serde(default)]
    pub registration: Registration,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PublicConfirmationKey {
    pub kty: String,
    pub crv: String,
    pub x: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Confirmation {
    pub jwk: PublicConfirmationKey,
}

impl Confirmation {
    pub fn ed25519(key: &[u8; 32]) -> Self {
        Self {
            jwk: PublicConfirmationKey {
                kty: "OKP".into(),
                crv: "Ed25519".into(),
                x: URL_SAFE_NO_PAD.encode(key),
            },
        }
    }

    /// The confirmed key, only for a canonical OKP/Ed25519 JWK.
    pub(crate) fn key(&self) -> Option<[u8; 32]> {
        if self.jwk.kty != "OKP" || self.jwk.crv != "Ed25519" {
            return None;
        }
        let bytes: [u8; 32] = URL_SAFE_NO_PAD.decode(&self.jwk.x).ok()?.try_into().ok()?;
        (URL_SAFE_NO_PAD.encode(bytes) == self.jwk.x).then_some(bytes)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    Delegation,
    Enrollment,
    Action,
    /// A locality statement. It is part of an enrollment, so a delegate needs
    /// `enrollment` in `may_issue` to sign one; `locality` itself is never a
    /// valid `may_issue` entry.
    Locality,
}

impl LinkKind {
    pub fn jwt_type(self) -> &'static str {
        match self {
            Self::Delegation => DELEGATION_JWT_TYPE,
            Self::Enrollment => ENROLLMENT_LINK_TYPE,
            Self::Action => ACTION_JWT_TYPE,
            Self::Locality => LOCALITY_JWT_TYPE,
        }
    }

    fn from_jwt_type(typ: &str) -> Option<Self> {
        [
            Self::Delegation,
            Self::Enrollment,
            Self::Action,
            Self::Locality,
        ]
        .into_iter()
        .find(|kind| kind.jwt_type() == typ)
    }

    /// The `may_issue` entry a parent delegation needs to sign this kind.
    pub(crate) fn authorized_by(self) -> Self {
        match self {
            Self::Locality => Self::Enrollment,
            kind => kind,
        }
    }
}

/// `aster_locality_v1`: a node's declared region and, within it, zone. The
/// only claims a locality statement carries. Values follow
/// [`is_valid_locality_value`](crate::topology::locality::is_valid_locality_value).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LocalityClaims {
    pub region: String,
    #[serde(
        default,
        deserialize_with = "some",
        skip_serializing_if = "Option::is_none"
    )]
    pub zone: Option<String>,
}

impl LocalityClaims {
    /// The declaration, or `None` when a value is malformed.
    pub fn declared(&self) -> Option<CoreDeclaredLocality> {
        CoreDeclaredLocality::from_values(Some(&self.region), self.zone.as_deref())
    }
}

impl From<CoreDeclaredLocality> for LocalityClaims {
    fn from(declared: CoreDeclaredLocality) -> Self {
        Self {
            region: declared.region,
            zone: declared.zone,
        }
    }
}

/// The reserved attribute keys a locality statement claims, for attenuation.
static LOCALITY_KEYS: std::sync::LazyLock<[String; 2]> =
    std::sync::LazyLock::new(|| [REGION_ATTRIBUTE.to_string(), ZONE_ATTRIBUTE.to_string()]);

/// `aster_delegation_v1`: the bounds a delegate's subtree must stay within.
/// Every field is required; an empty list means "none".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DelegationGrant {
    pub may_issue: BTreeSet<LinkKind>,
    /// Audiences the delegate's children may carry.
    pub audiences: BTreeSet<String>,
    pub scopes: BTreeSet<String>,
    pub tags: BTreeSet<String>,
    pub attribute_keys: BTreeSet<String>,
    /// Maximum `exp - iat` of a direct child, in seconds.
    pub max_child_lifetime: u64,
    /// Further delegation links allowed below this one; 0 means leaves only.
    pub max_depth: u32,
    /// `None` (JSON `null`) allows any subject for direct children.
    #[serde(deserialize_with = "present")]
    pub subjects: Option<BTreeSet<String>>,
    pub operations: BTreeSet<String>,
    pub mandatory: BTreeSet<String>,
    /// Whether the delegate may enroll persistent nodes, or grant this to a
    /// sub-delegate. Without it the subtree enrolls only ephemeral nodes.
    pub may_register: bool,
}

/// `aster_action_v1`: what an actor may do for the anchor principal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActionGrant {
    pub operations: BTreeSet<String>,
    /// Consumer-namespaced constraints; a consumer must match every one.
    pub resources: BTreeSet<String>,
    /// Optional lowercase-hex BLAKE3 digest of the one request authorized.
    #[serde(
        default,
        deserialize_with = "some",
        skip_serializing_if = "Option::is_none"
    )]
    pub request: Option<String>,
    pub mandatory: BTreeSet<String>,
}

/// The complete payload of every link. Exactly one extension is present and it
/// must match the header `typ`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct LinkPayload {
    pub iss: String,
    pub sub: String,
    pub cnf: Confirmation,
    pub aud: Audience,
    pub jti: String,
    pub iat: u64,
    #[serde(
        default,
        deserialize_with = "some",
        skip_serializing_if = "Option::is_none"
    )]
    pub nbf: Option<u64>,
    pub exp: u64,
    #[serde(
        default,
        deserialize_with = "some",
        skip_serializing_if = "Option::is_none"
    )]
    pub aster_delegation_v1: Option<DelegationGrant>,
    #[serde(
        default,
        deserialize_with = "some",
        skip_serializing_if = "Option::is_none"
    )]
    pub aster_enrollment_v1: Option<EnrollmentExtensionV1>,
    #[serde(
        default,
        deserialize_with = "some",
        skip_serializing_if = "Option::is_none"
    )]
    pub aster_action_v1: Option<ActionGrant>,
    #[serde(
        default,
        deserialize_with = "some",
        skip_serializing_if = "Option::is_none"
    )]
    pub aster_locality_v1: Option<LocalityClaims>,
    /// On an enrollment link only: the compact locality link the same issuer
    /// signed for the same subject, audience and validity. The holder
    /// publishes it, with the credential's ancestor links, as its locality
    /// statement; the credential itself is never published.
    #[serde(
        default,
        deserialize_with = "some",
        skip_serializing_if = "Option::is_none"
    )]
    pub aster_locality_statement: Option<String>,
}

/// An explicit `null` is not the same as omission: it cannot erase a value.
pub(crate) fn some<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}

/// Required member whose value may be `null`.
fn present<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    Option::<T>::deserialize(deserializer)
}

/// The kind-specific part of a decoded link.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum LinkBody {
    Delegation(DelegationGrant),
    Enrollment(EnrollmentExtensionV1),
    Action(ActionGrant),
    Locality(LocalityClaims),
}

/// A link that passed G1: well-formed, keys parsed, limits checked. Not yet
/// trusted.
#[derive(Debug, Clone)]
pub(crate) struct DecodedLink {
    pub kind: LinkKind,
    pub iss: [u8; 32],
    pub sub: [u8; 32],
    pub cnf: Option<[u8; 32]>,
    pub aud: Audience,
    pub jti: String,
    pub iat: u64,
    pub nbf: Option<u64>,
    pub exp: u64,
    pub body: LinkBody,
    /// An enrollment link's embedded locality link (`aster_locality_statement`).
    pub locality_statement: Option<String>,
}

impl DecodedLink {
    pub fn delegation(&self) -> Option<&DelegationGrant> {
        match &self.body {
            LinkBody::Delegation(grant) => Some(grant),
            _ => None,
        }
    }

    pub fn mandatory(&self) -> impl Iterator<Item = &String> {
        let (set, list): (Option<&BTreeSet<String>>, &[String]) = match &self.body {
            LinkBody::Delegation(grant) => (Some(&grant.mandatory), &[]),
            LinkBody::Action(grant) => (Some(&grant.mandatory), &[]),
            LinkBody::Enrollment(extension) => (None, &extension.mandatory),
            LinkBody::Locality(_) => (None, &[]),
        };
        set.into_iter().flatten().chain(list)
    }

    /// Whether this link asks for, or grants, persistent registration.
    pub fn registers(&self) -> bool {
        match &self.body {
            LinkBody::Delegation(grant) => grant.may_register,
            LinkBody::Enrollment(extension) => extension.registration == Registration::Persistent,
            LinkBody::Action(_) | LinkBody::Locality(_) => false,
        }
    }

    /// Scopes, tags, attribute keys and operations claimed by this link,
    /// whichever kind it is. Unknown (`None`) sets claim nothing.
    pub fn claimed(&self) -> Claimed<'_> {
        let empty = || Box::new(std::iter::empty()) as Box<dyn Iterator<Item = &String>>;
        match &self.body {
            LinkBody::Delegation(grant) => Claimed {
                scopes: Box::new(grant.scopes.iter()),
                tags: Box::new(grant.tags.iter()),
                attribute_keys: Box::new(grant.attribute_keys.iter()),
                operations: Box::new(grant.operations.iter()),
            },
            LinkBody::Enrollment(extension) => Claimed {
                scopes: Box::new(extension.claims.scopes.iter().flatten()),
                tags: Box::new(extension.claims.tags.iter().flatten()),
                attribute_keys: Box::new(extension.claims.attributes.iter().flat_map(|a| a.keys())),
                operations: empty(),
            },
            LinkBody::Action(grant) => Claimed {
                scopes: empty(),
                tags: empty(),
                attribute_keys: empty(),
                operations: Box::new(grant.operations.iter()),
            },
            LinkBody::Locality(claims) => Claimed {
                scopes: empty(),
                tags: empty(),
                attribute_keys: Box::new(LOCALITY_KEYS.iter().take(if claims.zone.is_some() {
                    2
                } else {
                    1
                })),
                operations: empty(),
            },
        }
    }
}

pub(crate) struct Claimed<'a> {
    pub scopes: Box<dyn Iterator<Item = &'a String> + 'a>,
    pub tags: Box<dyn Iterator<Item = &'a String> + 'a>,
    pub attribute_keys: Box<dyn Iterator<Item = &'a String> + 'a>,
    pub operations: Box<dyn Iterator<Item = &'a String> + 'a>,
}

/// G1 for one link's header type and payload JSON (already duplicate-checked).
pub(crate) fn decode_link(typ: &str, payload: &[u8]) -> Result<DecodedLink, ChainError> {
    let kind = LinkKind::from_jwt_type(typ).ok_or(ChainError::Header)?;
    let payload: LinkPayload =
        serde_json::from_slice(payload).map_err(|_| ChainError::Malformed)?;
    // Only an enrollment link may embed a locality statement.
    if kind != LinkKind::Enrollment && payload.aster_locality_statement.is_some() {
        return Err(ChainError::Malformed);
    }
    let body = match (
        kind,
        payload.aster_delegation_v1,
        payload.aster_enrollment_v1,
        payload.aster_action_v1,
        payload.aster_locality_v1,
    ) {
        (LinkKind::Delegation, Some(grant), None, None, None) => LinkBody::Delegation(grant),
        (LinkKind::Enrollment, None, Some(extension), None, None) => {
            LinkBody::Enrollment(extension)
        }
        (LinkKind::Action, None, None, Some(grant), None) => LinkBody::Action(grant),
        (LinkKind::Locality, None, None, None, Some(claims)) => LinkBody::Locality(claims),
        _ => return Err(ChainError::Malformed),
    };
    let iss = parse_key(&payload.iss).ok_or(ChainError::Malformed)?;
    let sub = parse_key(&payload.sub).ok_or(ChainError::Malformed)?;
    let audiences: Vec<&str> = payload.aud.values().collect();
    if audiences.is_empty()
        || audiences.len() > MAX_AUDIENCES
        || !audiences.iter().all(|value| valid_string(value))
        || !valid_jti(&payload.jti)
    {
        return Err(ChainError::Malformed);
    }
    let link = DecodedLink {
        kind,
        iss,
        sub,
        cnf: payload.cnf.key(),
        aud: payload.aud,
        jti: payload.jti,
        iat: payload.iat,
        nbf: payload.nbf,
        exp: payload.exp,
        body,
        locality_statement: payload.aster_locality_statement,
    };
    check_body_limits(&link.body)?;
    if link
        .locality_statement
        .as_deref()
        .is_some_and(|text| text.is_empty() || text.len() > MAX_LINK_BYTES || text.contains('~'))
    {
        return Err(ChainError::Malformed);
    }
    Ok(link)
}

const MAX_AUDIENCES: usize = 16;

fn check_body_limits(body: &LinkBody) -> Result<(), ChainError> {
    let lists_ok = |lists: &[&BTreeSet<String>]| {
        lists
            .iter()
            .all(|list| list.len() <= MAX_LIST_ENTRIES && list.iter().all(|v| valid_string(v)))
    };
    let ok = match body {
        LinkBody::Delegation(grant) => {
            lists_ok(&[
                &grant.audiences,
                &grant.scopes,
                &grant.tags,
                &grant.attribute_keys,
                &grant.operations,
                &grant.mandatory,
            ]) && !grant.may_issue.contains(&LinkKind::Locality)
                && grant.subjects.as_ref().is_none_or(|subjects| {
                    subjects.len() <= MAX_LIST_ENTRIES
                        && subjects.iter().all(|key| parse_key(key).is_some())
                })
        }
        LinkBody::Action(grant) => {
            lists_ok(&[&grant.operations, &grant.resources, &grant.mandatory])
                && grant.request.as_deref().is_none_or(valid_digest)
        }
        LinkBody::Locality(claims) => claims.declared().is_some(),
        LinkBody::Enrollment(extension) => {
            let claims = &extension.claims;
            lists_ok(&[
                claims.scopes.as_ref().unwrap_or(&BTreeSet::new()),
                claims.tags.as_ref().unwrap_or(&BTreeSet::new()),
            ]) && extension.mandatory.len() <= MAX_LIST_ENTRIES
                && extension.mandatory.iter().all(|v| valid_string(v))
                && claims.attributes.as_ref().is_none_or(|attributes| {
                    attributes.len() <= MAX_LIST_ENTRIES
                        && attributes.iter().all(|(key, value)| {
                            valid_string(key)
                                && match value {
                                    TypedAttribute::Float(value) => value.is_finite(),
                                    TypedAttribute::String(value) => valid_string(value),
                                    _ => true,
                                }
                        })
                })
        }
    };
    ok.then_some(()).ok_or(ChainError::Malformed)
}

fn valid_string(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_STRING_BYTES && !value.chars().any(char::is_control)
}

pub(crate) fn valid_jti(value: &str) -> bool {
    (16..=64).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Canonical lowercase hex of a valid, non-weak Ed25519 point in its RFC 8032
/// encoding. Merely decoding 32 bytes is insufficient.
pub(crate) fn parse_key(value: &str) -> Option<[u8; 32]> {
    let bytes: [u8; 32] = hex::decode(value).ok()?.try_into().ok()?;
    (hex::encode(bytes) == value && valid_key(&bytes)).then_some(bytes)
}

pub(crate) fn valid_key(value: &[u8; 32]) -> bool {
    let Ok(key) = ed25519_dalek::VerifyingKey::from_bytes(value) else {
        return false;
    };
    // Dalek accepts ZIP-215 point encodings; this profile selects RFC 8032
    // canonical encodings instead. Recompression uses the maintained curve
    // implementation and rejects aliases without implementing curve arithmetic.
    !key.is_weak() && key.to_edwards().compress().as_bytes() == value
}
