//! Mission Control — the Rust port of the Python/TS example, used to exercise
//! the Aster RPC framework end-to-end (all four call patterns) over both the
//! Iroh transport and the HTTP (Salvo) transport.
//!
//! One **shared** `MissionControl` service carries all four patterns:
//!
//! | Method           | Pattern        |
//! |------------------|----------------|
//! | `get_status`     | unary          |
//! | `submit_log`     | unary          |
//! | `tail_logs`      | server-stream  |
//! | `ingest_metrics` | client-stream  |
//! | `run_command`    | bidi-stream    |
//!
//! Wire method names match the Python/TS peers (`getStatus`, `tailLogs`, …) via
//! `#[rpc(name = "...")]`, so the service is cross-binding-identical.
//!
//! Differences from the Python/TS example: those split per-agent state into a
//! session-scoped `AgentSession` service, which the Rust crate doesn't support
//! yet, so the bidi `run_command` lives on the shared service here.
//! `run_command` echoes the command rather than executing a shell (the Python
//! example runs arbitrary shell; we don't).

use aster::rpc::{RequestStream, ResponseSink};
use fory_derive::ForyStruct;
use serde::{Deserialize, Serialize};

// ── Wire types ──────────────────────────────────────────────────────────────

#[derive(
    ForyStruct, aster::AsterType, Serialize, Deserialize, Debug, Default, Clone, PartialEq,
)]
#[aster(wire = "mission/StatusRequest")]
pub struct StatusRequest {
    pub agent_id: String,
}

#[derive(
    ForyStruct, aster::AsterType, Serialize, Deserialize, Debug, Default, Clone, PartialEq,
)]
#[aster(wire = "mission/StatusResponse")]
pub struct StatusResponse {
    pub agent_id: String,
    pub status: String,
    pub uptime_secs: i64,
}

#[derive(
    ForyStruct, aster::AsterType, Serialize, Deserialize, Debug, Default, Clone, PartialEq,
)]
#[aster(wire = "mission/LogEntry")]
pub struct LogEntry {
    pub timestamp: f64,
    pub level: String,
    pub message: String,
    pub agent_id: String,
}

#[derive(
    ForyStruct, aster::AsterType, Serialize, Deserialize, Debug, Default, Clone, PartialEq,
)]
#[aster(wire = "mission/SubmitLogResult")]
pub struct SubmitLogResult {
    pub accepted: bool,
}

#[derive(
    ForyStruct, aster::AsterType, Serialize, Deserialize, Debug, Default, Clone, PartialEq,
)]
#[aster(wire = "mission/TailRequest")]
pub struct TailRequest {
    pub agent_id: String,
    pub level: String,
}

#[derive(
    ForyStruct, aster::AsterType, Serialize, Deserialize, Debug, Default, Clone, PartialEq,
)]
#[aster(wire = "mission/MetricPoint")]
pub struct MetricPoint {
    pub name: String,
    pub value: f64,
    pub timestamp: f64,
}

#[derive(
    ForyStruct, aster::AsterType, Serialize, Deserialize, Debug, Default, Clone, PartialEq,
)]
#[aster(wire = "mission/IngestResult")]
pub struct IngestResult {
    pub accepted: i32,
    pub dropped: i32,
}

#[derive(
    ForyStruct, aster::AsterType, Serialize, Deserialize, Debug, Default, Clone, PartialEq,
)]
#[aster(wire = "mission/Command")]
pub struct Command {
    pub command: String,
}

#[derive(
    ForyStruct, aster::AsterType, Serialize, Deserialize, Debug, Default, Clone, PartialEq,
)]
#[aster(wire = "mission/CommandResult")]
pub struct CommandResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

// ── Service ─────────────────────────────────────────────────────────────────

#[aster::service(name = "MissionControl", version = 1, codecs = ["json"])]
pub trait MissionControl {
    /// Unary: fleet status for an agent.
    #[rpc(name = "getStatus")]
    async fn get_status(&self, req: StatusRequest) -> aster::Result<StatusResponse>;

    /// Unary: an agent pushes one log entry.
    #[rpc(name = "submitLog")]
    async fn submit_log(&self, entry: LogEntry) -> aster::Result<SubmitLogResult>;

    /// Server-stream: emit log entries (synthetic, finite, for the example).
    #[rpc(server_stream, name = "tailLogs")]
    async fn tail_logs(&self, req: TailRequest, out: ResponseSink<LogEntry>) -> aster::Result<()>;

    /// Client-stream: an agent streams metric points; we count them.
    #[rpc(client_stream, name = "ingestMetrics")]
    async fn ingest_metrics(&self, reqs: RequestStream<MetricPoint>)
        -> aster::Result<IngestResult>;

    /// Bidi: run commands and stream a result per command (echoes, no shell).
    #[rpc(bidi_stream, name = "runCommand")]
    async fn run_command(
        &self,
        reqs: RequestStream<Command>,
        out: ResponseSink<CommandResult>,
    ) -> aster::Result<()>;
}

/// In-memory implementation (stateless; faithful enough for the example/tests).
#[derive(Default)]
pub struct MissionControlImpl;

#[aster::rpc::async_trait]
impl MissionControl for MissionControlImpl {
    async fn get_status(&self, req: StatusRequest) -> aster::Result<StatusResponse> {
        Ok(StatusResponse {
            agent_id: req.agent_id,
            status: "running".into(),
            uptime_secs: 3600,
        })
    }

    async fn submit_log(&self, _entry: LogEntry) -> aster::Result<SubmitLogResult> {
        Ok(SubmitLogResult { accepted: true })
    }

    async fn tail_logs(&self, req: TailRequest, out: ResponseSink<LogEntry>) -> aster::Result<()> {
        for i in 0..3 {
            out.send(&LogEntry {
                timestamp: i as f64,
                level: "info".into(),
                message: format!("log {i}"),
                agent_id: req.agent_id.clone(),
            })?;
        }
        Ok(())
    }

    async fn ingest_metrics(
        &self,
        mut reqs: RequestStream<MetricPoint>,
    ) -> aster::Result<IngestResult> {
        let mut accepted = 0i32;
        while reqs.recv().await?.is_some() {
            accepted += 1;
        }
        Ok(IngestResult {
            accepted,
            dropped: 0,
        })
    }

    async fn run_command(
        &self,
        mut reqs: RequestStream<Command>,
        out: ResponseSink<CommandResult>,
    ) -> aster::Result<()> {
        while let Some(cmd) = reqs.recv().await? {
            out.send(&CommandResult {
                stdout: format!("ran: {}", cmd.command),
                stderr: String::new(),
                exit_code: 0,
            })?;
        }
        Ok(())
    }
}
