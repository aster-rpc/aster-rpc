//! The edge: a Salvo HTTP front end that calls the Aster service over iroh on
//! behalf of each browser user.
//!
//! Per request it attaches exactly one JWT to the call
//! (`CallOptions::on_behalf_of`):
//!
//! - **OIDC.** The browser sends `Authorization: Bearer <ID token>` from the
//!   identity provider. The edge forwards it unchanged; the service verifies
//!   it with the provider's keys, which the edge publishes into the fleet.
//! - **Passkey.** The edge runs the WebAuthn ceremony itself
//!   ([`crate::webauthn`]), keeps a session for the user, and on each request
//!   mints a short-lived token with `amr: ["webauthn"]`, signed with its own
//!   node key and bounded by its issuer delegation.
//!
//! Routes (JSON bodies, binary fields base64url without padding):
//!
//! | Route | Does |
//! |---|---|
//! | `GET /` | the page |
//! | `POST /idp/sign-in` `{user}` | the stand-in provider's sign-in: `{token}` |
//! | `POST /webauthn/register/options` `{user}` | creation options |
//! | `POST /webauthn/register` `{user, clientDataJSON, attestationObject}` | store the passkey |
//! | `POST /webauthn/login/options` `{user}` | request options |
//! | `POST /webauthn/login` `{user, credentialId, clientDataJSON, authenticatorData, signature}` | a session (cookie `edge_session`) |
//! | `GET /api/whoami`, `/api/read`, `/api/publish` | the service's methods |

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aster::rpc::identity::Issuer;
use aster::rpc::{bearer_token, CallOptions, StatusCode as RpcCode, AMR_WEBAUTHN};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use salvo::http::cookie::{Cookie, SameSite};
use salvo::http::StatusCode;
use salvo::prelude::*;
use serde::Deserialize;
use serde_json::json;

use crate::service::{roles, NotesClient};
use crate::webauthn::{new_challenge, Credential, RelyingParty};

/// The page.
pub const INDEX_HTML: &str = include_str!("../static/index.html");
/// The session cookie.
pub const SESSION_COOKIE: &str = "edge_session";
/// How long a ceremony's challenge stays valid.
const CHALLENGE_TTL: Duration = Duration::from_secs(120);
/// How long a passkey session lasts.
const SESSION_TTL: Duration = Duration::from_secs(3600);
/// The lifetime of each minted token: one request's worth.
const TOKEN_TTL: Duration = Duration::from_secs(60);
/// Largest JSON body read.
const MAX_BODY: usize = 64 * 1024;
/// Longest user name.
const MAX_USER: usize = 64;

/// The stand-in provider's sign-in (`/idp/sign-in`): user → ID token.
pub type SignIn = Arc<dyn Fn(&str) -> String + Send + Sync>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ceremony {
    Register,
    Login,
}

struct Pending {
    ceremony: Ceremony,
    challenge: [u8; 32],
    expires: Instant,
}

struct User {
    handle: [u8; 16],
    credentials: Vec<Credential>,
}

struct Session {
    user: String,
    expires: Instant,
}

#[derive(Default)]
struct State {
    users: HashMap<String, User>,
    pending: HashMap<String, Pending>,
    sessions: HashMap<String, Session>,
}

/// The edge's state: the service client, its token issuer, the relying
/// party, and the passkeys and sessions it holds (in memory).
pub struct Edge {
    notes: NotesClient,
    options: CallOptions,
    issuer: Issuer,
    rp: RelyingParty,
    sign_in: Option<SignIn>,
    state: Mutex<State>,
}

/// A refusal: HTTP status and a short reason.
type Refusal = (StatusCode, String);

fn refuse(status: StatusCode, reason: impl Into<String>) -> Refusal {
    (status, reason.into())
}

fn b64(field: &str, value: &str) -> Result<Vec<u8>, Refusal> {
    URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| refuse(StatusCode::BAD_REQUEST, format!("{field} is not base64url")))
}

