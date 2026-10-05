//! Shared test plumbing: a child-process harness (the pipeline and the
//! `tracing` subscriber are process-global, so each case runs in a fresh copy
//! of the test binary) and a minimal OTLP/HTTP receiver on 127.0.0.1.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const CASE: &str = "ASTER_OTEL_TEST_CASE";

/// The case the current process should run, if it is a child.
pub fn child_case() -> Option<String> {
    std::env::var(CASE).ok()
}

/// Output of a child run.
pub struct Child {
    pub stdout: String,
    pub stderr: String,
}

/// Run `case` in a child copy of this test binary with a clean `OTEL_*` /
/// `ASTER_*` environment plus `env`. Panics unless the child succeeds and
/// printed `case-ok`.
pub fn run(case: &str, env: &[(&str, &str)]) -> Child {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", "child", "--nocapture", "--test-threads=1"])
        .env(CASE, case);
    for (k, _) in std::env::vars() {
        if k.starts_with("OTEL_") || k.starts_with("ASTER_") {
            cmd.env_remove(k);
        }
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();
    let child = Child {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    };
    assert!(
        out.status.success() && child.stdout.contains("case-ok"),
        "case {case} failed: {:?}\n--- stdout\n{}\n--- stderr\n{}",
        out.status,
        child.stdout,
        child.stderr
    );
    child
}

/// One request the receiver saw.
#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    /// Lower-cased names.
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn body_contains(&self, needle: &str) -> bool {
        self.body
            .windows(needle.len())
            .any(|w| w == needle.as_bytes())
    }
}

/// A tiny HTTP/1.1 server answering every request with an empty `200`, which
/// is a valid (empty) OTLP export response. Bind first, start later, so a test
/// can put the port into the environment before any thread exists.
pub struct Receiver {
    listener: Option<TcpListener>,
    pub addr: SocketAddr,
    seen: Arc<Mutex<Vec<Request>>>,
}

impl Receiver {
    pub fn bind() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        Receiver {
            listener: Some(listener),
            addr,
            seen: Arc::default(),
        }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn start(&mut self) {
        let listener = self.listener.take().expect("started once");
        let seen = self.seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let seen = seen.clone();
                std::thread::spawn(move || serve(stream, &seen));
            }
        });
    }

    pub fn requests(&self) -> Vec<Request> {
        self.seen.lock().unwrap().clone()
    }

    pub fn posts_to(&self, path: &str) -> Vec<Request> {
        self.requests()
            .into_iter()
            .filter(|r| r.method == "POST" && r.path == path)
            .collect()
    }

    /// Wait until at least `n` POSTs to `path` arrived (or `timeout`).
    pub fn wait_for(&self, path: &str, n: usize, timeout: Duration) -> Vec<Request> {
        let deadline = Instant::now() + timeout;
        loop {
            let got = self.posts_to(path);
            if got.len() >= n || Instant::now() > deadline {
                return got;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn serve(stream: TcpStream, seen: &Mutex<Vec<Request>>) {
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or("").to_owned();
        let path = parts.next().unwrap_or("").to_owned();
        let mut headers = HashMap::new();
        loop {
            let mut h = String::new();
            if reader.read_line(&mut h).unwrap_or(0) == 0 {
                return;
            }
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_owned());
            }
        }
        let len: usize = headers
            .get("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let mut body = vec![0; len];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        seen.lock().unwrap().push(Request {
            method,
            path,
            headers,
            body,
        });
        let reply =
            b"HTTP/1.1 200 OK\r\ncontent-type: application/x-protobuf\r\ncontent-length: 0\r\n\r\n";
        if writer.write_all(reply).is_err() {
            return;
        }
    }
}
