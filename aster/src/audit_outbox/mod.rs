//! Explicit durable audit events, independent of ordinary tracing/logging.
//!
//! A successful [`AuditOutbox::submit`] means an immediate-durability local commit.
//! Delivery is at least once and requires the receiver's exact durable batch ACK.
//! Cancellation does not retract queued/committed work; resubmission can duplicate it.
//! The receiver must persist opaque envelope bytes and deduplicate by the entire ID.
//! Clones share the database, workers, capacity and closing state. Explicit `close`
//! waits for worker shutdown and releases the directory lock without remote delivery.
#[cfg(test)]
mod review_tests;
mod store;
mod types;
pub mod wire;
mod worker;
pub use types::*;

use crossbeam_channel::{bounded, Sender};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    },
    time::Duration,
};
use tokio::sync::oneshot;
use worker::{Control, Submission};

pub(crate) struct Admission {
    closing: bool,
    ticket: u64,
    records: usize,
    bytes: usize,
}
pub(crate) struct Shared {
    submissions: Sender<Submission>,
    controls: Sender<Control>,
    admission: Mutex<Admission>,
    status: Mutex<OutboxStatus>,
    stop_delivery: AtomicBool,
    storage_failed: AtomicBool,
    delivery_generation: AtomicU64,
    closed_notify: tokio::sync::Notify,
    /// Last committed acknowledgement-log cursor; waiters re-query when it moves.
    /// The storage worker also bumps it on exit so waiters observe closing.
    ack_cursor: tokio::sync::watch::Sender<u64>,
    handles: AtomicUsize,
    wake_lock: Mutex<u64>,
    wake: Condvar,
    max_event_bytes: usize,
    #[cfg(test)]
    hooks: tests::Hooks,
}
impl Shared {
    fn signal(&self) {
        let mut generation = self.wake_lock.lock().unwrap_or_else(|p| p.into_inner());
        *generation = generation.wrapping_add(1);
        self.wake.notify_all();
    }
    fn request_close(&self) {
        self.admission
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .closing = true;
        // Wake storage independently of the reserved control channel being full.
        let _ = self.controls.try_send(Control::Wake);
        self.signal();
    }
}

pub struct AuditOutbox {
    shared: Arc<Shared>,
}
impl std::fmt::Debug for AuditOutbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuditOutbox").finish_non_exhaustive()
    }
}
impl Clone for AuditOutbox {
    fn clone(&self) -> Self {
        self.shared.handles.fetch_add(1, Ordering::Relaxed);
        Self {
            shared: self.shared.clone(),
        }
    }
}
impl Drop for AuditOutbox {
    fn drop(&mut self) {
        if self.shared.handles.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.shared.request_close();
        }
    }
}

impl AuditOutbox {
    pub async fn open(config: OutboxConfig) -> Result<Self> {
        config.validate()?;
        let (submissions, submission_receiver) = bounded(QUEUE_EVENTS);
        let (controls, control_receiver) = bounded(CONTROL_CAPACITY);
        let (ready, completion) = oneshot::channel();
        let shared = Arc::new(Shared {
            submissions,
            controls,
            admission: Mutex::new(Admission {
                closing: false,
                ticket: 0,
                records: 0,
                bytes: 0,
            }),
            status: Mutex::new(OutboxStatus {
                outbox_id: String::new(),
                committed_sequence: 0,
                pending_records: 0,
                pending_bytes: 0,
                dead_letter_records: 0,
                dead_letter_bytes: 0,
                disposition_receipts: 0,
                disposition_bytes: 0,
                retained_bytes: 0,
                max_retained_bytes: config.max_retained_bytes,
                queued_records: 0,
                queued_bytes: 0,
                oldest_age_ms: None,
                accepting: false,
                ready: false,
                delivery_state: "starting".into(),
                last_delivery_error: None,
                storage_worker_alive: true,
                storage_recovered: false,
                delivery_worker_alive: false,
                maintenance: false,
                maintenance_duration_ms: None,
                closed: false,
                durable_commits: 0,
                last_group_size: 0,
            }),
            stop_delivery: AtomicBool::new(false),
            storage_failed: AtomicBool::new(false),
            delivery_generation: AtomicU64::new(1),
            closed_notify: tokio::sync::Notify::new(),
            ack_cursor: tokio::sync::watch::Sender::new(0),
            handles: AtomicUsize::new(1),
            wake_lock: Mutex::new(0),
            wake: Condvar::new(),
            max_event_bytes: config.max_event_bytes,
            #[cfg(test)]
            hooks: tests::Hooks::default(),
        });
        // This owner is the cancellation guard: dropping open's future signals worker cleanup.
        let owner = Self {
            shared: shared.clone(),
        };
        std::thread::Builder::new()
            .name("aster-audit-storage".into())
            .spawn(move || {
                worker::storage_entry(config, shared, submission_receiver, control_receiver, ready);
            })
            .map_err(|_| AuditError::worker())?;
        completion.await.map_err(|_| AuditError::worker())??;
        Ok(owner)
    }

