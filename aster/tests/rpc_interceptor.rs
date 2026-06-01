#![cfg(feature = "rpc")]
//! Client interceptor pipeline: a custom interceptor's hooks fire, a deadline
//! yields DEADLINE_EXCEEDED, a RetryPolicy recovers a flaky call, and a
//! CircuitBreaker fails fast (without hitting the server) once open.

use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aster::rpc::{
    async_trait, Call, CallContext, CircuitBreaker, Interceptor, RetryPolicy, RpcConnection,
    RpcStatus, SerializationMode, Server, ServiceDispatch, StatusCode, StreamHeader, RPC_ALPN,
};
use aster::{AsterConfig, Node, RelayMode};
use tokio::time::timeout;

#[derive(Default)]
struct State {
    flaky_calls: AtomicU32,
    fail_calls: AtomicU32,
}

struct FlakyService {
    state: Arc<State>,
}

#[async_trait]
impl ServiceDispatch for FlakyService {
    fn name(&self) -> &str {
        "Flaky"
    }
    fn version(&self) -> i32 {
        1
    }
    fn methods(&self) -> &[&str] {
        &["echo", "flaky", "slow", "always_fail"]
    }
    async fn dispatch(&self, method: &str, mut call: Call) {
        let _ = call.recv_request().await;
        match method {
            "echo" => {
                let _ = call.respond(b"ok".to_vec(), &RpcStatus::ok());
            }
            // Fails UNAVAILABLE for the first two calls, then succeeds.
            "flaky" => {
                let n = self.state.flaky_calls.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    let _ = call.finish(&RpcStatus::error(StatusCode::Unavailable, "flaky"));
                } else {
                    let _ = call.respond(b"recovered".to_vec(), &RpcStatus::ok());
                }
            }
            // Sleeps past any reasonable client deadline.
            "slow" => {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let _ = call.respond(b"slow".to_vec(), &RpcStatus::ok());
            }
            // Always fails; counts how often the server is actually reached.
            "always_fail" => {
                self.state.fail_calls.fetch_add(1, Ordering::SeqCst);
                let _ = call.finish(&RpcStatus::error(StatusCode::Unavailable, "down"));
            }
            other => {
                let _ = call.finish(&RpcStatus::error(StatusCode::Unimplemented, other));
            }
        }
    }
}

/// Records each hook invocation, for assertions.
#[derive(Clone)]
struct LogInterceptor {
    log: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Interceptor for LogInterceptor {
    async fn on_request(&self, ctx: &CallContext, request: Vec<u8>) -> aster::Result<Vec<u8>> {
        self.log.lock().unwrap().push(format!("req:{}", ctx.method));
        Ok(request)
    }
    async fn on_response(&self, ctx: &CallContext, response: Vec<u8>) -> aster::Result<Vec<u8>> {
        self.log
            .lock()
            .unwrap()
            .push(format!("resp:{}", ctx.method));
        Ok(response)
    }
}

/// Records the `ctx.attempt` seen by each `on_request`, and counts `on_response`.
#[derive(Clone)]
struct CountInterceptor {
    attempts: Arc<Mutex<Vec<u32>>>,
    responses: Arc<AtomicU32>,
}

#[async_trait]
impl Interceptor for CountInterceptor {
    async fn on_request(&self, ctx: &CallContext, request: Vec<u8>) -> aster::Result<Vec<u8>> {
        self.attempts.lock().unwrap().push(ctx.attempt);
        Ok(request)
    }
    async fn on_response(&self, _ctx: &CallContext, response: Vec<u8>) -> aster::Result<Vec<u8>> {
        self.responses.fetch_add(1, Ordering::SeqCst);
        Ok(response)
    }
}

fn cfg() -> AsterConfig {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .build()
}

async fn wait_for_addr(n: &Node) {
    for _ in 0..50 {
        if !n.addr().direct_addresses.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn within<F: Future>(f: F) -> F::Output {
    timeout(Duration::from_secs(15), f)
        .await
        .expect("rpc operation timed out")
}

fn header(method: &str) -> StreamHeader {
    StreamHeader {
        service: "Flaky".into(),
        method: method.into(),
        version: 1,
        call_id: 0,
        deadline: 0,
        serialization_mode: SerializationMode::Xlang.as_i8(),
        metadata_keys: vec![],
        metadata_values: vec![],
        session_id: 0,
    }
}

fn retry_policy() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 3,
        initial_backoff: Duration::from_millis(1),
        max_backoff: Duration::from_millis(10),
        multiplier: 2.0,
        retryable: vec![StatusCode::Unavailable],
    }
}

#[tokio::test]
async fn interceptor_pipeline_policies() {
    let state = Arc::new(State::default());
    let server = Node::start_with_alpns(cfg(), vec![RPC_ALPN.to_vec()])
        .await
        .unwrap();
    let client = Node::start(cfg()).await.unwrap();
    wait_for_addr(&server).await;
    wait_for_addr(&client).await;
    client.add_peer(&server).unwrap();
    server.add_peer(&client).unwrap();

    let _h = Server::new(&server)
        .register(FlakyService {
            state: state.clone(),
        })
        .serve();

    let base: RpcConnection = within(client.rpc_connect(&server.id())).await.unwrap();

    // 1. Interceptor hooks fire around a unary call.
    let log = Arc::new(Mutex::new(Vec::new()));
    let conn = base
        .clone()
        .with_interceptor(LogInterceptor { log: log.clone() });
    let resp = within(conn.unary(&header("echo"), vec![1])).await.unwrap();
    assert_eq!(resp, b"ok".to_vec());
    assert_eq!(*log.lock().unwrap(), vec!["req:echo", "resp:echo"]);

    // 2. Retry recovers the flaky method (2 UNAVAILABLE then OK), and the
    //    interceptor's on_request runs once per attempt with ctx.attempt set.
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let responses = Arc::new(AtomicU32::new(0));
    let conn = base
        .clone()
        .with_retry(retry_policy())
        .with_interceptor(CountInterceptor {
            attempts: attempts.clone(),
            responses: responses.clone(),
        });
    let resp = within(conn.unary(&header("flaky"), vec![1])).await.unwrap();
    assert_eq!(resp, b"recovered".to_vec());
    assert_eq!(
        state.flaky_calls.load(Ordering::SeqCst),
        3,
        "1 try + 2 retries"
    );
    assert_eq!(
        *attempts.lock().unwrap(),
        vec![1, 2, 3],
        "on_request runs once per attempt with ctx.attempt"
    );
    assert_eq!(
        responses.load(Ordering::SeqCst),
        1,
        "on_response runs once, on the successful attempt"
    );

    // 3. Deadline: the slow method exceeds a short client deadline.
    let conn = base.clone().with_deadline(Duration::from_millis(100));
    let err = within(conn.unary(&header("slow"), vec![1]))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, aster::Error::Rpc { code, .. } if *code == StatusCode::DeadlineExceeded.as_i32()),
        "expected DEADLINE_EXCEEDED, got {err:?}"
    );

    // 4. Circuit breaker: opens after 2 failures, then fails fast without
    // reaching the server.
    let conn = base
        .clone()
        .with_circuit_breaker(CircuitBreaker::new(2, Duration::from_secs(60)));
    for _ in 0..3 {
        let err = within(conn.unary(&header("always_fail"), vec![1]))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, aster::Error::Rpc { code, .. } if *code == StatusCode::Unavailable.as_i32())
        );
    }
    assert_eq!(
        state.fail_calls.load(Ordering::SeqCst),
        2,
        "3rd call must be short-circuited by the open breaker"
    );

    client.shutdown().await;
    server.shutdown().await;
}
