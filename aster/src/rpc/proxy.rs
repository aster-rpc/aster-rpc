//! Dynamic proxy client: call any service by method name with [`DynValue`] or
//! JSON payloads, using only its published contract.
//!
//! The payload codec is core's dynamic Fory codec
//! ([`aster_transport_core::dynamic`], plan D5): request and response bytes
//! are exactly what the producer's typed stubs read and write, so a proxy call
//! is indistinguishable on the wire from a generated client's. This is the
//! Rust counterpart of Python's / TypeScript's `client.proxy("Service")` and
//! the CLI shell's invoke path.
//!
//! ```ignore
//! let contract = aster::rpc::fetch_and_verify_contract(&registry, &blobs, &producer, "InteropEcho", 1)
//!     .await?
//!     .dyn_contract()?;
//! let proxy = DynamicClient::new(conn, contract);
//! let reply = proxy.call_json("echo", serde_json::json!({"message": "hi", "count": 1})).await?;
//! ```

use std::sync::Arc;

pub use aster_transport_core::dynamic::{
    DynContract, DynEnum, DynError, DynErrorKind, DynMethod, DynSchema, DynStruct, DynUnion,
    DynValue, EncodeOptions, JsonOptions, MissingFields, StringEncoding, UnknownFields,
};

use super::client::{CallOptions, ResponseStream, RpcConnection};
use super::codec::{SerializationMode, StreamHeader};
use super::contract::MethodPattern;
use crate::error::{Error, Result};

fn codec_err(e: DynError) -> Error {
    Error::InvalidArgument(format!("dynamic codec: {e}"))
}

/// A response decode failure: the peer sent a payload that does not match the
/// contract.
fn response_err(method: &str, e: DynError) -> Error {
    Error::Transport(anyhow::anyhow!("{method}: response does not decode: {e}"))
}

/// A proxy for one service on one connection, driven by a [`DynContract`].
/// Cheap to clone.
#[derive(Clone)]
pub struct DynamicClient {
    conn: RpcConnection,
    contract: Arc<DynContract>,
    options: CallOptions,
    json: JsonOptions,
}

impl DynamicClient {
    /// A proxy for `contract`'s service over `conn`.
    pub fn new(conn: RpcConnection, contract: DynContract) -> Self {
        Self::from_shared(conn, Arc::new(contract))
    }

    /// As [`new`](Self::new), sharing an already-built contract.
    pub fn from_shared(conn: RpcConnection, contract: Arc<DynContract>) -> Self {
        DynamicClient {
            conn,
            contract,
            options: CallOptions::default(),
            json: JsonOptions::default(),
        }
    }

    /// Per-call options (session, deadline, metadata) applied to every call.
    pub fn with_options(mut self, options: CallOptions) -> Self {
        self.options = options;
        self
    }

    /// How [`call_json`](Self::call_json) renders responses.
    pub fn with_json_options(mut self, json: JsonOptions) -> Self {
        self.json = json;
        self
    }

    /// The contract this proxy calls through.
    pub fn contract(&self) -> &DynContract {
        &self.contract
    }

    /// The underlying connection.
    pub fn connection(&self) -> &RpcConnection {
        &self.conn
    }

    fn header(&self, method: &str) -> StreamHeader {
        StreamHeader {
            service: self.contract.service.clone(),
            method: method.to_string(),
            version: self.contract.version,
            call_id: 0,
            deadline: 0,
            serialization_mode: SerializationMode::Xlang.as_i8(),
            metadata_keys: vec![],
            metadata_values: vec![],
            session_id: 0,
        }
    }

    fn method(&self, method: &str, want: &[MethodPattern]) -> Result<&DynMethod> {
        let m = self.contract.method(method).map_err(codec_err)?;
        if !want.contains(&m.pattern) {
            return Err(Error::InvalidArgument(format!(
                "{}.{method} is {:?}, not {:?}",
                self.contract.service, m.pattern, want[0]
            )));
        }
        Ok(m)
    }

    fn encode(&self, method: &str, value: &DynValue) -> Result<Vec<u8>> {
        self.contract
            .encode_request(method, value)
            .map_err(codec_err)
    }

    fn decode(&self, method: &str, bytes: &[u8]) -> Result<DynValue> {
        self.contract
            .decode_response(method, bytes)
            .map_err(|e| response_err(method, e))
    }

    /// Unary call.
    pub async fn unary(&self, method: &str, request: &DynValue) -> Result<DynValue> {
        self.method(method, &[MethodPattern::Unary])?;
        let req = self.encode(method, request)?;
        let resp = self
            .conn
            .unary_with(&self.header(method), req, &self.options)
            .await?;
        self.decode(method, &resp)
    }

    /// Server-streaming call: one request, a stream of responses.
    pub async fn server_stream(
        &self,
        method: &str,
        request: &DynValue,
    ) -> Result<DynResponseStream> {
        self.method(method, &[MethodPattern::ServerStream])?;
        let req = self.encode(method, request)?;
        let inner = self
            .conn
            .server_stream_with(&self.header(method), req, &self.options)
            .await?;
        Ok(DynResponseStream {
            inner,
            contract: self.contract.clone(),
            method: method.to_string(),
        })
    }

    /// Client-streaming call: many requests (at least one), one response.
    pub async fn client_stream(&self, method: &str, requests: &[DynValue]) -> Result<DynValue> {
        self.method(method, &[MethodPattern::ClientStream])?;
        let reqs = requests
            .iter()
            .map(|r| self.encode(method, r))
            .collect::<Result<Vec<_>>>()?;
        let resp = self
            .conn
            .client_stream_with(&self.header(method), reqs, &self.options)
            .await?;
        self.decode(method, &resp)
    }