    pub async fn submit(&self, event: AuditEvent) -> Result<EventId> {
        let bytes = wire::validate_event(&event, self.shared.max_event_bytes)?;
        let (reply, completion) = oneshot::channel();
        {
            let mut admission = self
                .shared
                .admission
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if admission.closing {
                return Err(AuditError::new(
                    AuditErrorKind::Closed,
                    "audit outbox is closing or closed",
                ));
            }
            let status = self.shared.status.lock().unwrap_or_else(|p| p.into_inner());
            if !status.storage_worker_alive {
                return Err(AuditError::worker());
            }
            if status.maintenance {
                return Err(AuditError::new(
                    AuditErrorKind::Maintenance,
                    "audit storage maintenance is active",
                ));
            }
            drop(status);
            if admission.records == QUEUE_EVENTS
                || admission.bytes.saturating_add(bytes) > QUEUE_BYTES
            {
                return Err(AuditError::new(
                    AuditErrorKind::QueueOverloaded,
                    "bounded audit submission queue is full",
                ));
            }
            let ticket = admission
                .ticket
                .checked_add(1)
                .ok_or_else(AuditError::worker)?;
            let submission = Submission {
                event,
                bytes,
                ticket,
                reply,
            };
            // Assign a ticket only after successful enqueue; failed admission creates no barrier hole.
            self.shared
                .submissions
                .try_send(submission)
                .map_err(|error| match error {
                    crossbeam_channel::TrySendError::Full(_) => AuditError::new(
                        AuditErrorKind::QueueOverloaded,
                        "bounded audit submission queue is full",
                    ),
                    crossbeam_channel::TrySendError::Disconnected(_) => AuditError::worker(),
                })?;
            admission.ticket = ticket;
            admission.records += 1;
            admission.bytes += bytes;
        }
        completion.await.map_err(|_| AuditError::worker())?
    }

