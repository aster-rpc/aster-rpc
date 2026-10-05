//! In-memory exporters for tests (feature `testing`).
//!
//! Put a [`TestExporters`] in [`crate::ObservabilityConfig::test_exporters`]
//! and `init` exports every signal to it instead of OTLP, through the same
//! batch processors and readers the real pipeline uses (so flush and shutdown
//! behave as in production).

use std::sync::{Arc, Mutex};

pub use opentelemetry_sdk::logs::InMemoryLogExporter;
pub use opentelemetry_sdk::metrics::InMemoryMetricExporter;
pub use opentelemetry_sdk::resource::Resource;
pub use opentelemetry_sdk::trace::{InMemorySpanExporter, SpanData};

use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::SpanExporter;

/// A span exporter that records every exported span and the resource it was
/// given. Unlike [`InMemorySpanExporter`] it keeps its records across
/// `shutdown`, so a test can assert what shutdown flushed.
#[derive(Clone, Debug, Default)]
pub struct SpanRecorder {
    spans: Arc<Mutex<Vec<SpanData>>>,
    resource: Arc<Mutex<Option<Resource>>>,
}

impl SpanRecorder {
    /// Every span exported so far.
    pub fn finished_spans(&self) -> Vec<SpanData> {
        self.spans.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The resource the provider handed the exporter.
    pub fn resource(&self) -> Option<Resource> {
        self.resource
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl SpanExporter for SpanRecorder {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        self.spans
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(batch);
        Ok(())
    }

    fn set_resource(&mut self, resource: &Resource) {
        *self.resource.lock().unwrap_or_else(|e| e.into_inner()) = Some(resource.clone());
    }
}

/// One in-memory exporter per signal. Clones share storage.
#[derive(Clone, Debug, Default)]
pub struct TestExporters {
    pub spans: SpanRecorder,
    pub metrics: InMemoryMetricExporter,
    /// Used with feature `otlp-logs`. Note: the SDK's in-memory log exporter
    /// clears its records on shutdown; assert after a `flush`.
    pub logs: InMemoryLogExporter,
}

impl TestExporters {
    /// Fresh, empty exporters.
    pub fn new() -> Self {
        Self::default()
    }

    /// Names of the spans exported so far, in export order.
    pub fn span_names(&self) -> Vec<String> {
        self.spans
            .finished_spans()
            .into_iter()
            .map(|s| s.name.into_owned())
            .collect()
    }
}
