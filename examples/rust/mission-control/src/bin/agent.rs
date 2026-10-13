//! Example agent: connects to Mission Control with `AsterClient` and calls
//! `getStatus`, `submitLog` and `ingestMetrics` (Python's `agent.py`).
//!
//! ```text
//! cargo run -p mission-control --bin agent -- <server address>
//! ```
//!
//! Configuration comes from `AsterConfig::from_env()`. Against
//! `mission-control --auth`, set this agent's key and credential
//! (`ASTER_SECRET_KEY_FILE`, `ASTER_ENROLLMENT_CREDENTIAL`) and the same
//! `ASTER_ROOT_PUBKEY`, `ASTER_ENROLLMENT_AUDIENCE` and
//! `ASTER_ENROLLMENT_FLEET_NAMESPACE` as the server; the roles in the
//! credential decide which calls succeed.

use std::time::{SystemTime, UNIX_EPOCH};

use aster::rpc::AsterClient;
use aster::AsterConfig;
use mission_control::auth::MissionControlClient;
use mission_control::{LogEntry, MetricPoint, StatusRequest};

#[tokio::main]
async fn main() -> aster::Result<()> {
    let mut builder = AsterClient::builder().config(AsterConfig::from_env()?);
    if let Some(address) = std::env::args().nth(1) {
        builder = builder.address(address);
    }
    let client = builder.connect().await?;
    // The open and auth services share wire names, so one client type
    // speaks to either server.
    let mc = MissionControlClient::new(client.rpc());

    match mc
        .get_status(StatusRequest {
            agent_id: "rs-agent-1".into(),
        })
        .await
    {
        Ok(status) => println!("Status: {status:?}"),
        Err(err) => println!("getStatus refused: {err}"),
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64());
    mc.submit_log(LogEntry {
        timestamp: now,
        level: "info".into(),
        message: "agent started".into(),
        agent_id: "rs-agent-1".into(),
    })
    .await?;
    println!("Log submitted");

    let points: Vec<MetricPoint> = (0..100)
        .map(|i| MetricPoint {
            name: "cpu.usage".into(),
            value: f64::from(i % 100),
            timestamp: now,
        })
        .collect();
    match mc.ingest_metrics(points).await {
        Ok(result) => println!("Metrics accepted: {}", result.accepted),
        Err(err) => println!("ingestMetrics refused: {err}"),
    }

    client.close().await;
    Ok(())
}