    /// Bidirectional call with all requests known up front (at least one);
    /// read responses from the returned stream.
    pub async fn bidi(&self, method: &str, requests: &[DynValue]) -> Result<DynResponseStream> {
        self.method(method, &[MethodPattern::BidiStream])?;
        let reqs = requests
            .iter()
            .map(|r| self.encode(method, r))
            .collect::<Result<Vec<_>>>()?;
        if reqs.is_empty() {
            return Err(Error::InvalidArgument(
                "bidi streaming requires at least one request".into(),
            ));
        }
        let (sink, inner) = self
            .conn
            .bidi_open_with(&self.header(method), &self.options)
            .await?;
        tokio::spawn(async move {
            let n = reqs.len();
            for (i, req) in reqs.into_iter().enumerate() {
                let sent = if i + 1 == n {
                    sink.send_last(req).await
                } else {
                    sink.send(req).await
                };
                if sent.is_err() {
                    return;
                }
            }
        });
        Ok(DynResponseStream {
            inner,
            contract: self.contract.clone(),
            method: method.to_string(),
        })
    }

    /// Any pattern, collected: `requests` is one value for unary and
    /// server-streaming methods, one or more for client-streaming and bidi;
    /// the result is one value for unary and client-streaming methods, all
    /// responses for server-streaming and bidi.
    pub async fn call(&self, method: &str, requests: Vec<DynValue>) -> Result<Vec<DynValue>> {
        let m = self.contract.method(method).map_err(codec_err)?;
        let single = |reqs: &[DynValue]| -> Result<()> {
            if reqs.len() != 1 {
                return Err(Error::InvalidArgument(format!(
                    "{method} takes exactly one request, got {}",
                    reqs.len()
                )));
            }
            Ok(())
        };
        match m.pattern {
            MethodPattern::Unary => {
                single(&requests)?;
                Ok(vec![self.unary(method, &requests[0]).await?])
            }
            MethodPattern::ServerStream => {
                single(&requests)?;
                self.server_stream(method, &requests[0])
                    .await?
                    .collect()
                    .await
            }
            MethodPattern::ClientStream => Ok(vec![self.client_stream(method, &requests).await?]),
            MethodPattern::BidiStream => self.bidi(method, &requests).await?.collect().await,
        }
    }

    /// [`call`](Self::call) with JSON: the request is an object (or, for a
    /// client-streaming / bidi method, an array of objects — a single object
    /// is one request); the response is an object for unary and
    /// client-streaming methods and an array for server-streaming and bidi.
    /// Values follow the Aster JSON encoding (see
    /// [`aster_transport_core::dynamic`]).
    pub async fn call_json(
        &self,
        method: &str,
        request: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let m = self.contract.method(method).map_err(codec_err)?;
        let streaming_in = matches!(
            m.pattern,
            MethodPattern::ClientStream | MethodPattern::BidiStream
        );
        let requests = match request {
            serde_json::Value::Array(items) if streaming_in => {
                items.iter().map(DynValue::from_json).collect()
            }
            other => vec![DynValue::from_json(&other)],
        };
        let pattern = m.pattern;
        let out = self.call(method, requests).await?;
        Ok(match pattern {
            MethodPattern::Unary | MethodPattern::ClientStream => out
                .first()
                .map(|v| v.to_json(&self.json))
                .unwrap_or(serde_json::Value::Null),
            _ => serde_json::Value::Array(out.iter().map(|v| v.to_json(&self.json)).collect()),
        })
    }
}

/// Responses of a server-streaming or bidi proxy call, decoded.
pub struct DynResponseStream {
    inner: ResponseStream,
    contract: Arc<DynContract>,
    method: String,
}

impl DynResponseStream {
    /// The next response; `None` at the OK trailer.
    pub async fn next(&mut self) -> Option<Result<DynValue>> {
        let item = self.inner.next().await?;
        Some(item.and_then(|bytes| {
            self.contract
                .decode_response(&self.method, &bytes)
                .map_err(|e| response_err(&self.method, e))
        }))
    }

    /// All remaining responses, stopping at the first error.
    pub async fn collect(&mut self) -> Result<Vec<DynValue>> {
        let mut out = Vec::new();
        while let Some(item) = self.next().await {
            out.push(item?);
        }
        Ok(out)
    }

    /// Cancel the call.
    pub async fn cancel(&mut self) {
        self.inner.cancel().await;
    }
}

impl RpcConnection {
    /// Call `method` of `contract`'s service with dynamic values — any of the
    /// four patterns (see [`DynamicClient::call`]).
    pub async fn call_dynamic(
        &self,
        contract: &DynContract,
        method: &str,
        requests: Vec<DynValue>,
    ) -> Result<Vec<DynValue>> {
        DynamicClient::new(self.clone(), contract.clone())
            .call(method, requests)
            .await
    }

    /// [`call_dynamic`](Self::call_dynamic) with JSON in and out (see
    /// [`DynamicClient::call_json`]).
    pub async fn call_dynamic_json(
        &self,
        contract: &DynContract,
        method: &str,
        request: serde_json::Value,
    ) -> Result<serde_json::Value> {
        DynamicClient::new(self.clone(), contract.clone())
            .call_json(method, request)
            .await
    }
}
