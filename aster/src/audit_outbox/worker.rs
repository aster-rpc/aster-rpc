use super::{
    store::{Batch, Store},
    types::*,
    wire::*,
    Shared,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use crossbeam_channel::{Receiver, Sender};
use std::{
    collections::VecDeque,
    io::Read,
    sync::{atomic::Ordering, Arc},
    time::{Duration, Instant},
};
use tokio::sync::oneshot;

pub(crate) struct Submission {
    pub event: AuditEvent,
    pub bytes: usize,
    pub ticket: u64,
    pub reply: oneshot::Sender<Result<EventId>>,
}
pub(crate) enum Control {
    Wake,
    Flush {
        ticket: u64,
        deadline: Instant,
        reply: oneshot::Sender<Result<()>>,
    },
    Reconfigure {
        destination: Destination,
        reply: oneshot::Sender<Result<()>>,
    },
    Quarantine {
        event_id: String,
        reason: String,
        operator_reference: String,
        reply: oneshot::Sender<Result<()>>,
    },
    Export {
        event_id: String,
        reply: oneshot::Sender<Result<QuarantineExport>>,
    },
    Replay {
        event_id: String,
        reply: oneshot::Sender<Result<()>>,
    },
    Fetch {
        reply: Sender<Result<Option<Batch>>>,
    },
    Ack {
        generation: u64,
        ids: Vec<EventId>,
        reply: Sender<Result<()>>,
    },
    Error {
        generation: u64,
        error: DeliveryError,
        isolate_until: Option<u64>,
        stall: bool,
        reply: Sender<Result<()>>,
    },
}
struct Flush {
    ticket: u64,
    watermark: Option<u64>,
    deadline: Instant,
    reply: oneshot::Sender<Result<()>>,
}

pub(crate) fn storage_entry(
    config: OutboxConfig,
    shared: Arc<Shared>,
    submissions: Receiver<Submission>,
    controls: Receiver<Control>,
    ready: oneshot::Sender<Result<()>>,
) {
    let mut ready = Some(ready);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
        let mut store = Store::open(config)?;
        refresh(&store, &shared)?;
        let delivery_shared = shared.clone();
        let (started, delivery_ready) = crossbeam_channel::bounded(1);
        let delivery = std::thread::Builder::new()
            .name("aster-audit-delivery".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    delivery_loop(&delivery_shared, started)
                }));
                let mut status = delivery_shared
                    .status
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                status.delivery_worker_alive = false;
                if !delivery_shared.stop_delivery.load(Ordering::Acquire)
                    || result.is_err()
                    || matches!(result, Ok(Err(_)))
                {
                    status.delivery_state = "worker_failed".into();
                    status.ready = false;
                }
                drop(status);
                let _ = delivery_shared.controls.try_send(Control::Wake);
                delivery_shared.signal();
            })
            .map_err(|_| AuditError::worker())?;
        let startup = delivery_ready
            .recv()
            .map_err(|_| AuditError::worker())
            .and_then(|r| r);
        if startup.is_ok() {
            let _ = ready.take().unwrap().send(Ok(()));
        }
        let outcome = if startup.is_ok() {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                storage_loop(&mut store, &shared, &submissions, &controls)
            }))
            .unwrap_or_else(|_| Err(AuditError::worker()))
        } else {
            startup
        };
        shared.stop_delivery.store(true, Ordering::Release);
        shared.signal();
        // The HTTP timeout bounds join; no network work or join runs on Tokio.
        let join = delivery.join().map_err(|_| AuditError::worker());
        let close = store.close(); // Drop DB and its lifetime lock before completing close.
        outcome.and(join).and(close)
    }))
    .unwrap_or_else(|_| Err(AuditError::worker()));
    if let Some(ready) = ready {
        let _ = ready.send(result.clone());
    }
    if result.is_err() {
        shared.storage_failed.store(true, Ordering::Release);
    }
    shared.stop_delivery.store(true, Ordering::Release);
    shared.signal();
    shared
        .admission
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .closing = true;
    {
        let mut status = shared.status.lock().unwrap_or_else(|p| p.into_inner());
        status.storage_worker_alive = false;
        status.accepting = false;
        status.ready = false;
        status.closed = true;
        if result.is_err() {
            status.delivery_state = "worker_failed".into();
        }
    }
    // Dropping both receivers resolves all outstanding oneshot and synchronous replies.
    drop(submissions);
    drop(controls);
    shared.closed_notify.notify_waiters();
}

