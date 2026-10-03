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

#![allow(dead_code)]

use aster::rpc::{async_trait, RequestStream, ResponseSink};
use aster::Result;
use fory_derive::ForyStruct;

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "interop/EchoRequest")]
pub struct EchoRequest {
    pub message: String,
    pub count: i32,
    // `#[fory(bytes)]` is REQUIRED for interop: without it fory-derive 1.7.6
    // writes `Vec<u8>` as LIST<UINT8> (type 22) while `AsterType` advertises
    // `binary` and Python `bytes` / TS `Uint8Array` write BINARY (type 41).
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
}

pub struct EchoImpl;

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
}
