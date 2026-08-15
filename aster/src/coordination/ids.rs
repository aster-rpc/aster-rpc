//! Strict identifier and resource-key validation (§4.1, §4.2).
//!
//! These types exist because the public `aster::NodeId` is a hex-string wrapper
//! whose constructor does not validate length. Coordination compares node
//! identities to decide who holds a lease and who may change an ACL, so it must
//! never compare an unvalidated string: two different strings that both "look
//! like" a node id would be two different holders.
//!
//! Validation happens once, at the boundary, and the validated types are what
//! the rest of the subsystem passes around. There is deliberately no
//! `from_bytes_unchecked`.

use std::fmt;

/// §12.1 sub-limits. Public because the charge arithmetic in §12 is defined in
/// terms of them.
pub const MAX_NAMESPACE_BYTES: usize = 64;
pub const MAX_KEY_BYTES: usize = 512;

/// Why an identifier or resource key was rejected. Carries enough detail to be
/// actionable in a log line without echoing the offending bytes, which may be
/// attacker-controlled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdError {
    /// Wrong length for a fixed-width identifier.
    Length { expected: usize, actual: usize },
    /// A hex string that was not exactly `2 * expected` hex digits.
    HexLength { expected: usize, actual: usize },
    /// A non-hex byte in a hex string.
    HexDigit,
    /// Namespace was empty or over `MAX_NAMESPACE_BYTES`.
    NamespaceLength { actual: usize },
    /// Namespace contained a byte outside `[a-z0-9._-]`, or began with one
    /// that is not `[a-z0-9]`.
    NamespaceCharset,
    /// Key was empty or over `MAX_KEY_BYTES`.
    KeyLength { actual: usize },
}

impl fmt::Display for IdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IdError::Length { expected, actual } => {
                write!(f, "expected {expected} bytes, got {actual}")
            }
            IdError::HexLength { expected, actual } => {
                write!(f, "expected {} hex digits, got {actual}", expected * 2)
            }
            IdError::HexDigit => write!(f, "non-hex digit"),
            IdError::NamespaceLength { actual } => write!(
                f,
                "namespace must be 1-{MAX_NAMESPACE_BYTES} bytes, got {actual}"
            ),
            IdError::NamespaceCharset => {
                write!(f, "namespace must match [a-z0-9][a-z0-9._-]{{0,63}}")
            }
            IdError::KeyLength { actual } => {
                write!(f, "key must be 1-{MAX_KEY_BYTES} bytes, got {actual}")
            }
        }
    }
}

impl std::error::Error for IdError {}

/// Decode exactly `N` bytes from a hex string, accepting either case.
///
/// Case-insensitive on input but not on output: `CoordinationGroupId` renders
/// as lowercase, so a config written in upper case and one written in lower
/// case name the same group and log the same way.
fn decode_hex<const N: usize>(s: &str) -> Result<[u8; N], IdError> {
    let bytes = s.as_bytes();
    if bytes.len() != N * 2 {
        return Err(IdError::HexLength {
            expected: N,
            actual: bytes.len(),
        });
    }
    let mut out = [0u8; N];
    for (i, chunk) in bytes.chunks_exact(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16).ok_or(IdError::HexDigit)?;
        let lo = (chunk[1] as char).to_digit(16).ok_or(IdError::HexDigit)?;
        out[i] = (hi * 16 + lo) as u8;
    }
    Ok(out)
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).expect("nibble"));
        s.push(char::from_digit((b & 0x0f) as u32, 16).expect("nibble"));
    }
    s
}

