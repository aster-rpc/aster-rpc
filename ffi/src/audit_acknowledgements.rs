//! Audit outbox acknowledgement log (ABI 7.1, additive). Each record the
//! receiver acknowledged is logged in the transaction that deletes it; this
//! reads that log by cursor. See `ffi_spec/Aster-audit-outbox-spec.md`.

use std::time::Duration;

use aster::audit_outbox::MAX_ACK_PAGE;
use serde::Deserialize;

use crate::audit_outbox::aster_audit_outbox_t;
use crate::audit_outbox::{error, outbox_for, output, request, version};
use crate::data::{bridge_for, invalid, spawn_op_coded, Done};
use crate::pure::try_status;
use crate::{aster_bytes_t, aster_event_kind_t, aster_operation_t, aster_runtime_t};

/// Largest `wait_ms` an acknowledgement query accepts (one hour).
pub const ASTER_AUDIT_OUTBOX_MAX_ACK_WAIT_MS: u64 = 3_600_000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AcknowledgementsRequest {
    version: u32,
    #[serde(default)]
    after: u64,
    limit: usize,
    #[serde(default)]
    wait_ms: u64,
}

/// Read the acknowledgement log, JSON v1
/// `{ "version":1, "after":<cursor>, "limit":1..=1000, "wait_ms":<ms> }`.
/// `after` (default 0) is exclusive: pass the last returned `cursor` to continue.
/// With `wait_ms` > 0 and nothing after `after`, completion waits up to that
/// long for the next acknowledgement, then returns an empty page. Emits
/// `ASTER_EVENT_AUDIT_OUTBOX_ACKNOWLEDGEMENTS` with `{ "version":1,
/// "acknowledgements":[{ "cursor", "event_id", "sequence", "batch_id",
/// "acknowledged_at_ms" }] }`, oldest first. Cancelling a waiting query is safe.
#[no_mangle]
pub unsafe extern "C" fn aster_audit_outbox_acknowledgements(
    runtime: aster_runtime_t,
    outbox: aster_audit_outbox_t,
    json: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("out_operation is NULL");
    }
    *out_operation = 0;
    let req: AcknowledgementsRequest = try_status!(request(
        json,
        crate::audit_outbox::ASTER_AUDIT_OUTBOX_MAX_OPERATOR_JSON
    ));
    try_status!(version(req.version));
    if req.limit == 0 || req.limit > MAX_ACK_PAGE {
        return invalid("acknowledgement limit must be between 1 and 1000");
    }
    if req.wait_ms > ASTER_AUDIT_OUTBOX_MAX_ACK_WAIT_MS {
        return invalid("acknowledgement wait_ms exceeds one hour");
    }
    let bridge = try_status!(bridge_for(runtime));
    let state = try_status!(outbox_for(&bridge, outbox));
    spawn_op_coded(&bridge, user_data, out_operation, async move {
        let page = if req.wait_ms == 0 {
            state.outbox.acknowledgements(req.after, req.limit).await
        } else {
            state
                .outbox
                .wait_acknowledgements(req.after, req.limit, Duration::from_millis(req.wait_ms))
                .await
        }
        .map_err(error)?;
        Ok(Done::ok(
            aster_event_kind_t::ASTER_EVENT_AUDIT_OUTBOX_ACKNOWLEDGEMENTS,
            outbox,
        )
        .data(output(
            serde_json::json!({"version":1,"acknowledgements":page}),
        )?))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit_outbox::{
        aster_audit_outbox_close, aster_audit_outbox_flush, aster_audit_outbox_open,
        aster_audit_outbox_submit,
    };
    use crate::audit_test_common as common;
    use crate::{
        aster_buffer_release, aster_runtime_close_and_wait, aster_runtime_config_t,
        aster_runtime_new, aster_status_t, ASTER_ABI_VERSION_MAJOR,
    };
    use aster::audit_outbox::wire::{BatchRequest, DurableAck};
    use serde_json::{json, Value};
    use std::io::{BufRead, BufReader, Read, Write};

    const OK: i32 = aster_status_t::ASTER_STATUS_OK as i32;
    const INVALID: i32 = aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;

    fn bytes(json: &[u8]) -> aster_bytes_t {
        aster_bytes_t {
            ptr: json.as_ptr(),
            len: json.len(),
        }
    }

    /// Minimal acknowledging receiver: one request per connection, durable ACK
    /// echoing the batch digest. Enough to drive deletion and the log.
    fn receiver() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/audit", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let mut reader = BufReader::new(stream);
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    let line = line.trim_end();
                    if line.is_empty() {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("content-length") {
                            length = value.trim().parse().unwrap_or(0);
                        }
                    }
                }
                let mut body = vec![0; length];
                if reader.read_exact(&mut body).is_err() {
                    continue;
                }
                let batch: BatchRequest = serde_json::from_slice(&body).unwrap();
                let ack = serde_json::to_vec(&DurableAck {
                    version: 1,
                    batch_id: batch.batch_id,
                    durable: true,
                })
                .unwrap();
                let mut stream = reader.into_inner();
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    ack.len()
                );
                let _ = stream.write_all(&ack);
            }
        });
        endpoint
    }

    unsafe fn payload(rt: u64, event: crate::aster_event_t) -> Vec<u8> {
        let data = if event.data_len == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(event.data_ptr, event.data_len).to_vec()
        };
        if event.buffer != 0 {
            assert_eq!(aster_buffer_release(rt, event.buffer), OK);
        }
        data
    }

    unsafe fn acknowledgements(rt: u64, handle: u64, request: Value) -> Vec<Value> {
        let request = serde_json::to_vec(&request).unwrap();
        let mut op = 0;
        assert_eq!(
            aster_audit_outbox_acknowledgements(rt, handle, bytes(&request), 0, &mut op),
            OK
        );
        let event = common::wait_for_successful_operation(
            rt,
            op,
            aster_event_kind_t::ASTER_EVENT_AUDIT_OUTBOX_ACKNOWLEDGEMENTS,
        );
        assert_eq!(event.handle, handle);
        let value: Value = serde_json::from_slice(&payload(rt, event)).unwrap();
        assert_eq!(value["version"], 1);
        value["acknowledgements"].as_array().unwrap().clone()
    }

    #[test]
    fn acknowledgement_log_reads_by_cursor_and_rejects_bad_requests() {
        let directory = tempfile::Builder::new()
            .prefix("aster-ffi-audit-ack-")
            .tempdir()
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        let endpoint = receiver();
        unsafe {
            let config = aster_runtime_config_t {
                struct_size: std::mem::size_of::<aster_runtime_config_t>() as u32,
                worker_threads: 1,
                event_queue_capacity: 16,
                max_inflight_operations: 16,
            };
            let mut rt = 0;
            assert_eq!(
                aster_runtime_new(ASTER_ABI_VERSION_MAJOR, &config, &mut rt),
                OK
            );
            let open = serde_json::to_vec(&json!({"version":1,"config":{
                "directory":directory.path(),"destination":{"endpoint":endpoint},
                "acknowledgement_log_max_entries":1000,
                "acknowledgement_log_max_age_ms":86_400_000u64}}))
            .unwrap();
            let mut op = 0;
            assert_eq!(aster_audit_outbox_open(rt, bytes(&open), 0, &mut op), OK);
            let handle = common::wait_for_successful_operation(
                rt,
                op,
                aster_event_kind_t::ASTER_EVENT_AUDIT_OUTBOX_OPENED,
            )
            .handle;

            // Synchronous rejection writes no operation.
            for bad in [
                json!({"version":2,"limit":10}),
                json!({"version":1,"limit":0}),
                json!({"version":1,"limit":1001}),
                json!({"version":1,"limit":10,"wait_ms":3_600_001u64}),
                json!({"version":1,"limit":10,"unknown":true}),
            ] {
                let bad = serde_json::to_vec(&bad).unwrap();
                let mut op = 7;
                assert_eq!(
                    aster_audit_outbox_acknowledgements(rt, handle, bytes(&bad), 0, &mut op),
                    INVALID
                );
                assert_eq!(op, 0);
            }
            assert!(
                acknowledgements(rt, handle, json!({"version":1,"limit":10,"wait_ms":10}))
                    .is_empty()
            );

            let mut ids = Vec::new();
            for n in 0..3 {
                let event = serde_json::to_vec(&json!({"version":1,"event":{
                    "timestamp_ms":1,"kind":"ffi.ack","payload":{"n":n}}}))
                .unwrap();
                assert_eq!(
                    aster_audit_outbox_submit(rt, handle, bytes(&event), 0, &mut op),
                    OK
                );
                let done = common::wait_for_successful_operation(
                    rt,
                    op,
                    aster_event_kind_t::ASTER_EVENT_AUDIT_OUTBOX_SUBMITTED,
                );
                ids.push(String::from_utf8(payload(rt, done)).unwrap());
            }
            assert_eq!(aster_audit_outbox_flush(rt, handle, 30_000, 0, &mut op), OK);
            common::wait_for_successful_operation(
                rt,
                op,
                aster_event_kind_t::ASTER_EVENT_UNIT_RESULT,
            );
            let all = acknowledgements(rt, handle, json!({"version":1,"limit":100}));
            assert_eq!(
                all.iter()
                    .map(|a| a["event_id"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                ids.iter().map(String::as_str).collect::<Vec<_>>()
            );
            for (index, entry) in all.iter().enumerate() {
                assert_eq!(entry["cursor"], index as u64 + 1);
                assert_eq!(entry["batch_id"].as_str().unwrap().len(), 64);
                assert!(entry["acknowledged_at_ms"].as_u64().unwrap() > 0);
            }
            let page = acknowledgements(
                rt,
                handle,
                json!({"version":1,"after":1,"limit":1,"wait_ms":1000}),
            );
            assert_eq!(page.len(), 1);
            assert_eq!(page[0]["cursor"], 2);

            assert_eq!(aster_audit_outbox_close(rt, handle, 0, &mut op), OK);
            common::wait_for_successful_operation(
                rt,
                op,
                aster_event_kind_t::ASTER_EVENT_UNIT_RESULT,
            );
            let request = serde_json::to_vec(&json!({"version":1,"limit":10})).unwrap();
            assert_eq!(
                aster_audit_outbox_acknowledgements(rt, handle, bytes(&request), 0, &mut op),
                OK
            );
            let closed = common::wait_for_operation(rt, op);
            assert_eq!(closed.kind, aster_event_kind_t::ASTER_EVENT_ERROR as u32);
            assert_eq!(
                closed.error_code,
                crate::audit_outbox::aster_audit_outbox_error_t::ASTER_AUDIT_OUTBOX_ERROR_CLOSED
                    as i32
            );
            payload(rt, closed);
            assert_eq!(
                aster_runtime_close_and_wait(rt, Duration::from_secs(10)),
                OK
            );
        }
    }
}
