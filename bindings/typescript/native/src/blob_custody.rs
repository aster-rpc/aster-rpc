//! Thin custody adapters. Admission occurs synchronously, before scheduling native work.
use crate::blobs::{BlobsClient, FetchReport};
use aster_transport_core::{CoreBlobCustody, CoreBlobCustodyError, CoreFetchStrategy};
use napi::bindgen_prelude::*;
use napi_derive::napi;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

pub(crate) fn custody_err(error: anyhow::Error) -> Error {
    let kind = match error.downcast_ref::<CoreBlobCustodyError>() {
        Some(CoreBlobCustodyError::Closed) => "ASTER_BLOB_CLOSED",
        Some(CoreBlobCustodyError::Cancelled) => "ASTER_BLOB_CANCELLED",
        Some(CoreBlobCustodyError::HashMismatch { .. }) => "ASTER_BLOB_HASH_MISMATCH",
        Some(CoreBlobCustodyError::UnprotectedRoot) => "ASTER_BLOB_UNPROTECTED_ROOT",
        None => "ASTER_BLOB_ERROR",
    };
    Error::from_reason(format!("{kind}: {error:#}"))
}

#[napi]
pub struct BlobCancelToken {
    flag: Arc<AtomicBool>,
}
#[napi]
impl BlobCancelToken {
    #[napi(constructor)]
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
        }
    }
    #[napi]
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Release);
    }
}
fn flag(token: Option<&BlobCancelToken>) -> Option<Arc<AtomicBool>> {
    token.map(|t| t.flag.clone())
}

#[napi]
pub struct BlobCustody {
    scope: CoreBlobCustody,
}
impl Drop for BlobCustody {
    fn drop(&mut self) {
        self.scope.close();
    }
}

#[napi]
impl BlobCustody {
    /// Seal new admission; accepted leases keep every root protected until cleanup finishes.
    #[napi]
    pub fn close(&self) {
        self.scope.close();
    }
    #[napi]
    pub fn protect<'env>(
        &self,
        env: &'env Env,
        hash: String,
        format: String,
        token: Option<&BlobCancelToken>,
    ) -> Result<PromiseRaw<'env, ()>> {
        let lease = self.scope.lease().map_err(custody_err)?;
        let flag = flag(token);
        env.spawn_future(async move {
            lease
                .protect_cancellable(hash, format, flag)
                .await
                .map_err(custody_err)
        })
    }
    #[napi]
    pub fn add_bytes<'env>(
        &self,
        env: &'env Env,
        data: Buffer,
        format: String,
        token: Option<&BlobCancelToken>,
    ) -> Result<PromiseRaw<'env, String>> {
        let lease = self.scope.lease().map_err(custody_err)?;
        let flag = flag(token);
        let data = data.to_vec();
        env.spawn_future(async move {
            lease
                .add_bytes_cancellable(data, format, flag)
                .await
                .map_err(custody_err)
        })
    }
    #[napi]
    pub fn add_path<'env>(
        &self,
        env: &'env Env,
        path: String,
        format: String,
        token: Option<&BlobCancelToken>,
    ) -> Result<PromiseRaw<'env, String>> {
        let lease = self.scope.lease().map_err(custody_err)?;
        let flag = flag(token);
        env.spawn_future(async move {
            lease
                .add_path_cancellable(path, format, flag)
                .await
                .map_err(custody_err)
        })
    }
    #[napi]
    pub fn retain<'env>(
        &self,
        env: &'env Env,
        hash: String,
        format: String,
        tag: String,
        token: Option<&BlobCancelToken>,
    ) -> Result<PromiseRaw<'env, ()>> {
        let lease = self.scope.lease().map_err(custody_err)?;
        let flag = flag(token);
        env.spawn_future(async move {
            lease
                .retain(hash, format, tag, flag)
                .await
                .map_err(custody_err)
        })
    }
    #[napi]
    pub fn read<'env>(
        &self,
        env: &'env Env,
        hash: String,
        token: Option<&BlobCancelToken>,
    ) -> Result<PromiseRaw<'env, Buffer>> {
        let lease = self.scope.lease().map_err(custody_err)?;
        let flag = flag(token);
        env.spawn_future(async move {
            lease
                .read_cancellable(hash, flag)
                .await
                .map(Buffer::from)
                .map_err(custody_err)
        })
    }
    /// Store-only fetch, interpreting HashSeq as a hash graph rather than a Collection.
    #[napi]
    pub fn download_hash_from<'env>(
        &self,
        env: &'env Env,
        hash: String,
        providers: Vec<String>,
        format: String,
        strategy: String,
        token: Option<&BlobCancelToken>,
    ) -> Result<PromiseRaw<'env, FetchReport>> {
        let strategy = match strategy.as_str() {
            "whole" => CoreFetchStrategy::Whole,
            "split_children" => CoreFetchStrategy::SplitChildren,
            _ => {
                return Err(Error::from_reason(
                    "strategy must be whole or split_children",
                ))
            }
        };
        let lease = self.scope.lease().map_err(custody_err)?;
        let flag = flag(token);
        env.spawn_future(async move {
            let report = lease
                .download_cancellable(hash, format, providers, strategy, flag)
                .await
                .map_err(custody_err)?;
            let report: aster::FetchReport = report.into();
            Ok(report.into())
        })
    }
}

#[napi]
impl BlobsClient {
    #[napi]
    pub fn custody(&self) -> BlobCustody {
        BlobCustody {
            scope: self.inner.custody(),
        }
    }
    #[napi]
    pub fn add_bytes_verified<'env>(
        &self,
        env: &'env Env,
        data: Buffer,
        expected_hash: String,
        format: String,
        tag: String,
        token: Option<&BlobCancelToken>,
    ) -> Result<PromiseRaw<'env, String>> {
        let client = self.inner.clone();
        let flag = flag(token);
        let data = data.to_vec();
        env.spawn_future(async move {
            client
                .add_bytes_verified(data, expected_hash, format, tag, flag)
                .await
                .map_err(custody_err)
        })
    }
    #[napi]
    pub fn add_path_verified<'env>(
        &self,
        env: &'env Env,
        path: String,
        expected_hash: String,
        format: String,
        tag: String,
        token: Option<&BlobCancelToken>,
    ) -> Result<PromiseRaw<'env, String>> {
        let client = self.inner.clone();
        let flag = flag(token);
        env.spawn_future(async move {
            client
                .add_path_verified(path, expected_hash, format, tag, flag)
                .await
                .map_err(custody_err)
        })
    }
}