macro_rules! fixed_id {
    ($name:ident, $len:expr, $doc:expr) => {
        #[doc = $doc]
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name([u8; $len]);

        impl $name {
            pub const LEN: usize = $len;

            /// The only constructor from raw bytes. Takes a fixed-size array,
            /// so a wrong length is a compile error rather than a runtime one.
            pub const fn from_array(bytes: [u8; $len]) -> Self {
                Self(bytes)
            }

            /// Validating constructor for a slice of unknown length.
            pub fn from_slice(bytes: &[u8]) -> Result<Self, IdError> {
                if bytes.len() != $len {
                    return Err(IdError::Length {
                        expected: $len,
                        actual: bytes.len(),
                    });
                }
                let mut out = [0u8; $len];
                out.copy_from_slice(bytes);
                Ok(Self(out))
            }

            /// Strict hex parsing, case-insensitive.
            pub fn parse_hex(s: &str) -> Result<Self, IdError> {
                decode_hex::<$len>(s).map(Self)
            }

            pub fn as_bytes(&self) -> &[u8; $len] {
                &self.0
            }

            pub fn to_vec(&self) -> Vec<u8> {
                self.0.to_vec()
            }

            /// Lowercase hex, the one rendering used in config and logs.
            pub fn to_hex(&self) -> String {
                to_hex(&self.0)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.to_hex())
            }
        }

        // Debug renders the same hex rather than a byte array: these appear in
        // assertion failures constantly, and `[110, 47, ...]` is unreadable.
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.to_hex())
            }
        }
    };
}

fixed_id!(
    CoordinationGroupId,
    32,
    "32 random bytes from the OS CSPRNG (§4.1).\n\nNever inferred from membership or an application name: every fence carries\nit, so a fence from a disaster-recovery replacement group is invalid even\nwhen its numeric epoch collides."
);
fixed_id!(
    CoordinationNodeId,
    32,
    "32 Ed25519 public-key bytes (§4.1).\n\nDistinct from `aster::NodeId`, which is an unvalidated hex-string wrapper."
);
impl CoordinationNodeId {
    /// The all-zero placeholder.
    ///
    /// Exists only because OpenRaft's `NodeId` bound requires `Default`, and
    /// this type is the type config's `NodeId`. It is **not** a node: every
    /// real id is an Ed25519 public key derived from a secret, and no keygen
    /// produces all zeros. (All-zero does decode as the curve's identity
    /// element, so the claim is "no generated key is this", not "this is
    /// undecodable" — which is why the guard below is a check and not a type.)
    pub const PLACEHOLDER: Self = Self([0u8; 32]);

    /// Whether this is the `Default` placeholder rather than a real identity.
    ///
    /// Call this anywhere a NodeId crosses into an authority decision. A
    /// defaulted id reaching membership or an ACL check means something
    /// constructed an identity it never actually learned.
    pub fn is_placeholder(&self) -> bool {
        self.0 == [0u8; 32]
    }
}

/// Required by OpenRaft's `NodeId` bound; see [`CoordinationNodeId::PLACEHOLDER`].
///
/// Deliberately implemented only for this one id type. The others have no such
/// requirement, and giving them a `Default` would invite exactly the
/// identity-by-omission bug this module exists to prevent.
impl Default for CoordinationNodeId {
    fn default() -> Self {
        Self::PLACEHOLDER
    }
}

fixed_id!(
    ClientId,
    16,
    "16 random bytes identifying one client journal (§4.1)."
);
fixed_id!(
    OperationId,
    16,
    "16 random bytes identifying one membership operation (§4.1)."
);

/// A validated `(namespace, key)` pair (§4.2).
///
/// Equality is over the raw bytes with **no normalization**: v1 has no
/// wildcards, no inherited grants, no path semantics, and no prefix grants, so
/// two keys are the same key only when their bytes are identical.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResourceKey {
    namespace: String,
    key: Vec<u8>,
}

