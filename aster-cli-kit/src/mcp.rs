//! The Aster MCP server: newline-delimited JSON-RPC 2.0 over stdio, hand-rolled
//! on serde_json. It hosts every family's [`Tool`]s.
//!
//! Serves both eras of the protocol:
//! - modern (2026-07-28 and later): every request carries
//!   `_meta["io.modelcontextprotocol/protocolVersion"]`; `server/discover` is
//!   mandatory; unsupported versions get error -32022.
//! - legacy (2025-11-25 and earlier): an `initialize` handshake selects the
//!   version for the rest of the session.
//!
//! Each tool call runs as its own task, so long calls do not block `ping`,
//! other calls or cancellation. `notifications/cancelled` aborts the task and
//! nothing is sent for that request. End of input ends the session and aborts
//! calls still running.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use crate::ctx::Ctx;
use crate::error::Result;
use crate::tool::{CallError, Tool};

pub const MODERN: &[&str] = &["2026-07-28"];
pub const LEGACY: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26"];
const VERSION_KEY: &str = "io.modelcontextprotocol/protocolVersion";

pub struct Server {
    pub name: &'static str,
    pub version: &'static str,
    pub instructions: &'static str,
    pub tools: Vec<Tool>,
}

type Outbox = mpsc::UnboundedSender<Value>;
type InFlight = Arc<Mutex<HashMap<String, AbortHandle>>>;

