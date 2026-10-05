//! The tracer behind the `tracing-opentelemetry` span layer.
//!
//! The span layer is installed once with the global subscriber and can never
//! be replaced: `tracing-opentelemetry` finds itself by downcasting the
//! dispatcher (`OpenTelemetrySpanExt::set_parent` / `context`), and a
//! `reload::Layer` does not forward downcasts. So the layer stays put, its
//! per-layer filter is reloaded (off until a provider exists), and the tracer it
//! holds is this indirection, which forwards to whichever provider the current
//! pipeline generation installed.

use std::sync::{OnceLock, RwLock};

use opentelemetry::global::{BoxedSpan, BoxedTracer};
use opentelemetry::trace::noop::NoopTracer;
use opentelemetry::trace::{SpanBuilder, Tracer};
use opentelemetry::Context;

static CURRENT: RwLock<Option<BoxedTracer>> = RwLock::new(None);

fn noop() -> &'static BoxedTracer {
    static NOOP: OnceLock<BoxedTracer> = OnceLock::new();
    NOOP.get_or_init(|| BoxedTracer::new(Box::new(NoopTracer::new())))
}

/// Install (or clear) the tracer every [`SwapTracer`] forwards to.
pub(crate) fn set(tracer: Option<BoxedTracer>) {
    *CURRENT.write().unwrap_or_else(|e| e.into_inner()) = tracer;
}

/// A tracer that forwards to the current pipeline's tracer, or to a no-op one
/// when there is none (or in a child forked after init).
#[derive(Debug, Clone, Copy, Default)]
pub struct SwapTracer;

impl Tracer for SwapTracer {
    type Span = BoxedSpan;

    fn build_with_context(&self, builder: SpanBuilder, parent_cx: &Context) -> Self::Span {
        if crate::fork::forked() {
            return noop().build_with_context(builder, parent_cx);
        }
        let guard = CURRENT.read().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(t) => t.build_with_context(builder, parent_cx),
            None => noop().build_with_context(builder, parent_cx),
        }
    }
}
