//! The example end to end, without a browser: HTTP requests into the edge's
//! router (Salvo's in-process client), the software authenticator in place
//! of the browser's passkey, and the real fleet behind it (root, identity
//! provider over HTTPS, service and edge over iroh).
//!
//! - OIDC: the provider's token is forwarded and accepted; `publish` (which
//!   needs a passkey sign-in) refuses it.
//! - WebAuthn: register, sign in, and the minted token is accepted by every
//!   method, `publish` included.
//! - Tampered assertions (bad signature, wrong origin, wrong challenge,
//!   replayed counter) open no session.

use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use gateway_identity::edge::{Edge, SESSION_COOKIE};
use gateway_identity::fleet::Fleet;
use gateway_identity::idp;
use gateway_identity::service::NotesClient;
use gateway_identity::webauthn::soft::{Assertion, SoftAuthenticator};
use gateway_identity::webauthn::RelyingParty;
use salvo::http::StatusCode;
use salvo::prelude::*;
use salvo::test::{ResponseExt, TestClient};
use serde_json::{json, Value};

const ORIGIN: &str = "http://localhost:8080";
const URL: &str = "http://localhost:8080";

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

async fn post(service: &Service, path: &str, body: Value) -> (StatusCode, Value, Option<String>) {
    let mut res = TestClient::post(format!("{URL}{path}"))
        .json(&body)
        .send(service)
        .await;
    let session = res
        .cookie(SESSION_COOKIE)
        .map(|cookie| cookie.value().to_string());
    let status = res.status_code.unwrap_or(StatusCode::OK);
    let body: Value = res.take_json().await.unwrap();
    (status, body, session)
}