impl ResourceKey {
    /// Exact rules, not prefix based:
    ///
    /// - `namespace` is 1-64 ASCII bytes matching `[a-z0-9][a-z0-9._-]{0,63}`;
    /// - `key` is 1-512 bytes.
    pub fn new(namespace: impl Into<String>, key: impl Into<Vec<u8>>) -> Result<Self, IdError> {
        let namespace = namespace.into();
        let key = key.into();

        let nb = namespace.as_bytes();
        if nb.is_empty() || nb.len() > MAX_NAMESPACE_BYTES {
            return Err(IdError::NamespaceLength { actual: nb.len() });
        }
        // First byte is deliberately stricter than the rest: a namespace may
        // not begin with `.`, `-` or `_`, which keeps leading-dot names from
        // looking like hidden files anywhere they are written to disk.
        if !nb[0].is_ascii_lowercase() && !nb[0].is_ascii_digit() {
            return Err(IdError::NamespaceCharset);
        }
        for &b in &nb[1..] {
            let ok =
                b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-');
            if !ok {
                return Err(IdError::NamespaceCharset);
            }
        }

        if key.is_empty() || key.len() > MAX_KEY_BYTES {
            return Err(IdError::KeyLength { actual: key.len() });
        }

        Ok(Self { namespace, key })
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub fn key(&self) -> &[u8] {
        &self.key
    }

    /// The §12.1 charge input: `len(namespace) + len(key)`.
    pub fn charge_len(&self) -> u64 {
        (self.namespace.len() + self.key.len()) as u64
    }
}

impl fmt::Debug for ResourceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The key is arbitrary bytes and may not be UTF-8, so it renders as
        // hex. Truncated, because a 512-byte key in a log line is noise.
        let k = if self.key.len() > 16 {
            format!("{}..({} bytes)", to_hex(&self.key[..16]), self.key.len())
        } else {
            to_hex(&self.key)
        };
        write!(f, "{}/{}", self.namespace, k)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips_and_is_case_insensitive_on_input() {
        let bytes = [0xabu8; 32];
        let id = CoordinationGroupId::from_array(bytes);
        assert_eq!(id.to_hex(), "ab".repeat(32));
        assert_eq!(CoordinationGroupId::parse_hex(&id.to_hex()).unwrap(), id);
        // Upper case parses to the same id, and renders back lowercase.
        let upper = CoordinationGroupId::parse_hex(&"AB".repeat(32)).unwrap();
        assert_eq!(upper, id);
        assert_eq!(upper.to_hex(), "ab".repeat(32));
    }

