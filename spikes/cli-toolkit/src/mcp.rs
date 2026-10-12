//! MCP over stdio, hand-rolled on serde_json: newline-delimited JSON-RPC 2.0.
//!
//! Serves both eras of the protocol:
//! - modern (2026-07-28+): every request carries
//!   `_meta["io.modelcontextprotocol/protocolVersion"]`; `server/discover` is
//!   mandatory; unsupported versions get error -32022.
//! - legacy (2025-11-25 and earlier): an `initialize` handshake selects the
//!   version for the rest of the process.
//!
//! Tool calls run on their own threads so long calls do not block `ping`,
//! further calls or `notifications/cancelled`. A cancelled call sends nothing.

use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};

use crate::Ctx;

const MODERN: &[&str] = &["2026-07-28"];
const LEGACY: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26"];
const VERSION_KEY: &str = "io.modelcontextprotocol/protocolVersion";
const INSTRUCTIONS: &str = "Aster operator tools.";

#[derive(Clone)]
struct Shared {
    out: Arc<Mutex<std::io::Stdout>>,
    cancelled: Arc<Mutex<HashSet<String>>>,
    home: std::path::PathBuf,
}

impl Shared {
    fn send(&self, msg: &Value) {
        let mut out = self.out.lock().expect("stdout lock");
        // serde_json never emits raw newlines, satisfying the framing rule.
        let _ = writeln!(out, "{msg}");
        let _ = out.flush();
    }
    fn is_cancelled(&self, id: &Value) -> bool {
        self.cancelled
            .lock()
            .expect("lock")
            .contains(&id.to_string())
    }
}

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

fn tools() -> Value {
    json!([
        {
            "name": "profile_list",
            "description": "List configured Aster operator profiles.",
            "inputSchema": {"type": "object", "additionalProperties": false},
        },
        {
            "name": "sleep",
            "description": "Wait for the given milliseconds (cancellation test).",
            "inputSchema": {
                "type": "object",
                "properties": {"ms": {"type": "integer", "minimum": 0}},
                "required": ["ms"],
                "additionalProperties": false,
            },
        },
    ])
}

/// Returns None when the call was cancelled (nothing must be sent).
fn call_tool(shared: &Shared, id: &Value, name: &str, args: &Value, modern: bool) -> Option<Value> {
    let complete = |mut r: Value| {
        if modern {
            r["resultType"] = json!("complete");
        }
        r
    };
    let result = match name {
        "profile_list" => {
            let ctx = Ctx {
                home: shared.home.clone(),
                json: true,
                interactive: false,
            };
            match crate::profile::list_json(&ctx) {
                Ok(v) => complete(
                    json!({"content": [{"type": "text", "text": v.to_string()}], "structuredContent": v}),
                ),
                Err(e) => complete(
                    json!({"content": [{"type": "text", "text": format!("{e:#}")}], "isError": true}),
                ),
            }
        }
        "sleep" => {
            let Some(ms) = args.get("ms").and_then(Value::as_u64) else {
                return Some(complete(
                    json!({"content": [{"type": "text", "text": "ms must be a non-negative integer"}], "isError": true}),
                ));
            };
            let deadline = std::time::Instant::now() + Duration::from_millis(ms);
            while std::time::Instant::now() < deadline {
                if shared.is_cancelled(id) {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            complete(json!({"content": [{"type": "text", "text": format!("slept {ms} ms")}]}))
        }
        _ => return Some(err(id, -32602, &format!("Unknown tool: {name}"), None)),
    };
    (!shared.is_cancelled(id)).then(|| ok(id, result))
}

pub fn run(ctx: &Ctx) -> Result<ExitCode> {
    let shared = Shared {
        out: Arc::new(Mutex::new(std::io::stdout())),
        cancelled: Arc::new(Mutex::new(HashSet::new())),
        home: ctx.home.clone(),
    };
    let mut legacy: Option<String> = None;
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                shared.send(&err(&Value::Null, -32700, "Parse error", None));
                continue;
            }
        };
        let Some(method) = msg.get("method").and_then(Value::as_str) else {
            continue; // a response; this server never sends requests
        };
        let params = msg.get("params").cloned().unwrap_or(json!({}));
        let Some(id) = msg.get("id").cloned() else {
            if method == "notifications/cancelled"
                && let Some(rid) = params.get("requestId")
            {
                shared
                    .cancelled
                    .lock()
                    .expect("lock")
                    .insert(rid.to_string());
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
            shared.send(&ok(
                &id,
                json!({
                    "protocolVersion": chosen,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "aster", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": INSTRUCTIONS,
                }),
            ));
            continue;
        }

        // Era: explicit per-request version wins; otherwise the legacy session.
        let modern = match params
            .get("_meta")
            .and_then(|m| m.get(VERSION_KEY))
            .and_then(Value::as_str)
        {
            Some(v) if MODERN.contains(&v) => true,
            Some(v) => {
                let supported: Vec<&str> = MODERN.iter().chain(LEGACY).copied().collect();
                shared.send(&err(
                    &id,
                    -32022,
                    "Unsupported protocol version",
                    Some(json!({"supported": supported, "requested": v})),
                ));
                continue;
            }
            None if legacy.is_some() => false,
            None => {
                shared.send(&err(&id, -32602, &format!("Missing _meta.{VERSION_KEY}; supported: {MODERN:?} or initialize with {LEGACY:?}"), None));
                continue;
            }
        };
        let complete = |mut r: Value| {
            if modern {
                r["resultType"] = json!("complete");
            }
            r
        };

        match method {
            "server/discover" if modern => shared.send(&ok(&id, complete(json!({
                "supportedVersions": MODERN,
                "capabilities": {"tools": {}},
                "_meta": {"io.modelcontextprotocol/serverInfo": {"name": "aster", "version": env!("CARGO_PKG_VERSION")}},
                "instructions": INSTRUCTIONS,
            })))),
            "ping" => shared.send(&ok(&id, complete(json!({})))),
            "tools/list" => shared.send(&ok(&id, complete(json!({"tools": tools()})))),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let shared = shared.clone();
                std::thread::spawn(move || {
                    if let Some(resp) = call_tool(&shared, &id, &name, &args, modern) {
                        shared.send(&resp);
                    }
                });
            }
            _ => shared.send(&err(&id, -32601, &format!("Method not found: {method}"), None)),
        }
    }
    Ok(ExitCode::SUCCESS)
}
