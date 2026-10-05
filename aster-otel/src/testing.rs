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

    /// The data points of metric `name` in the most recent export that has
    /// it (cumulative temporality, the default, so that export holds the
    /// totals). Empty when it was never exported. Flush first.
    pub fn metric_points(&self, name: &str) -> Vec<MetricPoint> {
        let exports = self.metrics.get_finished_metrics().unwrap_or_default();
        for rm in exports.iter().rev() {
            for sm in rm.scope_metrics() {
                if let Some(m) = sm.metrics().find(|m| m.name() == name) {
                    return points(m.data());
                }
            }
        }
        Vec::new()
    }
}

/// One exported data point, flattened for assertions.
#[derive(Clone, Debug, PartialEq)]
pub struct MetricPoint {
    /// Attribute key/value pairs (values as strings), sorted by key.
    pub attributes: Vec<(String, String)>,
    /// The sum (counters, up-down counters, histograms) or the last value
    /// (gauges).
    pub value: f64,
    /// Histograms: the number of recordings. Otherwise 0.
    pub count: u64,
}

impl MetricPoint {
    /// The value of attribute `key`.
    pub fn attr(&self, key: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

fn points(data: &opentelemetry_sdk::metrics::data::AggregatedMetrics) -> Vec<MetricPoint> {
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};

    fn attrs<'a>(kv: impl Iterator<Item = &'a opentelemetry::KeyValue>) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = kv
            .map(|kv| (kv.key.to_string(), kv.value.to_string()))
            .collect();
        v.sort();
        v
    }
    fn of<T: AsF64>(d: &MetricData<T>) -> Vec<MetricPoint> {
        match d {
            MetricData::Sum(s) => s
                .data_points()
                .map(|p| MetricPoint {
                    attributes: attrs(p.attributes()),
                    value: p.value().as_f64(),
                    count: 0,
                })
                .collect(),
            MetricData::Gauge(g) => g
                .data_points()
                .map(|p| MetricPoint {
                    attributes: attrs(p.attributes()),
                    value: p.value().as_f64(),
                    count: 0,
                })
                .collect(),
            MetricData::Histogram(h) => h
                .data_points()
                .map(|p| MetricPoint {
                    attributes: attrs(p.attributes()),
                    value: p.sum().as_f64(),
                    count: p.count(),
                })
                .collect(),
            MetricData::ExponentialHistogram(h) => h
                .data_points()
                .map(|p| MetricPoint {
                    attributes: attrs(p.attributes()),
                    value: p.sum().as_f64(),
                    count: p.count() as u64,
                })
                .collect(),
        }
    }
    match data {
        AggregatedMetrics::F64(d) => of(d),
        AggregatedMetrics::U64(d) => of(d),
        AggregatedMetrics::I64(d) => of(d),
    }
}

/// Data point values as `f64` (test values are small, so the casts are exact).
trait AsF64: Copy {
    fn as_f64(self) -> f64;
}

impl AsF64 for f64 {
    fn as_f64(self) -> f64 {
        self
    }
}

impl AsF64 for u64 {
    fn as_f64(self) -> f64 {
        self as f64
    }
}

impl AsF64 for i64 {
    fn as_f64(self) -> f64 {
        self as f64
    }
}
