#![cfg(feature = "enrollment")]

use std::time::Duration;

use aster::enrollment::{
    verify_connection, EnrollmentError, JwtEnrollmentVerifier, ENROLLMENT_JWT_TYPE,
};
use aster::{AsterConfig, Node, RelayMode, SecretKey};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{Signer, SigningKey};
use serde_json::json;

const NOW: u64 = 1_800_000_000;
const ALPN: &[u8] = b"aster.enrollment.test/1";

fn config(seed: u8) -> AsterConfig {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .secret_key(SecretKey::from_bytes([seed; 32]))
        .build()
}

#[tokio::test]
async fn jwt_subject_is_bound_to_real_authenticated_connection() {
    let root = SigningKey::from_bytes(&[7; 32]);
    let subject = SigningKey::from_bytes(&[8; 32]).verifying_key().to_bytes();
    let header = json!({"alg": "EdDSA", "typ": ENROLLMENT_JWT_TYPE});
    let claims = json!({
        "iss": hex::encode(root.verifying_key().to_bytes()),
        "sub": hex::encode(subject), "aud": "build-fleet", "jti": "reusable", "iat": NOW,
        "cnf": {"jwk": {"kty": "OKP", "crv": "Ed25519", "x": URL_SAFE_NO_PAD.encode(subject)}},
        "aster_enrollment_v1": {"mandatory": []}
    });
    let message = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap()),
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
    );
    let token = format!(
        "{}.{}",
        message,
        URL_SAFE_NO_PAD.encode(root.sign(message.as_bytes()).to_bytes())
    );
    let verifier =
        JwtEnrollmentVerifier::new(root.verifying_key().to_bytes(), "build-fleet").unwrap();
    let server = Node::start_with_alpns(config(10), vec![ALPN.to_vec()])
        .await
        .unwrap();
    for seed in [8, 9, 8] {
        // Seed 8 reconnects after shutdown with the same identity/credential;
        // seed 9 presents an identical copied credential on its own connection.
        let client = Node::start(config(seed)).await.unwrap();
        client.add_peer(&server).unwrap();
        server.add_peer(&client).unwrap();
        let server_id = server.id();
        let (outbound, inbound) = tokio::time::timeout(Duration::from_secs(15), async {
            tokio::join!(client.connect(&server_id, ALPN), server.accept())
        })
        .await
        .expect("loopback handshake timed out");
        let outbound = outbound.unwrap();
        let (_, inbound) = inbound.unwrap();
        let result = verify_connection(&verifier, &token, &inbound, NOW);
        if seed == 8 {
            assert_eq!(result.unwrap().subject(), &subject);
        } else {
            assert_eq!(result.unwrap_err(), EnrollmentError::PeerKeyMismatch);
        }
        outbound.close(0, b"done".to_vec()).unwrap();
        client.shutdown().await;
    }
    server.shutdown().await;
}