    /// Golden vectors: the exact bytes a given hex string must produce. These
    /// pin the nibble order, which a hand-rolled decoder gets backwards easily
    /// and which no round-trip test can catch.
    #[test]
    fn hex_golden_vectors_pin_nibble_order() {
        let id = ClientId::parse_hex("000102030405060708090a0b0c0d0e0f").unwrap();
        assert_eq!(
            id.as_bytes(),
            &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]
        );
        // 0xf0 vs 0x0f: a swapped decoder round-trips but fails here.
        let id = ClientId::parse_hex("f0000000000000000000000000000000").unwrap();
        assert_eq!(id.as_bytes()[0], 0xf0);
        assert_eq!(ClientId::from_array([0xf0; 16]).to_hex(), "f0".repeat(16));
    }

    #[test]
    fn wrong_length_is_rejected_everywhere() {
        assert_eq!(
            CoordinationNodeId::from_slice(&[0u8; 31]),
            Err(IdError::Length {
                expected: 32,
                actual: 31
            })
        );
        assert_eq!(
            CoordinationNodeId::from_slice(&[0u8; 33]),
            Err(IdError::Length {
                expected: 32,
                actual: 33
            })
        );
        assert!(matches!(
            CoordinationNodeId::parse_hex(&"ab".repeat(31)),
            Err(IdError::HexLength { .. })
        ));
        assert_eq!(
            ClientId::parse_hex(&"zz".repeat(16)),
            Err(IdError::HexDigit)
        );
        // The empty string is a length error, not a digit error.
        assert!(matches!(
            ClientId::parse_hex(""),
            Err(IdError::HexLength { .. })
        ));
    }

    #[test]
    fn namespace_charset_is_exact() {
        for ok in [
            "portal.roaming",
            "a",
            "0",
            "a-b_c.d",
            &"a".repeat(MAX_NAMESPACE_BYTES),
        ] {
            assert!(
                ResourceKey::new(ok, b"k".to_vec()).is_ok(),
                "{ok:?} should be valid"
            );
        }
        for bad in [
            "",                                   // empty
            "Portal",                             // upper case
            ".hidden",                            // leading dot
            "-lead",                              // leading dash
            "_lead",                              // leading underscore
            "has space",                          // space
            "sl/ash",                             // path separator
            "uni\u{e9}",                          // non-ASCII
            &"a".repeat(MAX_NAMESPACE_BYTES + 1), // too long
        ] {
            assert!(
                ResourceKey::new(bad, b"k".to_vec()).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn key_length_bounds_are_inclusive() {
        assert!(ResourceKey::new("ns", vec![0u8; 1]).is_ok());
        assert!(ResourceKey::new("ns", vec![0u8; MAX_KEY_BYTES]).is_ok());
        assert_eq!(
            ResourceKey::new("ns", Vec::new()),
            Err(IdError::KeyLength { actual: 0 })
        );
        assert_eq!(
            ResourceKey::new("ns", vec![0u8; MAX_KEY_BYTES + 1]),
            Err(IdError::KeyLength {
                actual: MAX_KEY_BYTES + 1
            })
        );
    }

    /// §4.2: equality is over raw bytes with no normalization. A key that
    /// differs only in trailing NUL, or in case, is a *different* key.
    #[test]
    fn resource_key_equality_does_not_normalize() {
        let a = ResourceKey::new("ns", b"k".to_vec()).unwrap();
        let b = ResourceKey::new("ns", b"k\0".to_vec()).unwrap();
        assert_ne!(a, b, "trailing NUL must not be trimmed");

        // And the namespace/key split is not flattened: ("a","bc") must not
        // equal ("ab","c"). This is the same prefix collision that broke the
        // storage ordering key twice.
        let x = ResourceKey::new("ab", b"c".to_vec()).unwrap();
        let y = ResourceKey::new("abc", b"".to_vec());
        assert!(y.is_err(), "an empty key is invalid, so cannot collide");
        assert_ne!(x, ResourceKey::new("a", b"bc".to_vec()).unwrap());
    }

    #[test]
    fn charge_len_matches_the_section_12_definition() {
        let k = ResourceKey::new("portal.roaming", vec![7u8; 100]).unwrap();
        assert_eq!(k.charge_len(), "portal.roaming".len() as u64 + 100);
        // The declared maximum, which §12.1 rounds to 576.
        let max =
            ResourceKey::new("n".repeat(MAX_NAMESPACE_BYTES), vec![0u8; MAX_KEY_BYTES]).unwrap();
        assert_eq!(max.charge_len(), 576);
    }

    /// The `Default` that OpenRaft's bound forces is a recognisable
    /// placeholder, and only `CoordinationNodeId` has one.
    #[test]
    fn only_the_node_id_has_a_default_and_it_is_a_placeholder() {
        let d = CoordinationNodeId::default();
        assert!(d.is_placeholder());
        assert_eq!(d, CoordinationNodeId::PLACEHOLDER);
        assert_eq!(d.to_hex(), "0".repeat(64));

        // Any real id is distinguishable from it.
        let real = CoordinationNodeId::from_array([1u8; 32]);
        assert!(!real.is_placeholder());
        assert_ne!(real, d);

        // A zero id parsed from config is still recognised as the placeholder:
        // the guard is on the bytes, not on how they arrived.
        let parsed = CoordinationNodeId::parse_hex(&"00".repeat(32)).unwrap();
        assert!(parsed.is_placeholder());
    }

    #[test]
    fn debug_renders_readably_without_dumping_a_long_key() {
        let k = ResourceKey::new("portal.roaming", vec![0xabu8; 512]).unwrap();
        let s = format!("{k:?}");
        assert!(s.starts_with("portal.roaming/abababab"), "{s}");
        assert!(s.contains("(512 bytes)"), "{s}");
        assert!(s.len() < 80, "debug output should stay log-sized: {s}");
    }
}
