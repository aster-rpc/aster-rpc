//! Test-only phase timing. Never log requests, values, tickets or credentials.
//! Enabled by the existing `reactor-test-hooks` feature, absent from shipping
//! graphs. A dropped unfinished phase records cancellation/error separately
//! from normal completion; phase IDs are unique within each process.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

static NEXT_PHASE: AtomicU64 = AtomicU64::new(1);

pub(crate) struct Phase {
    id: u64,
    stage: &'static str,
    method: String,
    client: String,
    sequence: u64,
    started: Instant,
    completed: bool,
}

impl Phase {
    pub(crate) fn new(stage: &'static str, method: &str, client: &[u8], sequence: u64) -> Self {
        let phase = Self {
            id: NEXT_PHASE.fetch_add(1, Ordering::Relaxed),
            stage,
            method: method.to_owned(),
            client: hex::encode(client),
            sequence,
            started: Instant::now(),
            completed: false,
        };
        phase.emit("begin");
        phase
    }

    // The journal identity is available only after its mutex is admitted.
    pub(crate) fn identify(&mut self, client: &[u8], sequence: u64) {
        self.client = hex::encode(client);
        self.sequence = sequence;
    }

    pub(crate) fn finish(mut self) {
        self.completed = true;
    }

    fn emit(&self, event: &str) {
        tracing::debug!(target: "aster::coordination::diagnostic",
            pid = std::process::id(), phase = self.id, stage = self.stage,
            method = %self.method, client_id = %self.client, sequence = self.sequence,
            elapsed_us = self.started.elapsed().as_micros() as u64,
            completed = self.completed, event, "coordination phase");
    }
}

impl Drop for Phase {
    fn drop(&mut self) {
        self.emit("end");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<String>>>);
    impl tracing::Subscriber for Capture {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            struct Fields(String);
            impl tracing::field::Visit for Fields {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    use std::fmt::Write;
                    write!(self.0, "{}={value:?};", field.name()).unwrap();
                }
            }
            let mut fields = Fields(String::new());
            event.record(&mut fields);
            self.0.lock().unwrap().push(fields.0);
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    #[test]
    fn admission_and_cancelled_phases_emit_identity_and_completion() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut admitted_id = 0;
        tracing::subscriber::with_default(Capture(events.clone()), || {
            let mut queued = Phase::new("journal_wait", "renew", &[], 0);
            admitted_id = queued.id;
            queued.identify(&[0xab; 16], 42);
            queued.finish();
            // Cancelling a genuinely pending future drops its phase guard
            // synchronously, without a sleep or a scheduler race.
            use std::future::Future;
            let mut future = Box::pin(async {
                let rpc = Phase::new("rpc", "renew", &[0xab; 16], 42);
                std::future::pending::<()>().await;
                rpc.finish();
            });
            let mut context = std::task::Context::from_waker(std::task::Waker::noop());
            assert!(future.as_mut().poll(&mut context).is_pending());
            drop(future);
        });
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 4);
        assert!(events[0].contains(&format!("phase={admitted_id};")));
        assert!(events[1].contains(&format!("phase={admitted_id};")));
        assert!(events[1].contains(&format!("client_id={};", "ab".repeat(16))));
        assert!(events[1].contains("sequence=42;"));
        assert!(events[1].contains("completed=true;"));
        assert!(events[3].contains("stage=\"rpc\";"));
        assert!(events[3].contains("completed=false;"));
        for event in events.iter() {
            assert!(event.contains("elapsed_us="));
            assert!(event.contains("pid="));
            assert!(!event.contains("request="));
            assert!(!event.contains("value="));
        }
    }
}