fn refresh(store: &Store, shared: &Shared) -> Result<()> {
    shared
        .delivery_generation
        .store(store.generation, Ordering::Release);
    let oldest = store.oldest()?;
    let mut status = shared.status.lock().unwrap_or_else(|p| p.into_inner());
    status.outbox_id = store.state.identity.clone();
    status.storage_recovered = store.storage_recovered;
    status.committed_sequence = store.state.last_sequence;
    status.pending_records = store.state.pending_records;
    status.pending_bytes = store.state.pending_bytes;
    status.dead_letter_records = store.state.dead_records;
    status.dead_letter_bytes = store.state.dead_bytes;
    status.disposition_receipts = store.state.receipt_records;
    status.disposition_bytes = store.state.receipt_bytes;
    status.retained_bytes = store.state.retained();
    status.oldest_age_ms = oldest.map(|(_, timestamp)| now_ms().saturating_sub(timestamp));
    status.last_delivery_error = store.last_error.clone();
    status.accepting = status.storage_worker_alive
        && status
            .retained_bytes
            .saturating_add(store.minimum_admission_charge())
            <= status.max_retained_bytes;
    status.ready = status.accepting
        && status.delivery_worker_alive
        && status.dead_letter_records == 0
        && store.state.stall.is_none();
    if status.delivery_state != "worker_failed" {
        status.delivery_state = if store.state.stall.is_some() {
            "stalled"
        } else if store.last_error.is_some() {
            "retrying"
        } else if store.state.pending_records > 0 {
            "pending"
        } else {
            "idle"
        }
        .into();
    }
    status.durable_commits = store.commits;
    status.last_group_size = store.last_group_size;
    Ok(())
}
fn fatal<T>(result: &Result<T>) -> bool {
    matches!(
        result,
        Err(AuditError {
            code: AuditErrorKind::Storage
                | AuditErrorKind::CommitUncertain
                | AuditErrorKind::WorkerFailed,
            ..
        })
    )
}
fn storage_loop(
    store: &mut Store,
    shared: &Shared,
    submissions: &Receiver<Submission>,
    controls: &Receiver<Control>,
) -> Result<()> {
    let mut completed_ticket = 0;
    let mut flushes: VecDeque<Flush> = VecDeque::new();
    let mut held_submission: Option<Submission> = None;
    loop {
        #[cfg(test)]
        {
            if shared.hooks.storage_exit.load(Ordering::Acquire) {
                return Err(AuditError::worker());
            }
            if shared.hooks.storage_pause.load(Ordering::Acquire) {
                shared.hooks.storage_paused.store(true, Ordering::Release);
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            shared.hooks.storage_paused.store(false, Ordering::Release);
        }
        // Under the same admission lock used by flush, capture all already queued control
        // barriers before constructing the next submission group. This prevents a group
        // from crossing a flush watermark even when control traffic is saturated.
        let control_group = {
            let _admission = shared.admission.lock().unwrap_or_else(|p| p.into_inner());
            controls
                .try_iter()
                .take(CONTROL_CAPACITY)
                .collect::<Vec<_>>()
        };
        for control in control_group {
            match control {
                Control::Wake => {}
                Control::Flush {
                    ticket,
                    deadline,
                    reply,
                } => {
                    if flushes.len() == CONTROL_CAPACITY {
                        let _ = reply.send(Err(AuditError::new(
                            AuditErrorKind::QueueOverloaded,
                            "too many outstanding flushes",
                        )));
                    } else {
                        flushes.push_back(Flush {
                            ticket,
                            watermark: (ticket <= completed_ticket)
                                .then_some(store.state.last_sequence),
                            deadline,
                            reply,
                        });
                    }
                }
                other => process_selected(store, shared, other)?,
            }
        }
        refresh(store, shared)?;
        let delivery_alive = shared
            .status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .delivery_worker_alive;
        let mut index = 0;
        while index < flushes.len() {
            let flush = &mut flushes[index];
            if flush.watermark.is_none() && flush.ticket <= completed_ticket {
                flush.watermark = Some(store.state.last_sequence);
            }
            let result = if flush.reply.is_closed() {
                Some(Ok(()))
            } else if Instant::now() >= flush.deadline {
                Some(Err(AuditError::new(
                    AuditErrorKind::Timeout,
                    "audit flush timed out",
                )))
            } else if let Some(watermark) = flush.watermark {
                match store.flush_result(watermark) {
                    Ok(true) => Some(Ok(())),
                    Ok(false) if !delivery_alive => Some(Err(AuditError::worker())),
                    Ok(false) => None,
                    Err(error) => Some(Err(error)),
                }
            } else {
                None
            };
            if let Some(result) = result {
                let flush = flushes.remove(index).unwrap();
                let _ = flush.reply.send(result);
            } else {
                index += 1;
            }
        }
        let barrier = flushes
            .iter()
            .filter(|f| f.watermark.is_none())
            .map(|f| f.ticket)
            .min()
            .unwrap_or(u64::MAX);
        let mut group = Vec::new();
        let mut bytes = 0;
        {
            let mut admission = shared.admission.lock().unwrap_or_else(|p| p.into_inner());
            // A flush enqueued since control capture is a barrier too. Defer grouping to
            // the next loop while controls are queued; lock prevents a concurrent enqueue.
            if controls.is_empty() {
                while group.len() < MAX_BATCH_EVENTS {
                    let submission = held_submission
                        .take()
                        .or_else(|| submissions.try_recv().ok());
                    let Some(submission) = submission else {
                        break;
                    };
                    if submission.ticket > barrier
                        || (!group.is_empty() && bytes + submission.bytes > MAX_BATCH_BYTES)
                    {
                        held_submission = Some(submission);
                        break;
                    }
                    bytes += submission.bytes;
                    admission.records -= 1;
                    admission.bytes -= submission.bytes;
                    group.push(submission);
                }
            }
        }
        if !group.is_empty() {
            let mut replies = Vec::new();
            let mut events = Vec::new();
            for submission in group {
                completed_ticket = submission.ticket;
                replies.push(submission.reply);
                events.push(submission.event);
            }
            #[cfg(test)]
            super::tests::WRITE_FAILURE
                .with(|value| value.set(shared.hooks.write_failure.load(Ordering::Acquire)));
            let results = store.submit_group(events);
            let failed = results.iter().any(fatal);
            refresh(store, shared)?;
            #[cfg(test)]
            while shared.hooks.hold_completions.load(Ordering::Acquire)
                && !shared
                    .admission
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .closing
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            for (reply, result) in replies.into_iter().zip(results) {
                let _ = reply.send(result);
            }
            shared.signal();
            if failed {
                return Err(AuditError::new(
                    AuditErrorKind::CommitUncertain,
                    "submission storage failed; durable outcome may be uncertain",
                ));
            }
            continue;
        }
        let admission = shared.admission.lock().unwrap_or_else(|p| p.into_inner());
        if admission.closing && admission.ticket <= completed_ticket {
            drop(admission);
            for flush in flushes {
                let _ = flush.reply.send(Err(AuditError::new(
                    AuditErrorKind::Closed,
                    "outbox closed before flush completed",
                )));
            }
            return Ok(());
        }
        drop(admission);
        // Select wakes immediately for idle submissions: no coalescing delay.
        if held_submission.is_some() {
            if let Ok(control) = controls.recv_timeout(Duration::from_millis(25)) {
                match control {
                    Control::Flush {
                        ticket,
                        deadline,
                        reply,
                    } => {
                        if flushes.len() < CONTROL_CAPACITY {
                            flushes.push_back(Flush {
                                ticket,
                                watermark: (ticket <= completed_ticket)
                                    .then_some(store.state.last_sequence),
                                deadline,
                                reply,
                            });
                        } else {
                            let _ = reply.send(Err(AuditError::new(
                                AuditErrorKind::QueueOverloaded,
                                "too many outstanding flushes",
                            )));
                        }
                    }
                    other => process_selected(store, shared, other)?,
                }
            }
            continue;
        }
        crossbeam_channel::select! {
            recv(controls) -> control => if let Ok(control) = control {
                // Keep selected control ordered ahead of subsequent controls. A local
                // handler queue avoids trying to reinsert into a potentially full channel.
                match control {
                    Control::Flush { ticket, deadline, reply } => {
                        if flushes.len() < CONTROL_CAPACITY { flushes.push_back(Flush { ticket, watermark: (ticket <= completed_ticket).then_some(store.state.last_sequence), deadline, reply }); }
                        else { let _ = reply.send(Err(AuditError::new(AuditErrorKind::QueueOverloaded, "too many outstanding flushes"))); }
                    }
                    other => process_selected(store, shared, other)?,
                }
            },
            recv(submissions) -> submission => if let Ok(submission) = submission { held_submission = Some(submission); },
            default(Duration::from_millis(25)) => {},
        }
    }
}
fn process_selected(store: &mut Store, shared: &Shared, control: Control) -> Result<()> {
    macro_rules! reply {
        ($sender:expr, $operation:expr) => {{
            let result = $operation;
            let error = result.as_ref().err().filter(|_| fatal(&result)).cloned();
            refresh(store, shared)?;
            let _ = $sender.send(result);
            if let Some(error) = error {
                return Err(error);
            }
        }};
    }
    match control {
        Control::Wake => {}
        Control::Fetch { reply: sender } => reply!(sender, store.batch()),
        Control::Ack {
            generation,
            ids,
            reply: sender,
        } => reply!(sender, store.acknowledge(generation, ids)),
        Control::Error {
            generation,
            error,
            isolate_until,
            stall,
            reply: sender,
        } => reply!(
            sender,
            store.delivery_error(generation, error, isolate_until, stall)
        ),
        Control::Reconfigure {
            destination,
            reply: sender,
        } => reply!(sender, store.reconfigure(destination)),
        Control::Quarantine {
            event_id,
            reason,
            operator_reference,
            reply: sender,
        } => reply!(
            sender,
            store.quarantine(event_id, reason, operator_reference)
        ),
        Control::Export {
            event_id,
            reply: sender,
        } => reply!(sender, store.export(event_id)),
        Control::Replay {
            event_id,
            reply: sender,
        } => reply!(sender, store.replay(event_id)),
        Control::Flush { .. } => unreachable!(),
    }
    shared.signal();
    Ok(())
}

const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const RETRY_MAX: Duration = Duration::from_secs(30);
pub(crate) fn retry_after(value: &str, now: std::time::SystemTime) -> Option<Duration> {
    if value.len() > 128 {
        return None;
    }
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds.min(RETRY_MAX.as_secs())));
    }
    httpdate::parse_http_date(value)
        .ok()
        .map(|date| date.duration_since(now).unwrap_or_default().min(RETRY_MAX))
}
fn ask<T>(shared: &Shared, command: impl FnOnce(Sender<Result<T>>) -> Control) -> Result<T> {
    let (reply, response) = crossbeam_channel::bounded(1);
    let mut control = command(reply);
    loop {
        if shared.stop_delivery.load(Ordering::Acquire) {
            return Err(AuditError::new(
                AuditErrorKind::Closed,
                "delivery is stopping",
            ));
        }
        match shared
            .controls
            .send_timeout(control, Duration::from_millis(100))
        {
            Ok(()) => break,
            Err(crossbeam_channel::SendTimeoutError::Timeout(returned)) => control = returned,
            Err(_) => return Err(AuditError::worker()),
        }
    }
    loop {
        match response.recv_timeout(Duration::from_millis(100)) {
            Ok(result) => return result,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                return Err(AuditError::worker())
            }
            Err(_) if shared.stop_delivery.load(Ordering::Acquire) => {
                return Err(AuditError::new(
                    AuditErrorKind::Closed,
                    "delivery is stopping",
                ))
            }
            Err(_) => {}
        }
    }
}
fn wait(shared: &Shared, duration: Duration) {
    let guard = shared.wake_lock.lock().unwrap_or_else(|p| p.into_inner());
    if !shared.stop_delivery.load(Ordering::Acquire) {
        let _ = shared.wake.wait_timeout(guard, duration);
    }
}
fn wait_retry(shared: &Shared, duration: Duration, generation: u64) {
    let deadline = Instant::now() + duration;
    let mut guard = shared.wake_lock.lock().unwrap_or_else(|p| p.into_inner());
    while !shared.stop_delivery.load(Ordering::Acquire)
        && shared.delivery_generation.load(Ordering::Acquire) == generation
    {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        guard = shared
            .wake
            .wait_timeout(guard, remaining)
            .unwrap_or_else(|p| p.into_inner())
            .0;
    }
}
#[derive(Debug)]
pub(crate) struct Classification {
    pub kind: DeliveryErrorKind,
    pub isolate: bool,
    pub stall: bool,
    pub immediate: bool,
}
pub(crate) fn classify(
    status: u16,
    valid_ack: bool,
    integrity: bool,
    singleton: bool,
) -> Option<Classification> {
    use DeliveryErrorKind::*;
    if (200..300).contains(&status) && valid_ack {
        return None;
    }
    let (kind, isolate, stall, immediate) = match status {
        200..=299 => (Protocol, false, false, false),
        401 | 403 => (Credentials, false, false, false),
        408 | 425 | 429 | 500..=599 => (Transport, false, false, false),
        413 | 422 => (Rejected, !singleton, singleton, singleton),
        409 if integrity => (Integrity, !singleton, singleton, true),
        _ => (Configuration, false, true, true),
    };
    Some(Classification {
        kind,
        isolate,
        stall,
        immediate,
    })
}