enum Auth<'a> {
    Bearer(&'a str),
    Session(&'a str),
    None,
}

async fn call(service: &Service, method: &str, auth: Auth<'_>) -> (StatusCode, Value) {
    let mut request = TestClient::get(format!("{URL}/api/{method}"));
    request = match auth {
        Auth::Bearer(token) => request.add_header("authorization", format!("Bearer {token}"), true),
        Auth::Session(id) => request.add_header("cookie", format!("{SESSION_COOKIE}={id}"), true),
        Auth::None => request,
    };
    let mut res = request.send(service).await;
    let status = res.status_code.unwrap_or(StatusCode::OK);
    (status, res.take_json().await.unwrap())
}

async fn register(service: &Service, user: &str, authenticator: &mut SoftAuthenticator) {
    let (status, options, _) =
        post(service, "/webauthn/register/options", json!({"user": user})).await;
    assert_eq!(status, StatusCode::OK, "{options}");
    assert_eq!(options["rp"]["id"], "localhost");
    let challenge = URL_SAFE_NO_PAD
        .decode(options["challenge"].as_str().unwrap())
        .unwrap();
    let (client_data, attestation) = authenticator.register(&challenge, ORIGIN);
    let (status, reply, _) = post(
        service,
        "/webauthn/register",
        json!({"user": user, "clientDataJSON": b64(&client_data),
               "attestationObject": b64(&attestation)}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
}

/// Ask for login options; return the challenge.
async fn login_challenge(service: &Service, user: &str) -> Vec<u8> {
    let (status, options, _) =
        post(service, "/webauthn/login/options", json!({"user": user})).await;
    assert_eq!(status, StatusCode::OK, "{options}");
    URL_SAFE_NO_PAD
        .decode(options["challenge"].as_str().unwrap())
        .unwrap()
}

/// Send `assertion` to sign `user` in: the status and the session, if any.
async fn login(service: &Service, user: &str, a: &Assertion) -> (StatusCode, Option<String>) {
    let (status, _, session) = post(
        service,
        "/webauthn/login",
        json!({"user": user, "credentialId": b64(&a.credential_id),
               "clientDataJSON": b64(&a.client_data_json),
               "authenticatorData": b64(&a.authenticator_data),
               "signature": b64(&a.signature)}),
    )
    .await;
    (status, session)
}

#[tokio::test]
async fn browser_users_reach_the_service_through_the_edge() {
    tokio::time::timeout(Duration::from_secs(180), async {
        let state = tempfile::tempdir().unwrap();
        let fleet = Fleet::start(state.path()).await.unwrap();
        let edge_id = fleet.edge.id().to_string();
        let rp = RelyingParty::new("localhost", ORIGIN);
        let provider = fleet.idp.clone();
        let edge = Edge::new(
            NotesClient::new(fleet.edge.rpc()),
            fleet.edge.user_token_issuer().unwrap(),
            rp,
        )
        .with_sign_in(Arc::new(move |user: &str| {
            provider.sign_in(user, &["readers", "writers"])
        }));
        let service = Service::new(Arc::new(edge).router());

        // The page.
        let mut page = TestClient::get(format!("{URL}/")).send(&service).await;
        assert!(page
            .take_string()
            .await
            .unwrap()
            .contains("navigator.credentials"));

        // No identity at all.
        assert_eq!(
            call(&service, "whoami", Auth::None).await.0,
            StatusCode::UNAUTHORIZED
        );

        // ── OIDC: the provider's token, forwarded unchanged ────────────────
        let (status, reply, _) = post(&service, "/idp/sign-in", json!({"user": "alice"})).await;
        assert_eq!(status, StatusCode::OK);
        let token = reply["token"].as_str().unwrap().to_string();
        let (status, alice) = call(&service, "read", Auth::Bearer(&token)).await;
        assert_eq!(status, StatusCode::OK, "{alice}");
        assert_eq!(alice["principal"], format!("{}|alice", idp::ISSUER));
        assert_eq!(alice["actor"], edge_id);
        assert_eq!(alice["auth_methods"], "pwd");
        assert_eq!(alice["roles"], "notes.read,notes.write");
        // notes.write is held, but publish needs a passkey sign-in.
        let (status, refused) = call(&service, "publish", Auth::Bearer(&token)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
        // A token the provider never signed.
        let mut forged = token.clone().into_bytes();
        let at = forged.len() - 10;
        forged[at] = if forged[at] == b'A' { b'B' } else { b'A' };
        let forged = String::from_utf8(forged).unwrap();
        assert_eq!(
            call(&service, "whoami", Auth::Bearer(&forged)).await.0,
            StatusCode::UNAUTHORIZED
        );

        // ── WebAuthn: the edge's ceremony, then minted tokens ─────────────
        let mut passkey = SoftAuthenticator::new("localhost");
        register(&service, "bob", &mut passkey).await;
        let challenge = login_challenge(&service, "bob").await;
        let (status, session) = login(&service, "bob", &passkey.assert(&challenge, ORIGIN)).await;
        assert_eq!(status, StatusCode::OK);
        let session = session.expect("a session cookie");
        for method in ["whoami", "read", "publish"] {
            let (status, bob) = call(&service, method, Auth::Session(&session)).await;
            assert_eq!(status, StatusCode::OK, "{method}: {bob}");
            assert_eq!(bob["principal"], format!("{edge_id}|bob"));
            assert_eq!(bob["actor"], edge_id);
            assert_eq!(bob["auth_methods"], "webauthn");
        }
        assert_eq!(
            call(&service, "publish", Auth::Session("not-a-session"))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );

        // ── Tampered assertions open no session ──────────────────────────
        // A bad signature.
        let challenge = login_challenge(&service, "bob").await;
        let mut bad = passkey.assert(&challenge, ORIGIN);
        let last = bad.signature.len() - 1;
        bad.signature[last] ^= 1;
        assert_eq!(
            login(&service, "bob", &bad).await,
            (StatusCode::UNAUTHORIZED, None)
        );
        // The wrong origin.
        let challenge = login_challenge(&service, "bob").await;
        let wrong_origin = passkey.assert(&challenge, "http://evil.example");
        assert_eq!(
            login(&service, "bob", &wrong_origin).await,
            (StatusCode::UNAUTHORIZED, None)
        );
        // The wrong challenge.
        let _issued = login_challenge(&service, "bob").await;
        let wrong_challenge = passkey.assert(&[9u8; 32], ORIGIN);
        assert_eq!(
            login(&service, "bob", &wrong_challenge).await,
            (StatusCode::UNAUTHORIZED, None)
        );
        // A replayed assertion: its challenge was used up.
        let challenge = login_challenge(&service, "bob").await;
        let once = passkey.assert(&challenge, ORIGIN);
        assert_eq!(login(&service, "bob", &once).await.0, StatusCode::OK);
        assert_eq!(
            login(&service, "bob", &once).await,
            (StatusCode::UNAUTHORIZED, None)
        );
        // A cloned authenticator replaying an old counter on a fresh challenge.
        passkey.set_counter(1);
        let challenge = login_challenge(&service, "bob").await;
        let cloned = passkey.assert(&challenge, ORIGIN);
        assert_eq!(
            login(&service, "bob", &cloned).await,
            (StatusCode::UNAUTHORIZED, None)
        );
    })
    .await
    .expect("test timed out");
}
