//! Thin raw WebTransport owners and cancellation tokens for NAPI.
use aster_transport_salvo::raw_webtransport::{
    RawWtConfig, RawWtListener, RawWtSession, RawWtStream,
};
use napi::bindgen_prelude::*;
use napi_derive::napi;
use std::{future::Future, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

#[napi]
pub struct RawWtCancel {
    inner: CancellationToken,
}
#[napi]
impl RawWtCancel {
    #[napi(constructor)]
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            inner: CancellationToken::new(),
        }
    }
    #[napi]
    pub fn cancel(&self) {
        self.inner.cancel();
    }
}
fn token(t: Option<&RawWtCancel>) -> CancellationToken {
    t.map(|t| t.inner.clone()).unwrap_or_default()
}
async fn operation<T>(
    cancel: CancellationToken,
    future: impl Future<Output = std::result::Result<T, String>>,
) -> Result<T> {
    // Once a native result has won, return it rather than abandon a fresh owner.
    if cancel.is_cancelled() {
        return Err(Error::from_reason("WebTransport operation cancelled"));
    }
    tokio::select! { biased; result=future=>result.map_err(Error::from_reason), ()=cancel.cancelled()=>Err(Error::from_reason("WebTransport operation cancelled")) }
}
fn json<T: serde::Serialize>(v: &T) -> Result<String> {
    serde_json::to_string(v).map_err(|e| Error::from_reason(e.to_string()))
}
#[napi(object)]
pub struct RawWtOptions {
    pub bind_addr: Option<String>,
    pub certificate_pem: Buffer,
    pub private_key_pem: Buffer,
    pub path: Option<String>,
    pub max_sessions: Option<u32>,
    pub max_streams: Option<u32>,
    pub max_datagrams: Option<u32>,
    pub max_datagram_bytes: Option<u32>,
    pub max_read_bytes: Option<u32>,
}
impl RawWtOptions {
    fn config(self) -> RawWtConfig {
        RawWtConfig {
            bind_addr: self.bind_addr.unwrap_or_else(|| "127.0.0.1:0".into()),
            certificate_pem: self.certificate_pem.to_vec(),
            private_key_pem: self.private_key_pem.to_vec(),
            path: self.path.unwrap_or_else(|| "/wt".into()),
            max_sessions: self.max_sessions.unwrap_or(64) as usize,
            max_streams: self.max_streams.unwrap_or(128) as usize,
            max_datagrams: self.max_datagrams.unwrap_or(128) as usize,
            max_datagram_bytes: self.max_datagram_bytes.unwrap_or(65536) as usize,
            max_read_bytes: self.max_read_bytes.unwrap_or(1048576) as usize,
        }
    }
}
#[napi]
pub struct RawWtListenerHandle {
    inner: RawWtListener,
    runtime: tokio::runtime::Handle,
}
#[napi]
pub fn raw_wt_start<'env>(
    env: &'env Env,
    options: RawWtOptions,
    upcalls: Option<&crate::upcalls::Upcalls>,
) -> Result<PromiseRaw<'env, RawWtListenerHandle>> {
    let auth = upcalls
        .map(|u| {
            let table = u.table();
            if table.is_closed() {
                return Err(Error::from_reason("authentication table closed"));
            }
            Ok(
                Arc::new(aster::binding_support::upcall::auth::ForeignAuthenticator::new(table))
                    as Arc<dyn aster::rpc::Authenticator>,
            )
        })
        .transpose()?;
    let config = options.config();
    env.spawn_future(async move {
        let inner = match auth {
            Some(auth) => RawWtListener::start_authenticated(config, auth).await,
            None => RawWtListener::start_public(config).await,
        }
        .map_err(Error::from_reason)?;
        Ok(RawWtListenerHandle {
            inner,
            runtime: tokio::runtime::Handle::current(),
        })
    })
}
#[napi]
impl RawWtListenerHandle {
    #[napi(getter)]
    pub fn tcp_addr(&self) -> String {
        self.inner.tcp_addr().to_string()
    }
    #[napi(getter)]
    pub fn quic_addr(&self) -> String {
        self.inner.quic_addr().to_string()
    }
    #[napi]
    pub fn accept<'env>(
        &self,
        env: &'env Env,
        cancel: Option<&RawWtCancel>,
    ) -> Result<PromiseRaw<'env, RawWtSessionHandle>> {
        let inner = self.inner.clone();
        let cancel = token(cancel);
        env.spawn_future(async move {
            Ok(RawWtSessionHandle {
                inner: operation(cancel, inner.accept()).await?,
            })
        })
    }
    #[napi]
    pub fn stop<'env>(&self, env: &'env Env, grace_ms: u32) -> Result<PromiseRaw<'env, ()>> {
        let inner = self.inner.clone();
        env.spawn_future(async move {
            inner
                .stop(Duration::from_millis(grace_ms as u64))
                .await
                .map_err(Error::from_reason)
        })
    }
}
impl Drop for RawWtListenerHandle {
    fn drop(&mut self) {
        self.inner.close();
        let inner = self.inner.clone();
        self.runtime.spawn(async move {
            let _ = inner.stop(Duration::ZERO).await;
        });
    }
}
#[napi]
pub struct RawWtSessionHandle {
    inner: RawWtSession,
}
#[napi]
impl RawWtSessionHandle {
    #[napi]
    pub fn info(&self) -> Result<String> {
        json(&self.inner.info())
    }
    #[napi]
    pub fn stats(&self) -> Result<String> {
        json(&self.inner.stats())
    }
    #[napi]
    pub fn max_datagram_size(&self) -> Option<u32> {
        self.inner.max_datagram_size().map(|n| n as u32)
    }
    #[napi]
    pub fn send_datagram(&self, data: Buffer) -> Result<()> {
        self.inner.send_datagram(&data).map_err(Error::from_reason)
    }
    #[napi]
    pub fn receive_datagram<'env>(
        &self,
        env: &'env Env,
        cancel: Option<&RawWtCancel>,
    ) -> Result<PromiseRaw<'env, Buffer>> {
        let inner = self.inner.clone();
        let cancel = token(cancel);
        env.spawn_future(async move {
            operation(cancel, inner.receive_datagram())
                .await
                .map(Into::into)
        })
    }
    #[napi]
    pub fn accept_bi<'env>(
        &self,
        env: &'env Env,
        cancel: Option<&RawWtCancel>,
    ) -> Result<PromiseRaw<'env, RawWtStreamHandle>> {
        let inner = self.inner.clone();
        let cancel = token(cancel);
        env.spawn_future(async move {
            Ok(RawWtStreamHandle::new(
                operation(cancel, inner.accept_bi()).await?,
            ))
        })
    }
    #[napi]
    pub fn accept_uni<'env>(
        &self,
        env: &'env Env,
        cancel: Option<&RawWtCancel>,
    ) -> Result<PromiseRaw<'env, RawWtStreamHandle>> {
        let inner = self.inner.clone();
        let cancel = token(cancel);
        env.spawn_future(async move {
            Ok(RawWtStreamHandle::new(
                operation(cancel, inner.accept_uni()).await?,
            ))
        })
    }
    #[napi]
    pub fn open_bi<'env>(
        &self,
        env: &'env Env,
        cancel: Option<&RawWtCancel>,
    ) -> Result<PromiseRaw<'env, RawWtStreamHandle>> {
        let inner = self.inner.clone();
        let cancel = token(cancel);
        env.spawn_future(async move {
            Ok(RawWtStreamHandle::new(
                operation(cancel, inner.open_bi()).await?,
            ))
        })
    }
    #[napi]
    pub fn open_uni<'env>(
        &self,
        env: &'env Env,
        cancel: Option<&RawWtCancel>,
    ) -> Result<PromiseRaw<'env, RawWtStreamHandle>> {
        let inner = self.inner.clone();
        let cancel = token(cancel);
        env.spawn_future(async move {
            Ok(RawWtStreamHandle::new(
                operation(cancel, inner.open_uni()).await?,
            ))
        })
    }
    #[napi]
    pub fn close(&self, code: u32, reason: Buffer) -> Result<()> {
        self.inner.close(code, &reason).map_err(Error::from_reason)
    }
    #[napi]
    pub fn closed<'env>(
        &self,
        env: &'env Env,
        cancel: Option<&RawWtCancel>,
    ) -> Result<PromiseRaw<'env, String>> {
        let inner = self.inner.clone();
        let cancel = token(cancel);
        env.spawn_future(async move {
            operation(cancel, async {
                serde_json::to_string(&inner.closed().await).map_err(|e| e.to_string())
            })
            .await
        })
    }
}
impl Drop for RawWtSessionHandle {
    fn drop(&mut self) {
        let _ = self.inner.close(0, b"JavaScript session released");
    }
}
#[napi]
pub struct RawWtStreamHandle {
    inner: RawWtStream,
    runtime: tokio::runtime::Handle,
}
impl RawWtStreamHandle {
    fn new(inner: RawWtStream) -> Self {
        Self {
            inner,
            runtime: tokio::runtime::Handle::current(),
        }
    }
}
#[napi]
impl RawWtStreamHandle {
    #[napi]
    pub fn info(&self) -> Result<String> {
        json(&self.inner.info())
    }
    #[napi]
    pub fn read<'env>(
        &self,
        env: &'env Env,
        maximum: u32,
        cancel: Option<&RawWtCancel>,
    ) -> Result<PromiseRaw<'env, Buffer>> {
        let inner = self.inner.clone();
        let cancel = token(cancel);
        env.spawn_future(async move {
            operation(cancel, inner.read(maximum as usize))
                .await
                .map(Into::into)
        })
    }
    #[napi]
    pub fn write<'env>(
        &self,
        env: &'env Env,
        data: Buffer,
        cancel: Option<&RawWtCancel>,
    ) -> Result<PromiseRaw<'env, ()>> {
        let inner = self.inner.clone();
        let cancel = token(cancel);
        if data.len() > inner.max_write_size() {
            return Err(Error::from_reason("write exceeds configured byte limit"));
        }
        let data = data.to_vec();
        env.spawn_future(async move { operation(cancel, inner.write(&data)).await })
    }
    #[napi]
    pub fn finish<'env>(
        &self,
        env: &'env Env,
        cancel: Option<&RawWtCancel>,
    ) -> Result<PromiseRaw<'env, ()>> {
        let inner = self.inner.clone();
        let cancel = token(cancel);
        env.spawn_future(async move { operation(cancel, inner.finish()).await })
    }
    #[napi]
    pub fn reset<'env>(&self, env: &'env Env, code: u32) -> Result<PromiseRaw<'env, ()>> {
        let inner = self.inner.clone();
        env.spawn_future(async move { inner.reset(code).await.map_err(Error::from_reason) })
    }
    #[napi]
    pub fn stop<'env>(&self, env: &'env Env, code: u32) -> Result<PromiseRaw<'env, ()>> {
        let inner = self.inner.clone();
        env.spawn_future(async move { inner.stop(code).await.map_err(Error::from_reason) })
    }
    #[napi]
    pub fn set_priority<'env>(
        &self,
        env: &'env Env,
        priority: i32,
    ) -> Result<PromiseRaw<'env, ()>> {
        let inner = self.inner.clone();
        env.spawn_future(async move {
            inner
                .set_priority(priority)
                .await
                .map_err(Error::from_reason)
        })
    }
    #[napi]
    pub fn priority<'env>(&self, env: &'env Env) -> Result<PromiseRaw<'env, Option<i32>>> {
        let inner = self.inner.clone();
        env.spawn_future(async move { inner.priority().await.map_err(Error::from_reason) })
    }
    #[napi]
    pub fn transport_acked<'env>(
        &self,
        env: &'env Env,
        cancel: Option<&RawWtCancel>,
    ) -> Result<PromiseRaw<'env, bool>> {
        let inner = self.inner.clone();
        let cancel = token(cancel);
        env.spawn_future(async move { operation(cancel, inner.transport_acked()).await })
    }
}
impl Drop for RawWtStreamHandle {
    fn drop(&mut self) {
        let inner = self.inner.clone();
        self.runtime.spawn(async move {
            if inner.info().has_send {
                let _ = inner.reset(0).await;
            }
            if inner.info().has_receive {
                let _ = inner.stop(0).await;
            }
        });
    }
}