fn delivery_loop(shared: &Shared, started: Sender<Result<()>>) -> Result<()> {
    // Explicit provider avoids depending on whichever features the application happens
    // to unify into rustls. This client uses the workspace's ring implementation.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = rustls_platform_verifier::Verifier::new(provider.clone())
        .map_err(|_| AuditError::worker())?;
    let tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|_| AuditError::worker())?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(HTTP_TIMEOUT)
        .connect_timeout(Duration::from_secs(5))
        .no_proxy()
        .tls_backend_preconfigured(tls)
        .build()
        .map_err(|_| AuditError::worker())?;
    {
        let mut status = shared.status.lock().unwrap_or_else(|p| p.into_inner());
        status.delivery_worker_alive = true;
        status.ready = status.accepting;
    }
    let _ = started.send(Ok(()));
    let mut attempt: u32 = 0;
    while !shared.stop_delivery.load(Ordering::Acquire) {
        #[cfg(test)]
        if shared.hooks.delivery_exit.load(Ordering::Acquire) {
            return Err(AuditError::worker());
        }
        let batch = match ask(shared, |reply| Control::Fetch { reply }) {
            Ok(Some(batch)) => batch,
            Ok(None) => {
                wait(shared, Duration::from_millis(250));
                continue;
            }
            Err(error) if error.kind() == AuditErrorKind::Closed => return Ok(()),
            Err(error) => return Err(error),
        };
        let id = batch_id(batch.events.iter().map(|event| event.id.as_str()));
        let request = BatchRequest {
            version: 1,
            batch_id: id.clone(),
            events: batch
                .events
                .iter()
                .map(|event| WireEvent {
                    event_id: event.id.to_string(),
                    envelope_base64: STANDARD.encode(&event.bytes),
                })
                .collect(),
        };
        let body = bounded_json(&request, MAX_WIRE_BYTES)?;
        let mut builder = client
            .post(batch.destination.validated_url()?)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body);
        if let Some(authorization) = &batch.destination.authorization {
            builder = builder.header(reqwest::header::AUTHORIZATION, authorization);
        }
        let mut retry_after = None;
        let classification = match builder.send() {
            Ok(mut response) => {
                let status = response.status().as_u16();
                retry_after = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| self::retry_after(v, std::time::SystemTime::now()));
                let mut bytes = Vec::with_capacity(MAX_RESPONSE_BYTES.min(512));
                let bounded_read = response
                    .by_ref()
                    .take((MAX_RESPONSE_BYTES + 1) as u64)
                    .read_to_end(&mut bytes)
                    .is_ok()
                    && bytes.len() <= MAX_RESPONSE_BYTES;
                let valid_ack = bounded_read
                    && serde_json::from_slice::<DurableAck>(&bytes)
                        .is_ok_and(|ack| ack.version == 1 && ack.durable && ack.batch_id == id);
                let integrity = bounded_read
                    && serde_json::from_slice::<IntegrityResponse>(&bytes).is_ok_and(|error| {
                        error.version == 1 && error.error == "integrity_conflict"
                    });
                classify(status, valid_ack, integrity, batch.singleton)
                    .map(|class| (Some(status), class))
            }
            Err(_) => Some((
                None,
                Classification {
                    kind: DeliveryErrorKind::Transport,
                    isolate: false,
                    stall: false,
                    immediate: false,
                },
            )),
        };
        if let Some((status, classification)) = classification {
            let error = DeliveryError {
                kind: classification.kind.clone(),
                http_status: status,
                sequence: batch.events[0].sequence,
                immediate_flush_failure: classification.immediate,
                message: match classification.kind {
                    DeliveryErrorKind::Transport => "receiver transport/retry condition",
                    DeliveryErrorKind::Protocol => {
                        "receiver did not return an exact durable batch acknowledgement"
                    }
                    DeliveryErrorKind::Credentials => {
                        "receiver rejected credentials or authorization"
                    }
                    DeliveryErrorKind::Rejected => "receiver rejected record size or schema",
                    DeliveryErrorKind::Integrity => {
                        "receiver reported an ID/content integrity conflict"
                    }
                    DeliveryErrorKind::Configuration => {
                        "receiver configuration/protocol requires explicit repair"
                    }
                }
                .into(),
            };
            let until = classification
                .isolate
                .then(|| batch.events.last().unwrap().sequence);
            match ask(shared, |reply| Control::Error {
                generation: batch.generation,
                error,
                isolate_until: until,
                stall: classification.stall,
                reply,
            }) {
                Ok(()) => {}
                Err(error) if error.kind() == AuditErrorKind::Closed => return Ok(()),
                Err(error) => return Err(error),
            }
            if classification.isolate {
                attempt = 0;
                continue;
            }
            attempt = attempt.saturating_add(1);
            let cap_ms = 100u64
                .saturating_mul(1u64 << attempt.min(9))
                .min(RETRY_MAX.as_millis() as u64);
            let mut random = [0; 8];
            getrandom::getrandom(&mut random).map_err(AuditError::storage)?;
            let delay =
                Duration::from_millis(cap_ms / 2 + u64::from_le_bytes(random) % (cap_ms / 2 + 1));
            wait_retry(shared, retry_after.unwrap_or(delay), batch.generation);
        } else {
            let ids = batch.events.into_iter().map(|event| event.id).collect();
            match ask(shared, |reply| Control::Ack {
                generation: batch.generation,
                ids,
                reply,
            }) {
                Ok(()) => attempt = 0,
                Err(error) if error.kind() == AuditErrorKind::StaleDelivery => attempt = 0,
                Err(error) if error.kind() == AuditErrorKind::Closed => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}
