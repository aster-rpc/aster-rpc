//! The shared `InteropEcho` contract, Rust side. Included by
//! `tests/interop/rust/interop_echo.rs` and `benchmarks/dispatch/driver.rs`
//! (`#[path]` module), so the interop matrix and the dispatch benchmark use the
//! same types, service and server implementation.
//!
//! ```text
//! service InteropEcho v1
//!   echo(interop/EchoRequest) -> interop/EchoReply                (unary)
//!   generate(interop/EchoRequest) -> stream interop/EchoReply     (server stream)
//!   ingest(stream interop/EchoRequest) -> interop/EchoReply       (client stream)
//!   chat(stream interop/EchoRequest) -> stream interop/EchoReply  (bidi)
//!   hold(interop/EchoRequest) -> interop/EchoReply                (unary, blocks)
//!   cancelled(interop/EchoRequest) -> interop/EchoReply           (unary)
//! interop/EchoRequest { message: string, count: varint32, data: binary }
//! interop/EchoReply   { message: string, count: varint32, data: binary, server_lang: string }
//! ```
//!
//! Server semantics (identical in every language):
//! - `echo`: `message` and `data` echoed, `count + 1` (i32 wrap), `server_lang` set.
//! - `generate`: `max(count, 0)` replies; reply `i` carries the request's
//!   `message` and `data`, `count = i`, and `server_lang`.
//! - `ingest`: one reply with `count` = number of requests received,
//!   `message` = the last request's message (`""` if none), empty `data`.
//! - `chat`: for every request, in arrival order, ONE reply is sent right away
//!   (before the next request is read): `message` and `data` echoed,
//!   `count` = the request's 0-based position in the stream, `server_lang` set.
//!   The reply stream ends when the request stream ends.
//! - `hold`: blocks until the call is cancelled (client cancel, client deadline
//!   or connection loss) or 60 s pass. Observing the cancellation increments a
//!   process-wide counter. After 60 s it replies `count = -1`,
//!   `message = "timeout"` instead (the cancel cases treat that as a failure).
//! - `cancelled`: replies `count` = the number of `hold` calls that observed
//!   a cancellation so far, `message = "cancelled"`.
//!
//! A second service, `InteropSession v1`, is **session-scoped** (a fresh
//! instance per `(connection, session id)`; see `interop_echo.rs`):
//!
//! ```text
//! service InteropSession v1 (scope = session)
//!   add(interop/EchoRequest) -> interop/EchoReply                 (unary)
//! ```
//!
//! `add`: the instance keeps `total` (sum of the requests' `count`, i32 wrap)
//! and `calls`; it replies `count = total`, `message = "calls=<calls>"`
//! (after counting this call), `server_lang` set.

#![allow(dead_code)]

use aster::rpc::{async_trait, RequestStream, ResponseSink};
use aster::Result;
use fory_derive::ForyStruct;

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "interop/EchoRequest")]
pub struct EchoRequest {
    pub message: String,
    pub count: i32,
    // `#[fory(bytes)]` makes fory-derive write BINARY (type 41), matching the
    // contract's `binary` and Python `bytes` / TS `Uint8Array`. Without it
    // Fory writes LIST<UINT8> (type 22), so `#[derive(AsterType)]` rejects an
    // unannotated `Vec<u8>` at compile time.
    #[fory(bytes)]
    pub data: Vec<u8>,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "interop/EchoReply")]
pub struct EchoReply {
    pub message: String,
    pub count: i32,
    #[fory(bytes)]
    pub data: Vec<u8>,
    pub server_lang: String,
}

#[aster::service(name = "InteropEcho", version = 1)]
pub trait InteropEcho {
    async fn echo(&self, req: EchoRequest) -> Result<EchoReply>;

    #[rpc(server_stream)]
    async fn generate(&self, req: EchoRequest, out: ResponseSink<EchoReply>) -> Result<()>;

    #[rpc(client_stream)]
    async fn ingest(&self, reqs: RequestStream<EchoRequest>) -> Result<EchoReply>;

    #[rpc(bidi_stream)]
    async fn chat(
        &self,
        reqs: RequestStream<EchoRequest>,
        out: ResponseSink<EchoReply>,
    ) -> Result<()>;

    async fn hold(&self, req: EchoRequest) -> Result<EchoReply>;

    async fn cancelled(&self, req: EchoRequest) -> Result<EchoReply>;
}

pub struct EchoImpl;

/// `hold` calls that observed a cancellation (process-wide; the matrix runs
/// one server process per server language and clients take turns).
static HOLD_CANCELLED: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// Counts a cancelled `hold`: the handler future is dropped when the call is
/// cancelled or its deadline passes, which runs this before `disarm`.
struct HoldGuard {
    armed: bool,
}

impl Drop for HoldGuard {
    fn drop(&mut self) {
        if self.armed {
            HOLD_CANCELLED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

#[async_trait]
impl InteropEcho for EchoImpl {
    async fn echo(&self, req: EchoRequest) -> Result<EchoReply> {
        Ok(EchoReply {
            message: req.message,
            count: req.count.wrapping_add(1),
            data: req.data,
            server_lang: "rust".into(),
        })
    }

    async fn generate(&self, req: EchoRequest, out: ResponseSink<EchoReply>) -> Result<()> {
        for i in 0..req.count.max(0) {
            out.send(&EchoReply {
                message: req.message.clone(),
                count: i,
                data: req.data.clone(),
                server_lang: "rust".into(),
            })
            .await?;
        }
        Ok(())
    }

    async fn ingest(&self, mut reqs: RequestStream<EchoRequest>) -> Result<EchoReply> {
        let mut n = 0i32;
        let mut last = String::new();
        while let Some(r) = reqs.recv().await? {
            n = n.wrapping_add(1);
            last = r.message;
        }
        Ok(EchoReply {
            message: last,
            count: n,
            data: vec![],
            server_lang: "rust".into(),
        })
    }

    async fn chat(
        &self,
        mut reqs: RequestStream<EchoRequest>,
        out: ResponseSink<EchoReply>,
    ) -> Result<()> {
        let mut i = 0i32;
        while let Some(r) = reqs.recv().await? {
            out.send(&EchoReply {
                message: r.message,
                count: i,
                data: r.data,
                server_lang: "rust".into(),
            })
            .await?;
            i = i.wrapping_add(1);
        }
        Ok(())
    }

    async fn hold(&self, _req: EchoRequest) -> Result<EchoReply> {
        let mut guard = HoldGuard { armed: true };
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        guard.armed = false;
        Ok(EchoReply {
            message: "timeout".into(),
            count: -1,
            data: vec![],
            server_lang: "rust".into(),
        })
    }

    async fn cancelled(&self, _req: EchoRequest) -> Result<EchoReply> {
        Ok(EchoReply {
            message: "cancelled".into(),
            count: HOLD_CANCELLED.load(std::sync::atomic::Ordering::SeqCst),
            data: vec![],
            server_lang: "rust".into(),
        })
    }
}
