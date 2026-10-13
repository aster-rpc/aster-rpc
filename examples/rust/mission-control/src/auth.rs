//! Mission Control with role-based access control (the auth chapter).
//!
//! The same `MissionControl` service as the crate root, with `requires` on
//! each method, mirroring Python's `services_auth.py`. Served by
//! `mission-control --auth`: the server's `[enrollment]` configuration admits
//! only enrolled nodes and turns the roles in each node's credential into the
//! attributes these requirements are checked against. No code here reads a
//! credential.

use aster::rpc::{require_any_of, require_role, RequestStream, ResponseSink};

use crate::roles;
use crate::{
    Command, CommandResult, IngestResult, LogEntry, MetricPoint, MissionControlImpl, StatusRequest,
    StatusResponse, SubmitLogResult, TailRequest,
};

#[aster::service(name = "MissionControl", version = 1)]
pub trait MissionControl {
    /// Requires `ops.status`.
    #[rpc(name = "getStatus", requires = require_role(roles::STATUS))]
    async fn get_status(&self, req: StatusRequest) -> aster::Result<StatusResponse>;

    /// Open to every enrolled node.
    #[rpc(name = "submitLog")]
    async fn submit_log(&self, entry: LogEntry) -> aster::Result<SubmitLogResult>;

    /// Requires `ops.logs` or `ops.admin`.
    #[rpc(
        server_stream,
        name = "tailLogs",
        requires = require_any_of([roles::LOGS, roles::ADMIN])
    )]
    async fn tail_logs(&self, req: TailRequest, out: ResponseSink<LogEntry>) -> aster::Result<()>;

    /// Requires `ops.ingest`.
    #[rpc(client_stream, name = "ingestMetrics", requires = require_role(roles::INGEST))]
    async fn ingest_metrics(&self, reqs: RequestStream<MetricPoint>)
        -> aster::Result<IngestResult>;

    /// Requires `ops.admin`.
    #[rpc(bidi_stream, name = "runCommand", requires = require_role(roles::ADMIN))]
    async fn run_command(
        &self,
        reqs: RequestStream<Command>,
        out: ResponseSink<CommandResult>,
    ) -> aster::Result<()>;
}

/// The same behaviour as the open service; only the requirements differ.
#[aster::rpc::async_trait]
impl MissionControl for MissionControlImpl {
    async fn get_status(&self, req: StatusRequest) -> aster::Result<StatusResponse> {
        crate::MissionControl::get_status(self, req).await
    }

    async fn submit_log(&self, entry: LogEntry) -> aster::Result<SubmitLogResult> {
        crate::MissionControl::submit_log(self, entry).await
    }

    async fn tail_logs(&self, req: TailRequest, out: ResponseSink<LogEntry>) -> aster::Result<()> {
        crate::MissionControl::tail_logs(self, req, out).await
    }

    async fn ingest_metrics(
        &self,
        reqs: RequestStream<MetricPoint>,
    ) -> aster::Result<IngestResult> {
        crate::MissionControl::ingest_metrics(self, reqs).await
    }

    async fn run_command(
        &self,
        reqs: RequestStream<Command>,
        out: ResponseSink<CommandResult>,
    ) -> aster::Result<()> {
        crate::MissionControl::run_command(self, reqs, out).await
    }
}
