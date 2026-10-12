//! `aster mcp` over real stdio, in both protocol eras.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

const META: &str = "io.modelcontextprotocol/protocolVersion";

/// Send each request, read one response per request that has an id.
fn session(requests: &[Value]) -> Vec<Value> {
    let home = std::env::temp_dir().join(format!("aster-cli-test-mcp-{}", std::process::id()));
    let mut child = Command::new(env!("CARGO_BIN_EXE_aster"))
        .arg("mcp")
        .env("ASTER_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn aster mcp");
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut responses = Vec::new();
    for req in requests {
        writeln!(stdin, "{req}").unwrap();
        if req.get("id").is_some() {
            let mut line = String::new();
            stdout.read_line(&mut line).unwrap();
            responses.push(serde_json::from_str(&line).unwrap());
        }
    }
    drop(stdin);
    assert!(child.wait().unwrap().success());
    let _ = std::fs::remove_dir_all(home);
    responses
}

fn modern(id: u32, method: &str, params: Value) -> Value {
    let mut params = params;
    params["_meta"] = json!({META: "2026-07-28"});
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

#[test]
fn modern_era() {
    let r = session(&[
        modern(1, "server/discover", json!({})),
        modern(2, "tools/list", json!({})),
        modern(
            3,
            "tools/call",
            json!({"name": "profile_list", "arguments": {}}),
        ),
        modern(
            4,
            "tools/call",
            json!({"name": "profile_show", "arguments": {"name": "x"}}),
        ),
        modern(5, "tools/call", json!({"name": "nope"})),
        modern(
            6,
            "tools/call",
            json!({"name": "profile_list", "arguments": {"extra": 1}}),
        ),
    ]);
    assert_eq!(r[0]["result"]["supportedVersions"], json!(["2026-07-28"]));
    let names: Vec<&str> = r[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"profile_list") && names.contains(&"profile_show"));
    assert_eq!(
        r[1]["result"]["tools"][0]["annotations"]["readOnlyHint"],
        true
    );
    assert_eq!(r[2]["result"]["structuredContent"], json!({"profiles": []}));
    assert_eq!(r[2]["result"]["resultType"], "complete");
    assert_eq!(r[3]["result"]["isError"], true);
    assert!(
        r[3]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("not_found")
    );
    assert_eq!(r[4]["error"]["code"], -32602);
    assert_eq!(r[5]["error"]["code"], -32602);
}

#[test]
fn version_errors_and_legacy_era() {
    let r = session(&[
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {"_meta": {META: "2099-01-01"}}}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        json!({"jsonrpc": "2.0", "id": 3, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "profile_list"}}),
        json!({"jsonrpc": "2.0", "id": 5, "method": "nope"}),
    ]);
    assert_eq!(r[0]["error"]["code"], -32022);
    assert_eq!(r[0]["error"]["data"]["requested"], "2099-01-01");
    assert_eq!(r[1]["error"]["code"], -32602);
    assert_eq!(r[2]["result"]["protocolVersion"], "2025-06-18");
    assert!(r[3]["result"].get("resultType").is_none());
    assert_eq!(r[3]["result"]["structuredContent"]["profiles"], json!([]));
    assert_eq!(r[4]["error"]["code"], -32601);
}
