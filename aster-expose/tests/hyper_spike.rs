//! Load-bearing spike (design doc §7, step 1): prove `AsterStreamIo` actually
//! drives **real hyper** — a server connection and a client connection over a
//! single in-memory Aster-stream duplex — with HTTP/1.1 keep-alive reuse and a
//! multi-chunk body. No networking: two `AsterStreamIo` instances are
//! cross-wired through unbounded channels via the public `ChunkRead`/
//! `ChunkWrite` seam. If this passes, the L7 acceptor/`dial_http` wiring rests
//! on solid ground.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::Result;
use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::Mutex as TokioMutex;

use aster_transport_core::stream_io::{AsterStreamIo, BoxFut, ChunkRead, ChunkWrite};

// ── In-memory pipe backing AsterStreamIo via the public seam ──────────────

struct PipeWriter {
    tx: Arc<StdMutex<Option<UnboundedSender<Vec<u8>>>>>,
}

impl ChunkWrite for PipeWriter {
    fn write_chunk(&self, data: Vec<u8>) -> BoxFut<Result<()>> {
        let tx = self.tx.clone();
        Box::pin(async move {
            if let Some(s) = tx.lock().unwrap().as_ref() {
                let _ = s.send(data);
            }
            Ok(())
        })
    }
    fn finish(&self) -> BoxFut<Result<()>> {
        let tx = self.tx.clone();
        // Drop the sender so the peer's reader observes clean EOF.
        Box::pin(async move {
            *tx.lock().unwrap() = None;
            Ok(())
        })
    }
}

struct ReaderState {
    rx: UnboundedReceiver<Vec<u8>>,
    buf: VecDeque<u8>,
}

struct PipeReader {
    st: Arc<TokioMutex<ReaderState>>,
}

impl ChunkRead for PipeReader {
    fn read_chunk(&self, max_len: usize) -> BoxFut<Result<Option<Vec<u8>>>> {
        let st = self.st.clone();
        Box::pin(async move {
            let mut g = st.lock().await;
            if g.buf.is_empty() {
                match g.rx.recv().await {
                    Some(v) => g.buf.extend(v),
                    None => return Ok(None), // all senders dropped -> EOF
                }
            }
            let take = g.buf.len().min(max_len);
            Ok(Some(g.buf.drain(..take).collect()))
        })
    }
}

fn duplex_pair() -> (
    AsterStreamIo<PipeReader, PipeWriter>,
    AsterStreamIo<PipeReader, PipeWriter>,
) {
    let (a_tx, a_rx) = unbounded_channel::<Vec<u8>>(); // A writes -> B reads
    let (b_tx, b_rx) = unbounded_channel::<Vec<u8>>(); // B writes -> A reads

    let make = |rx, tx| {
        AsterStreamIo::new(
            PipeReader {
                st: Arc::new(TokioMutex::new(ReaderState {
                    rx,
                    buf: VecDeque::new(),
                })),
            },
            PipeWriter {
                tx: Arc::new(StdMutex::new(Some(tx))),
            },
        )
    };

    (make(b_rx, a_tx), make(a_rx, b_tx))
}

// ── Spikes ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn round_trip_and_keepalive() {
    let (server_io, client_io) = duplex_pair();

    // Server: echo the request path. Draining the body exercises the read path.
    tokio::spawn(async move {
        let io = TokioIo::new(server_io);
        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(
                io,
                service_fn(|req: Request<Incoming>| async move {
                    let path = req.uri().path().to_owned();
                    let _ = req.into_body().collect().await;
                    Ok::<_, Infallible>(Response::new(Full::new(Bytes::from(format!("ok:{path}")))))
                }),
            )
            .await;
    });

    let io = TokioIo::new(client_io);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });

    // Request 1.
    let req = Request::builder()
        .uri("/a")
        .header("Host", "local")
        .body(Empty::<Bytes>::new())
        .unwrap();
    let resp = sender.send_request(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"ok:/a");

    // Request 2 on the SAME sender — proves HTTP/1.1 keep-alive reuse over the
    // one duplex (a fresh connection would have required a new pair).
    sender.ready().await.unwrap();
    let req2 = Request::builder()
        .uri("/b")
        .header("Host", "local")
        .body(Empty::<Bytes>::new())
        .unwrap();
    let resp2 = sender.send_request(req2).await.unwrap();
    let body2 = resp2.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body2[..], b"ok:/b");
}

#[tokio::test]
async fn large_body_echo_spans_chunks() {
    let (server_io, client_io) = duplex_pair();

    tokio::spawn(async move {
        let io = TokioIo::new(server_io);
        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(
                io,
                service_fn(|req: Request<Incoming>| async move {
                    let body = req.into_body().collect().await.unwrap().to_bytes();
                    Ok::<_, Infallible>(Response::new(Full::new(body)))
                }),
            )
            .await;
    });

    let io = TokioIo::new(client_io);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });

    // 256 KiB forces the body across many AsterStreamIo read chunks +
    // leftover-buffer hops in both directions.
    let payload = vec![0xABu8; 256 * 1024];
    let req = Request::builder()
        .method("POST")
        .uri("/echo")
        .header("Host", "local")
        .body(Full::new(Bytes::from(payload.clone())))
        .unwrap();
    let resp = sender.send_request(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let echoed = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(echoed.len(), payload.len());
    assert_eq!(&echoed[..], &payload[..]);
}