    pub async fn status(&self) -> Result<OutboxStatus> {
        let admission = self
            .shared
            .admission
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let mut status = self
            .shared
            .status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        status.queued_records = admission.records;
        status.queued_bytes = admission.bytes;
        status.accepting &=
            !admission.closing && admission.records < QUEUE_EVENTS && admission.bytes < QUEUE_BYTES;
        status.ready &= status.accepting;
        Ok(status)
    }
    fn enqueue(&self, control: Control) -> Result<()> {
        let admission = self
            .shared
            .admission
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if admission.closing {
            return Err(AuditError::new(
                AuditErrorKind::Closed,
                "audit outbox is closing or closed",
            ));
        }
        if !self
            .shared
            .status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .storage_worker_alive
        {
            return Err(AuditError::worker());
        }
        self.shared
            .controls
            .try_send(control)
            .map_err(|error| match error {
                crossbeam_channel::TrySendError::Full(_) => AuditError::new(
                    AuditErrorKind::QueueOverloaded,
                    "bounded audit control queue is full",
                ),
                crossbeam_channel::TrySendError::Disconnected(_) => AuditError::worker(),
            })
    }
    pub async fn flush(&self, timeout: Duration) -> Result<()> {
        if timeout.is_zero() {
            return Err(AuditError::new(
                AuditErrorKind::Timeout,
                "audit flush timed out",
            ));
        }
        let deadline = std::time::Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| {
                AuditError::new(AuditErrorKind::InvalidConfig, "flush duration is too large")
            })?;
        let (reply, completion) = oneshot::channel();
        {
            let admission = self
                .shared
                .admission
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if admission.closing {
                return Err(AuditError::new(
                    AuditErrorKind::Closed,
                    "audit outbox is closing or closed",
                ));
            }
            self.shared
                .controls
                .try_send(Control::Flush {
                    ticket: admission.ticket,
                    deadline,
                    reply,
                })
                .map_err(|error| match error {
                    crossbeam_channel::TrySendError::Full(_) => AuditError::new(
                        AuditErrorKind::QueueOverloaded,
                        "audit control queue is full",
                    ),
                    crossbeam_channel::TrySendError::Disconnected(_) => AuditError::worker(),
                })?;
        }
        tokio::time::timeout(timeout, completion)
            .await
            .map_err(|_| AuditError::new(AuditErrorKind::Timeout, "audit flush timed out"))?
            .map_err(|_| AuditError::worker())?
    }
    pub async fn close(&self) -> Result<()> {
        self.shared.request_close();
        // Subscription-style completion avoids filling control capacity with cloned close calls.
        loop {
            let notified = self.shared.closed_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let status = self.shared.status.lock().unwrap_or_else(|p| p.into_inner());
                if status.closed {
                    return if !self.shared.storage_failed.load(Ordering::Acquire) {
                        Ok(())
                    } else {
                        Err(AuditError::worker())
                    };
                }
            }
            notified.await;
        }
    }
    pub async fn reconfigure(&self, destination: Destination) -> Result<()> {
        destination.fingerprint()?;
        let (reply, completion) = oneshot::channel();
        self.enqueue(Control::Reconfigure { destination, reply })?;
        completion.await.map_err(|_| AuditError::worker())?
    }
    pub async fn quarantine(
        &self,
        event_id: String,
        reason: String,
        operator_reference: String,
    ) -> Result<()> {
        wire::parse_id(&event_id)?;
        if reason.is_empty()
            || reason.len() > 512
            || operator_reference.is_empty()
            || operator_reference.len() > 256
        {
            return Err(AuditError::new(
                AuditErrorKind::InvalidEvent,
                "quarantine requires bounded reason and operator reference",
            ));
        }
        let (reply, completion) = oneshot::channel();
        self.enqueue(Control::Quarantine {
            event_id,
            reason,
            operator_reference,
            reply,
        })?;
        completion.await.map_err(|_| AuditError::worker())?
    }
    pub async fn export(&self, event_id: String) -> Result<QuarantineExport> {
        wire::parse_id(&event_id)?;
        let (reply, completion) = oneshot::channel();
        self.enqueue(Control::Export { event_id, reply })?;
        completion.await.map_err(|_| AuditError::worker())?
    }
    /// Up to `limit` (1..=[`MAX_ACK_PAGE`]) acknowledgement-log entries with a
    /// cursor greater than `after`, oldest first. Pass 0 to start from the oldest
    /// retained entry and the last returned `cursor` to continue. Entries are
    /// pruned by the configured count and age bounds; a consumer that falls
    /// behind sees a gap in cursors, never a reordering.
    pub async fn acknowledgements(&self, after: u64, limit: usize) -> Result<Vec<Acknowledgement>> {
        if limit == 0 || limit > MAX_ACK_PAGE {
            return Err(AuditError::new(
                AuditErrorKind::InvalidEvent,
                "acknowledgement page limit must be between 1 and 1000",
            ));
        }
        let (reply, completion) = oneshot::channel();
        self.enqueue(Control::Acknowledgements {
            after,
            limit,
            reply,
        })?;
        completion.await.map_err(|_| AuditError::worker())?
    }

    /// Like [`Self::acknowledgements`], but when no entry after `after` exists yet,
    /// waits up to `timeout` for the next committed acknowledgement. Returns an
    /// empty page on timeout. Loop on the last cursor for a live, lossless feed.
    pub async fn wait_acknowledgements(
        &self,
        after: u64,
        limit: usize,
        timeout: Duration,
    ) -> Result<Vec<Acknowledgement>> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut cursor = self.shared.ack_cursor.subscribe();
        loop {
            cursor.borrow_and_update();
            let page = self.acknowledgements(after, limit).await?;
            if !page.is_empty() {
                return Ok(page);
            }
            match tokio::time::timeout_at(deadline, cursor.changed()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => return Err(AuditError::worker()),
                Err(_) => return Ok(page),
            }
        }
    }

    pub async fn replay(&self, event_id: String) -> Result<()> {
        wire::parse_id(&event_id)?;
        let (reply, completion) = oneshot::channel();
        self.enqueue(Control::Replay { event_id, reply })?;
        completion.await.map_err(|_| AuditError::worker())?
    }
}

#[cfg(test)]
mod fixture;
#[cfg(test)]
mod tests;