fn ok(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn err(id: &Value, code: i64, message: &str, data: Option<Value>) -> Value {
    let mut e = json!({"code": code, "message": message});
    if let Some(d) = data {
        e["data"] = d;
    }
    json!({"jsonrpc": "2.0", "id": id, "error": e})
}

/// Modern-era results declare that they are final.
fn finish(mut result: Value, modern: bool) -> Value {
    if modern {
        result["resultType"] = json!("complete");
    }
    result
}

fn tool_result(outcome: Result<Value, CallError>, id: &Value, modern: bool) -> Value {
    match outcome {
        Ok(v) => ok(
            id,
            finish(
                json!({"content": [{"type": "text", "text": v.to_string()}], "structuredContent": v}),
                modern,
            ),
        ),
        Err(CallError::Tool(e)) => ok(
            id,
            finish(
                json!({"content": [{"type": "text", "text": e.to_json().to_string()}], "isError": true}),
                modern,
            ),
        ),
        Err(CallError::InvalidParams(m)) => {
            err(id, -32602, &format!("Invalid arguments: {m}"), None)
        }
    }
}

impl Server {
    /// Serve until `input` ends.
    pub async fn serve<R, W>(self, ctx: &Ctx, input: R, mut output: W) -> Result<()>
    where
        R: AsyncBufRead + Unpin,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let ctx = Arc::new(ctx.for_tools());
        let tools: Arc<HashMap<&str, Tool>> =
            Arc::new(self.tools.iter().map(|t| (t.name(), t.clone())).collect());
        let listing: Vec<Value> = self.tools.iter().map(Tool::describe).collect();
        let in_flight: InFlight = Arc::default();

        // One writer keeps each message on its own line.
        let (outbox, mut rx) = mpsc::unbounded_channel::<Value>();
        let writer = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                // serde_json never emits raw newlines, satisfying the framing rule.
                let mut line = msg.to_string();
                line.push('\n');
                if output.write_all(line.as_bytes()).await.is_err() || output.flush().await.is_err()
                {
                    break;
                }
            }
        });

        let mut legacy: Option<String> = None;
        let mut input = input;
        let mut buf = Vec::new();
        loop {
            buf.clear();
            if input.read_until(b'\n', &mut buf).await? == 0 {
                break;
            }
            // A line that is not UTF-8 is a parse error, not the end of the session.
            let Ok(line) = std::str::from_utf8(&buf) else {
                send(&outbox, err(&Value::Null, -32700, "Parse error", None));
                continue;
            };
            if line.trim().is_empty() {
                continue;
            }
            let Ok(msg) = serde_json::from_str::<Value>(line) else {
                send(&outbox, err(&Value::Null, -32700, "Parse error", None));
                continue;
            };
            let Some(method) = msg.get("method").and_then(Value::as_str) else {
                continue; // a response; this server sends no requests
            };
            let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
            let Some(id) = msg.get("id").cloned() else {
                if method == "notifications/cancelled"
                    && let Some(rid) = params.get("requestId")
                    && let Some(task) = in_flight
                        .lock()
                        .expect("in-flight lock")
                        .remove(&rid.to_string())
                {
                    task.abort();
                }
                continue; // other notifications need no action
            };

            if method == "initialize" {
                let requested = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let chosen = if LEGACY.contains(&requested) {
                    requested
                } else {
                    LEGACY[0]
                };
                legacy = Some(chosen.to_string());
                send(
                    &outbox,
                    ok(
                        &id,
                        json!({
                            "protocolVersion": chosen,
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": self.name, "version": self.version},
                            "instructions": self.instructions,
                        }),
                    ),
                );
                continue;
            }

            // Era: an explicit per-request version wins; otherwise the legacy session.
            let modern = match params
                .get("_meta")
                .and_then(|m| m.get(VERSION_KEY))
                .and_then(Value::as_str)
            {
                Some(v) if MODERN.contains(&v) => true,
                Some(v) => {
                    let supported: Vec<&str> = MODERN.iter().chain(LEGACY).copied().collect();
                    send(
                        &outbox,
                        err(
                            &id,
                            -32022,
                            "Unsupported protocol version",
                            Some(json!({"supported": supported, "requested": v})),
                        ),
                    );
                    continue;
                }
                None if legacy.is_some() => false,
                None => {
                    send(
                        &outbox,
                        err(
                            &id,
                            -32602,
                            &format!(
                                "Missing _meta.{VERSION_KEY}; send one of {MODERN:?} or initialize with one of {LEGACY:?}"
                            ),
                            None,
                        ),
                    );
                    continue;
                }
            };

            match method {
                "server/discover" if modern => send(
                    &outbox,
                    ok(
                        &id,
                        finish(
                            json!({
                                "supportedVersions": MODERN,
                                "capabilities": {"tools": {}},
                                "_meta": {"io.modelcontextprotocol/serverInfo": {"name": self.name, "version": self.version}},
                                "instructions": self.instructions,
                            }),
                            true,
                        ),
                    ),
                ),
                "ping" => send(&outbox, ok(&id, finish(json!({}), modern))),
                "tools/list" => send(&outbox, ok(&id, finish(json!({"tools": listing}), modern))),
                "tools/call" => {
                    let name = params
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let Some(tool) = tools.get(name) else {
                        send(
                            &outbox,
                            err(&id, -32602, &format!("Unknown tool: {name}"), None),
                        );
                        continue;
                    };
                    let args = params
                        .get("arguments")
                        .cloned()
                        .unwrap_or_else(|| json!({}));
                    spawn_call(
                        tool.call(ctx.clone(), args),
                        id,
                        modern,
                        &outbox,
                        &in_flight,
                    );
                }
                _ => send(
                    &outbox,
                    err(&id, -32601, &format!("Method not found: {method}"), None),
                ),
            }
        }

        for (_, task) in in_flight.lock().expect("in-flight lock").drain() {
            task.abort();
        }
        drop(outbox);
        let _ = writer.await;
        Ok(())
    }
}

fn send(outbox: &Outbox, msg: Value) {
    let _ = outbox.send(msg);
}

