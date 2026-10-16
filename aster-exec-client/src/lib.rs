//! Reusable authenticated Exec v1 client. Cloning keeps the same RPC connection.
use aster::rpc::{CallOptions, RpcConnection, StreamHeader};
pub use aster_exec::contract::*;
use serde::{Serialize, de::DeserializeOwned};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug)]
pub enum Error {
    Transport(aster::Error),
    Encoding(serde_json::Error),
    Exec(ExecErrorV1),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(e) => e.fmt(f),
            Self::Encoding(e) => e.fmt(f),
            Self::Exec(e) => e.fmt(f),
        }
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone)]
pub struct Client {
    connection: Arc<RpcConnection>,
    call_timeout: Duration,
    on_behalf_of: Option<String>,
}
impl Client {
    pub fn new(connection: RpcConnection) -> Self {
        Self {
            connection: Arc::new(connection),
            call_timeout: Duration::from_secs(30),
            on_behalf_of: None,
        }
    }
    /// Connection/setup timeouts belong to the caller; this bounds control RPCs,
    /// independently of the server-side execution timeout in each request.
    pub fn with_call_timeout(mut self, timeout: Duration) -> Self {
        self.call_timeout = timeout;
        self
    }
    /// Call on behalf of a user: every call carries `jwt` in the
    /// `aster-on-behalf-of` metadata entry. For a gateway edge holding
    /// `aster.gateway`; the target runs the call as the token's `iss|sub`
    /// under the Exec grant naming it. The unary control calls carry it
    /// (`watch` has no client method here).
    pub fn on_behalf_of(mut self, jwt: impl Into<String>) -> Self {
        self.on_behalf_of = Some(jwt.into());
        self
    }
    async fn call<A: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        request: &A,
    ) -> Result<R> {
        let header = StreamHeader {
            service: SERVICE_NAME.into(),
            method: method.into(),
            version: SERVICE_VERSION as i32,
            ..Default::default()
        };
        let bytes = serde_json::to_vec(request).map_err(Error::Encoding)?;
        let mut options = CallOptions::new().deadline(self.call_timeout);
        if let Some(jwt) = &self.on_behalf_of {
            options = options.on_behalf_of(jwt.clone());
        }
        let reply = self
            .connection
            .unary_with(&header, bytes, &options)
            .await
            .map_err(Error::Transport)?;
        let result: std::result::Result<R, ExecErrorV1> =
            serde_json::from_slice(&reply).map_err(Error::Encoding)?;
        result.map_err(Error::Exec)
    }
    pub async fn targets(&self) -> Result<Vec<TargetRefV1>> {
        self.call("targets", &()).await
    }
    pub async fn capabilities(&self, target: &TargetRefV1) -> Result<TargetCapabilitiesV1> {
        self.call("capabilities", target).await
    }
    /// Retry a failed/ambiguous submission with this exact request, including ID.
    /// This method never generates a replacement ID or retries automatically.
    pub async fn submit(&self, request: &ExecRequestV1) -> Result<JobRefV1> {
        request.validate().map_err(Error::Exec)?;
        self.call("submit", request).await
    }
    pub async fn status(&self, job: &JobRefV1) -> Result<JobStatusV1> {
        self.call("status", job).await
    }
    pub async fn output(&self, request: &OutputRequestV1) -> Result<OutputPageV1> {
        self.call("output", request).await
    }
    pub async fn cancel(&self, request: &CancelRequestV1) -> Result<CancelReceiptV1> {
        self.call("cancel", request).await
    }
}

/// Construct a direct argv request; no shell is inserted and stdin is closed.
pub fn request(
    target: impl Into<String>,
    cwd: impl Into<String>,
    executable: impl Into<String>,
    argv: Vec<String>,
) -> ExecRequestV1 {
    ExecRequestV1 {
        request_id: uuid::Uuid::new_v4().to_string(),
        target_id: target.into(),
        profile: None,
        cwd: cwd.into(),
        executable: executable.into(),
        argv,
        environment_overrides: Default::default(),
        stdin_bytes: None,
        execution_timeout_ms: None,
        output_limit_bytes: None,
        on_behalf_of: None,
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn direct_arguments_are_preserved() {
        let request = super::request(
            "target",
            "/work",
            "printf",
            vec!["a b".into(), "$(whoami)".into(), "".into()],
        );
        request.validate().unwrap();
        assert_eq!(request.argv, ["a b", "$(whoami)", ""]);
        assert!(request.stdin_bytes.is_none());
    }
}