fn valid_user(user: &str) -> Result<(), Refusal> {
    let ok = !user.is_empty()
        && user.len() <= MAX_USER
        && user
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(())
    } else {
        Err(refuse(
            StatusCode::BAD_REQUEST,
            "user: 1-64 of A-Z a-z 0-9 - _ .",
        ))
    }
}

#[derive(Deserialize)]
struct UserBody {
    user: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RegisterBody {
    user: String,
    #[serde(rename = "clientDataJSON")]
    client_data_json: String,
    attestation_object: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoginBody {
    user: String,
    credential_id: String,
    #[serde(rename = "clientDataJSON")]
    client_data_json: String,
    authenticator_data: String,
    signature: String,
}

impl Edge {
    /// An edge calling `notes` (built once over the edge's `AsterClient`),
    /// minting with `issuer` (`AsterClient::user_token_issuer`), for the
    /// relying party `rp`.
    pub fn new(notes: NotesClient, issuer: Issuer, rp: RelyingParty) -> Self {
        Self {
            notes,
            options: CallOptions::new().deadline(Duration::from_secs(15)),
            issuer,
            rp,
            sign_in: None,
            state: Mutex::default(),
        }
    }

    /// Serve the stand-in provider's sign-in at `/idp/sign-in`.
    pub fn with_sign_in(mut self, sign_in: SignIn) -> Self {
        self.sign_in = Some(sign_in);
        self
    }

    /// The Salvo router.
    pub fn router(self: Arc<Self>) -> Router {
        let route = |path: &str, action: Action| {
            Router::with_path(path.to_string()).goal(Route {
                edge: self.clone(),
                action,
            })
        };
        Router::new()
            .goal(Route {
                edge: self.clone(),
                action: Action::Index,
            })
            .push(route("idp/sign-in", Action::SignIn))
            .push(route("webauthn/register/options", Action::RegisterOptions))
            .push(route("webauthn/register", Action::Register))
            .push(route("webauthn/login/options", Action::LoginOptions))
            .push(route("webauthn/login", Action::Login))
            .push(route("api/whoami", Action::Call(Method::Whoami)))
            .push(route("api/read", Action::Call(Method::Read)))
            .push(route("api/publish", Action::Call(Method::Publish)))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Issue a challenge for `user`'s `ceremony`, replacing any earlier one.
    fn challenge(&self, user: &str, ceremony: Ceremony) -> [u8; 32] {
        let challenge = new_challenge();
        let mut state = self.state();
        let now = Instant::now();
        state.pending.retain(|_, pending| pending.expires > now);
        state.pending.insert(
            user.to_string(),
            Pending {
                ceremony,
                challenge,
                expires: now + CHALLENGE_TTL,
            },
        );
        challenge
    }

    /// Take `user`'s pending challenge for `ceremony`: single use.
    fn take_challenge(&self, user: &str, ceremony: Ceremony) -> Result<[u8; 32], Refusal> {
        let pending = self.state().pending.remove(user);
        match pending {
            Some(p) if p.ceremony == ceremony && p.expires > Instant::now() => Ok(p.challenge),
            _ => Err(refuse(StatusCode::UNAUTHORIZED, "no ceremony in progress")),
        }
    }

    fn register_options(&self, user: &str) -> Result<serde_json::Value, Refusal> {
        valid_user(user)?;
        let challenge = self.challenge(user, Ceremony::Register);
        let mut state = self.state();
        let handle = state
            .users
            .entry(user.to_string())
            .or_insert_with(|| User {
                handle: new_challenge()[..16].try_into().unwrap(),
                credentials: Vec::new(),
            })
            .handle;
        Ok(json!({
            "challenge": URL_SAFE_NO_PAD.encode(challenge),
            "rp": {"id": self.rp.rp_id, "name": "Aster gateway example"},
            "user": {"id": URL_SAFE_NO_PAD.encode(handle), "name": user, "displayName": user},
            "alg": crate::webauthn::COSE_ES256 as i64,
            "userVerification": self.user_verification(),
        }))
    }

    fn user_verification(&self) -> &'static str {
        if self.rp.require_user_verification {
            "required"
        } else {
            "preferred"
        }
    }

    fn register(&self, body: RegisterBody) -> Result<serde_json::Value, Refusal> {
        valid_user(&body.user)?;
        let challenge = self.take_challenge(&body.user, Ceremony::Register)?;
        let credential = self
            .rp
            .verify_registration(
                &challenge,
                &b64("clientDataJSON", &body.client_data_json)?,
                &b64("attestationObject", &body.attestation_object)?,
            )
            .map_err(|e| refuse(StatusCode::UNAUTHORIZED, e.to_string()))?;
        let mut state = self.state();
        if state
            .users
            .values()
            .any(|user| user.credentials.iter().any(|c| c.id == credential.id))
        {
            return Err(refuse(
                StatusCode::CONFLICT,
                "credential already registered",
            ));
        }
        let user = state
            .users
            .get_mut(&body.user)
            .ok_or_else(|| refuse(StatusCode::UNAUTHORIZED, "unknown user"))?;
        user.credentials.push(credential);
        Ok(json!({"registered": body.user}))
    }

    fn login_options(&self, user: &str) -> Result<serde_json::Value, Refusal> {
        valid_user(user)?;
        let ids: Vec<String> = self
            .state()
            .users
            .get(user)
            .map(|u| {
                u.credentials
                    .iter()
                    .map(|c| URL_SAFE_NO_PAD.encode(&c.id))
                    .collect()
            })
            .unwrap_or_default();
        if ids.is_empty() {
            return Err(refuse(StatusCode::NOT_FOUND, "no passkey registered"));
        }
        let challenge = self.challenge(user, Ceremony::Login);
        Ok(json!({
            "challenge": URL_SAFE_NO_PAD.encode(challenge),
            "rpId": self.rp.rp_id,
            "allowCredentials": ids,
            "userVerification": self.user_verification(),
        }))
    }

    /// Verify the assertion and open a session: `(session id, body)`.
    fn login(&self, body: LoginBody) -> Result<(String, serde_json::Value), Refusal> {
        valid_user(&body.user)?;
        let challenge = self.take_challenge(&body.user, Ceremony::Login)?;
        let credential_id = b64("credentialId", &body.credential_id)?;
        let client_data_json = b64("clientDataJSON", &body.client_data_json)?;
        let authenticator_data = b64("authenticatorData", &body.authenticator_data)?;
        let signature = b64("signature", &body.signature)?;
        let mut state = self.state();
        let user = state
            .users
            .get_mut(&body.user)
            .ok_or_else(|| refuse(StatusCode::UNAUTHORIZED, "unknown user"))?;
        let credential = user
            .credentials
            .iter_mut()
            .find(|c| c.id == credential_id)
            .ok_or_else(|| refuse(StatusCode::UNAUTHORIZED, "unknown credential"))?;
        let count = self
            .rp
            .verify_assertion(
                &challenge,
                credential,
                &credential_id,
                &client_data_json,
                &authenticator_data,
                &signature,
            )
            .map_err(|e| refuse(StatusCode::UNAUTHORIZED, e.to_string()))?;
        credential.sign_count = count;
        let session = hex::encode(new_challenge());
        let now = Instant::now();
        state.sessions.retain(|_, s| s.expires > now);
        state.sessions.insert(
            session.clone(),
            Session {
                user: body.user.clone(),
                expires: now + SESSION_TTL,
            },
        );
        Ok((session, json!({"signedIn": body.user})))
    }

    /// The JWT this request is made under: the bearer token, forwarded, or
    /// one minted for the passkey session.
    fn token_for(&self, req: &Request) -> Result<String, Refusal> {
        if let Some(authorization) = req.header::<String>("authorization") {
            return bearer_token(&authorization)
                .map(str::to_string)
                .ok_or_else(|| refuse(StatusCode::UNAUTHORIZED, "not a bearer token"));
        }
        let Some(cookie) = req.cookie(SESSION_COOKIE) else {
            return Err(refuse(StatusCode::UNAUTHORIZED, "sign in first"));
        };
        let user = {
            let state = self.state();
            match state.sessions.get(cookie.value()) {
                Some(session) if session.expires > Instant::now() => session.user.clone(),
                _ => return Err(refuse(StatusCode::UNAUTHORIZED, "session expired")),
            }
        };
        // The edge's directory gives every passkey user both roles; the
        // core refuses any role outside the edge's issuer delegation.
        self.issuer
            .mint_user_token(
                &user,
                [roles::READ, roles::WRITE],
                [AMR_WEBAUTHN],
                TOKEN_TTL,
            )
            .map_err(|e| refuse(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
    }

    async fn call(&self, req: &Request, method: Method) -> Result<serde_json::Value, Refusal> {
        let token = self.token_for(req)?;
        // A per-request view of the client: cheap, no codec rebuild.
        let notes = self
            .notes
            .with_options(self.options.clone().on_behalf_of(token));
        let result = match method {
            Method::Whoami => notes.whoami().await,
            Method::Read => notes.read().await,
            Method::Publish => notes.publish().await,
        };
        match result {
            Ok(caller) => Ok(serde_json::to_value(caller).unwrap_or_default()),
            Err(error) => {
                let status = match error.rpc_code() {
                    Some(RpcCode::Unauthenticated) => StatusCode::UNAUTHORIZED,
                    Some(RpcCode::PermissionDenied) => StatusCode::FORBIDDEN,
                    _ => StatusCode::BAD_GATEWAY,
                };
                Err(refuse(status, error.to_string()))
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Method {
    Whoami,
    Read,
    Publish,
}

#[derive(Clone, Copy)]
enum Action {
    Index,
    SignIn,
    RegisterOptions,
    Register,
    LoginOptions,
    Login,
    Call(Method),
}

struct Route {
    edge: Arc<Edge>,
    action: Action,
}

async fn body<T: serde::de::DeserializeOwned>(req: &mut Request) -> Result<T, Refusal> {
    let bytes = req
        .payload_with_max_size(MAX_BODY)
        .await
        .map_err(|_| refuse(StatusCode::BAD_REQUEST, "unreadable body"))?;
    serde_json::from_slice(bytes).map_err(|e| refuse(StatusCode::BAD_REQUEST, e.to_string()))
}

impl Route {
    async fn run(
        &self,
        req: &mut Request,
        res: &mut Response,
    ) -> Result<serde_json::Value, Refusal> {
        let edge = &self.edge;
        let post = req.method() == salvo::http::Method::POST;
        let get = req.method() == salvo::http::Method::GET;
        let wrong_method = || refuse(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
        match self.action {
            Action::Index => unreachable!("served directly"),
            Action::Call(method) if get => edge.call(req, method).await,
            Action::Call(_) => Err(wrong_method()),
            _ if !post => Err(wrong_method()),
            Action::SignIn => {
                let sign_in = edge
                    .sign_in
                    .as_ref()
                    .ok_or_else(|| refuse(StatusCode::NOT_FOUND, "no stand-in provider"))?;
                let UserBody { user } = body(req).await?;
                valid_user(&user)?;
                Ok(json!({"token": sign_in(&user)}))
            }
            Action::RegisterOptions => edge.register_options(&body::<UserBody>(req).await?.user),
            Action::Register => edge.register(body(req).await?),
            Action::LoginOptions => edge.login_options(&body::<UserBody>(req).await?.user),
            Action::Login => {
                let (session, reply) = edge.login(body(req).await?)?;
                let mut cookie = Cookie::new(SESSION_COOKIE, session);
                cookie.set_http_only(true);
                cookie.set_same_site(SameSite::Strict);
                cookie.set_path("/");
                res.add_cookie(cookie);
                Ok(reply)
            }
        }
    }
}

#[salvo::async_trait]
impl Handler for Route {
    async fn handle(
        &self,
        req: &mut Request,
        _depot: &mut Depot,
        res: &mut Response,
        _ctrl: &mut FlowCtrl,
    ) {
        if let Action::Index = self.action {
            res.render(Text::Html(INDEX_HTML));
            return;
        }
        match self.run(req, res).await {
            Ok(reply) => res.render(Json(reply)),
            Err((status, reason)) => {
                res.status_code(status);
                res.render(Json(json!({"error": reason})));
            }
        }
    }
}
