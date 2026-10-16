//! The Aster service behind the edge. It never sees a browser: every call
//! comes from the edge over iroh, carrying the user's JWT, and the service
//! authorises the user from it. `requires` checks the user's roles (from the
//! identity provider's groups, or the roles the edge minted); the handlers
//! read who the user is, which edge forwarded the call, and how the user
//! signed in from the [`CallContext`].

use aster::rpc::{require_role, CallContext, AMR_WEBAUTHN};
use fory_derive::ForyStruct;

/// Roles the service checks.
pub mod roles {
    pub const READ: &str = "notes.read";
    pub const WRITE: &str = "notes.write";
}

/// Who called, as the service sees it.
#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq, serde::Serialize)]
#[aster(wire = "gateway_example/Caller")]
pub struct Caller {
    /// `iss|sub` of the user's token.
    pub principal: String,
    /// The edge's node id.
    pub actor: String,
    /// How the user signed in (`amr`), comma-separated.
    pub auth_methods: String,
    /// The user's roles, as Gate 3 checked them.
    pub roles: String,
    /// What the method did.
    pub result: String,
}

fn caller(ctx: &CallContext, result: &str) -> Caller {
    Caller {
        principal: ctx.principal().into(),
        actor: ctx.actor().unwrap_or_default().into(),
        auth_methods: ctx.auth_methods().join(","),
        roles: ctx.attribute("aster.role").unwrap_or_default().into(),
        result: result.into(),
    }
}

#[aster::service(name = "Notes", version = 1)]
pub trait Notes {
    /// Open to every caller: who am I?
    async fn whoami(&self, ctx: &CallContext) -> aster::Result<Caller>;

    /// Needs `notes.read`.
    #[rpc(requires = require_role(roles::READ))]
    async fn read(&self, ctx: &CallContext) -> aster::Result<Caller>;

    /// Needs `notes.write`, and a passkey sign-in (`amr` contains
    /// `webauthn`): an OIDC user with the role is still refused.
    #[rpc(requires = require_role(roles::WRITE))]
    async fn publish(&self, ctx: &CallContext) -> aster::Result<Caller>;
}

pub struct NotesImpl;

#[aster::rpc::async_trait]
impl Notes for NotesImpl {
    async fn whoami(&self, ctx: &CallContext) -> aster::Result<Caller> {
        Ok(caller(ctx, "hello"))
    }

    async fn read(&self, ctx: &CallContext) -> aster::Result<Caller> {
        Ok(caller(ctx, "3 notes"))
    }

    async fn publish(&self, ctx: &CallContext) -> aster::Result<Caller> {
        ctx.require_auth_method(AMR_WEBAUTHN)?;
        Ok(caller(ctx, "published"))
    }
}
