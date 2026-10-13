//! Ed25519 identity key helpers shared by the facade and every binding.

use ed25519_dalek::SigningKey;

/// Derive the 32-byte Ed25519 public key from a 32-byte secret seed.
pub fn public_key(secret: &[u8; 32]) -> [u8; 32] {
    SigningKey::from_bytes(secret).verifying_key().to_bytes()
}

#[cfg(test)]
mod tests {
    #[test]
    fn derives_the_rfc8032_public_key() {
        // RFC 8032 §7.1, TEST 1.
        let secret: [u8; 32] =
            hex::decode("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60")
                .unwrap()
                .try_into()
                .unwrap();
        assert_eq!(
            hex::encode(super::public_key(&secret)),
            "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"
        );
    }
}