/// Run a call as a task registered under its request id. Whichever of
/// completion and cancellation removes the entry first wins, under the lock,
/// so a cancelled call never answers.
fn spawn_call(
    call: crate::tool::CallFuture,
    id: Value,
    modern: bool,
    outbox: &Outbox,
    in_flight: &InFlight,
) {
    let key = id.to_string();
    let mut guard = in_flight.lock().expect("in-flight lock");
    // A second call under a running call's id would orphan the first.
    if guard.contains_key(&key) {
        send(
            outbox,
            err(
                &id,
                -32600,
                "Invalid Request: a call with this id is still running",
                None,
            ),
        );
        return;
    }
    let (outbox, registry) = (outbox.clone(), in_flight.clone());
    let task = tokio::spawn({
        let key = key.clone();
        async move {
            let response = tool_result(call.await, &id, modern);
            if registry
                .lock()
                .expect("in-flight lock")
                .remove(&key)
                .is_some()
            {
                send(&outbox, response);
            }
        }
    });
    // Registered before the task can look itself up: it needs this lock.
    guard.insert(key, task.abort_handle());
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::ctx::{ColorChoice, GlobalArgs};
    use crate::tool::{NoArgs, no_arguments};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[derive(serde::Deserialize)]
    struct SleepArgs {
        ms: u64,
    }

    fn server() -> Server {
        Server {
            name: "test",
            version: "0",
            instructions: "",
            tools: vec![
                Tool::new(
                    "sleep",
                    "",
                    json!({"type": "object"}),
                    |_, a: SleepArgs| async move {
                        tokio::time::sleep(Duration::from_millis(a.ms)).await;
                        Ok(json!({"slept": a.ms}))
                    },
                ),
                Tool::new("fail", "", no_arguments(), |_, _: NoArgs| async {
                    Err::<Value, _>(crate::Error::not_found("gone"))
                }),
            ],
        }
    }

    fn call(id: u32, name: &str, args: Value) -> String {
        json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
               "params": {"name": name, "arguments": args, "_meta": {VERSION_KEY: MODERN[0]}}})
        .to_string()
    }

    #[tokio::test]
    async fn slow_call_does_not_block_and_cancelled_call_never_answers() {
        let ctx = Ctx::new(&GlobalArgs {
            home: Some("/nonexistent".into()),
            profile: None,
            json: true,
            color: ColorChoice::Never,
        })
        .unwrap();
        let (client, server_end) = tokio::io::duplex(1 << 16);
        let (server_read, server_write) = tokio::io::split(server_end);
        let serve = tokio::spawn(async move {
            server()
                .serve(&ctx, BufReader::new(server_read), server_write)
                .await
        });
        let (client_read, mut client_write) = tokio::io::split(client);
        let mut responses = BufReader::new(client_read).lines();

        let cancel = json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 1}});
        for line in [
            call(1, "sleep", json!({"ms": 10_000})),
            call(2, "sleep", json!({"ms": 300})),
            call(2, "sleep", json!({"ms": 1})),
            cancel.to_string(),
            call(3, "fail", json!({})),
        ] {
            client_write
                .write_all(format!("{line}\n").as_bytes())
                .await
                .unwrap();
        }
        client_write
            .write_all(b"\xff\xfe not utf-8\n")
            .await
            .unwrap();

        // Four answers, in whatever order: the refused duplicate, the parse
        // error, the failing call, then call 2. Call 1 was cancelled.
        let mut answers = Vec::new();
        while answers.len() < 4 {
            let line = tokio::time::timeout(Duration::from_secs(2), responses.next_line());
            let v: Value = serde_json::from_str(&line.await.unwrap().unwrap().unwrap()).unwrap();
            answers.push(v);
        }
        let find =
            |pred: &dyn Fn(&Value) -> bool| answers.iter().find(|v| pred(v)).cloned().unwrap();
        assert_eq!(
            find(&|v| v["error"]["code"] == -32600)["id"],
            2,
            "duplicate id refused"
        );
        assert_eq!(
            find(&|v| v["error"]["code"] == -32700)["id"],
            Value::Null,
            "bad UTF-8 is a parse error"
        );
        assert_eq!(find(&|v| v["id"] == 3)["result"]["isError"], true);
        let two = find(&|v| v["id"] == 2 && v.get("result").is_some());
        assert_eq!(
            two["result"]["structuredContent"]["slept"], 300,
            "the first call 2 still answers"
        );
        let pos = |id: i64| {
            answers
                .iter()
                .position(|v| v["id"] == id && v.get("result").is_some())
                .unwrap()
        };
        assert!(
            pos(3) < pos(2),
            "the failing call answers while call 2 still runs"
        );

        client_write.shutdown().await.unwrap(); // end of input
        tokio::time::timeout(Duration::from_secs(2), serve)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            responses.next_line().await.unwrap().is_none(),
            "cancelled call 1 never answered"
        );
    }
}
