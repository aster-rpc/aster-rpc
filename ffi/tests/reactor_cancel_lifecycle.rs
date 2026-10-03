//! Live FFI cancellation lifecycle coverage.
//!
//! These tests deliberately observe the production `ReactorState` maps. The
//! earlier `cancel_state_cleanup` tests modelled the maps and therefore missed
//! both bugs this file guards: arming the watcher before insertion, and omitting
//! it from the blocking-poll drain path.

use std::ptr;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use aster_transport_core::framing::{encode_frame, FLAG_CANCEL, FLAG_END_STREAM, FLAG_HEADER};
use aster_transport_core::reactor::RPC_ALPN;
use aster_transport_ffi::reactor::*;
use aster_transport_ffi::*;

mod common;

use common::{wait_for_operation, wait_for_successful_operation};

struct Rig {
    runtime: aster_runtime_t,
    reactor: aster_reactor_t,
    connection: aster_connection_t,
}

impl Rig {
    unsafe fn start() -> Self {
        let mut runtime = 0;
        assert_eq!(
            aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime),
            aster_status_t::ASTER_STATUS_OK as i32
        );

        let server = create_node(runtime);
        let client = create_node(runtime);

        let mut reactor = 0;
        assert_eq!(
            aster_reactor_create(runtime, server, 64, &mut reactor),
            aster_status_t::ASTER_STATUS_OK as i32
        );

        let mut server_id = [0u8; 128];
        let mut server_id_len = 0;
        assert_eq!(
            aster_node_id(
                runtime,
                server,
                server_id.as_mut_ptr(),
                server_id.len(),
                &mut server_id_len,
            ),
            aster_status_t::ASTER_STATUS_OK as i32
        );

        let config = aster_connect_config_t {
            struct_size: std::mem::size_of::<aster_connect_config_t>() as u32,
            flags: 0,
            node_id: bytes(&server_id[..server_id_len]),
            alpn: bytes(RPC_ALPN),
            addr: ptr::null(),
        };
        let mut connect_op = 0;
        assert_eq!(
            aster_connect(runtime, client, &config, 0, &mut connect_op),
            aster_status_t::ASTER_STATUS_OK as i32
        );
        let connected = wait_for_successful_operation(
            runtime,
            connect_op,
            aster_event_kind_t::ASTER_EVENT_CONNECTED,
        );

        assert_eq!(debug_call_state_len(reactor), Some((0, 0, 0)));
        Self {
            runtime,
            reactor,
            connection: connected.handle,
        }
    }

    unsafe fn open_stream(&self) -> (aster_send_stream_t, aster_recv_stream_t) {
        let mut operation = 0;
        assert_eq!(
            aster_open_bi(self.runtime, self.connection, 0, &mut operation),
            aster_status_t::ASTER_STATUS_OK as i32
        );
        let opened = wait_for_successful_operation(
            self.runtime,
            operation,
            aster_event_kind_t::ASTER_EVENT_STREAM_OPENED,
        );
        (opened.handle, opened.related)
    }

    unsafe fn write(&self, stream: aster_send_stream_t, payload: &[u8]) {
        let mut operation = 0;
        assert_eq!(
            aster_stream_write(self.runtime, stream, bytes(payload), 0, &mut operation,),
            aster_status_t::ASTER_STATUS_OK as i32
        );
        wait_for_successful_operation(
            self.runtime,
            operation,
            aster_event_kind_t::ASTER_EVENT_SEND_COMPLETED,
        );
    }

    unsafe fn send_call(&self, stream: aster_send_stream_t) {
        let mut request = encode_frame(b"ffi-live-header", FLAG_HEADER).expect("header frame");
        request.extend_from_slice(
            &encode_frame(b"ffi-live-request", FLAG_END_STREAM).expect("request frame"),
        );
        self.write(stream, &request);
    }

    unsafe fn cancel_and_wait_for_reset(
        &self,
        send_stream: aster_send_stream_t,
        recv_stream: aster_recv_stream_t,
    ) {
        let cancel = encode_frame(&[], FLAG_CANCEL).expect("cancel frame");
        self.write(send_stream, &cancel);

        // Starting the read after the cancel write completes avoids racing two
        // completion-queue operations. The stream is still live as a handle,
        // and the read blocks until core observes FLAG_CANCEL and resets it.
        let mut read_op = 0;
        assert_eq!(
            aster_stream_read(self.runtime, recv_stream, 1, 0, &mut read_op),
            aster_status_t::ASTER_STATUS_OK as i32
        );
        let reset = wait_for_operation(self.runtime, read_op);
        assert_eq!(
            reset.kind,
            aster_event_kind_t::ASTER_EVENT_ERROR as u32,
            "cancelled response direction should reset, got event kind {} status {}",
            reset.kind,
            reset.status
        );
        if reset.buffer != 0 {
            assert_eq!(
                aster_buffer_release(self.runtime, reset.buffer),
                aster_status_t::ASTER_STATUS_OK as i32
            );
        }
    }

    unsafe fn poll_one(&self, timeout_ms: u32) -> u64 {
        let mut call: aster_reactor_call_t = std::mem::zeroed();
        let count = aster_reactor_poll(self.runtime, self.reactor, &mut call, 1, timeout_ms);
        assert_eq!(count, 1, "expected exactly one live reactor call");
        let call_id = call.call_id;
        self.release_call_buffers(&call);
        call_id
    }

    unsafe fn release_call_buffers(&self, call: &aster_reactor_call_t) {
        for buffer in [call.header_buffer, call.request_buffer, call.peer_buffer] {
            if buffer != 0 {
                assert_eq!(
                    aster_reactor_buffer_release(self.runtime, self.reactor, buffer),
                    aster_status_t::ASTER_STATUS_OK as i32
                );
            }
        }
    }

    unsafe fn close(self) {
        assert_eq!(
            aster_reactor_destroy(self.runtime, self.reactor),
            aster_status_t::ASTER_STATUS_OK as i32
        );
        assert_eq!(
            aster_runtime_close(self.runtime),
            aster_status_t::ASTER_STATUS_OK as i32
        );
    }
}

unsafe fn create_node(runtime: aster_runtime_t) -> aster_node_t {
    let alpns = [RPC_ALPN.as_ptr()];
    let alpn_lens = [RPC_ALPN.len()];
    let mut operation = 0;
    assert_eq!(
        aster_node_memory_with_alpns(
            runtime,
            alpns.as_ptr(),
            alpn_lens.as_ptr(),
            alpns.len(),
            0,
            &mut operation,
        ),
        aster_status_t::ASTER_STATUS_OK as i32
    );
    wait_for_successful_operation(
        runtime,
        operation,
        aster_event_kind_t::ASTER_EVENT_NODE_CREATED,
    )
    .handle
}

fn bytes(value: &[u8]) -> aster_bytes_t {
    aster_bytes_t {
        ptr: value.as_ptr(),
        len: value.len(),
    }
}

fn wait_for_call_state(reactor: aster_reactor_t, expected: (usize, usize, usize)) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let actual = debug_call_state_len(reactor).expect("reactor exists");
        if actual == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "call-state maps did not reach {expected:?}; last value {actual:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_for_blocking_poll(reactor: aster_reactor_t) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while debug_blocking_poll_entries(reactor).expect("reactor exists") == 0 {
        assert!(
            Instant::now() < deadline,
            "poll thread never entered the wait-after-initial-drain path"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn cancellation_before_poll_reaps_the_real_maps_without_a_submit() {
    unsafe {
        let rig = Rig::start();
        let (send, recv) = rig.open_stream();
        rig.send_call(send);

        // Core closes the response receiver before the FFI has ever polled the
        // call. `install_call_state` must insert all maps and only then arm a
        // watcher that observes the already-closed channel.
        rig.cancel_and_wait_for_reset(send, recv);
        let call_id = rig.poll_one(10_000);

        wait_for_call_state(rig.reactor, (0, 0, 0));
        assert_eq!(
            aster_reactor_check_cancelled(rig.runtime, rig.reactor, call_id),
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
            "the cancelled call should be reaped without any submit"
        );
        rig.close();
    }
}

#[test]
fn cancellation_after_arrival_in_blocking_poll_reaps_the_real_maps() {
    unsafe {
        let rig = Rig::start();
        let (send, recv) = rig.open_stream();

        let runtime = rig.runtime;
        let reactor = rig.reactor;
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let poller = std::thread::spawn(move || {
            let mut call: aster_reactor_call_t = std::mem::zeroed();
            let count = aster_reactor_poll(runtime, reactor, &mut call, 1, 10_000);
            let result = (
                count,
                call.call_id,
                call.header_buffer,
                call.request_buffer,
                call.peer_buffer,
            );
            result_tx.send(result).expect("test receiver alive");
        });

        // This instrumentation makes the branch selection an assertion rather
        // than a scheduling guess: the initial drain was empty, and the call
        // below must arrive through the second drain.
        wait_for_blocking_poll(rig.reactor);
        rig.send_call(send);

        let (count, call_id, header_buffer, request_buffer, peer_buffer) = result_rx
            .recv_timeout(Duration::from_secs(15))
            .expect("blocking poll should receive the call");
        poller.join().expect("poll thread");
        assert_eq!(count, 1);
        for buffer in [header_buffer, request_buffer, peer_buffer] {
            if buffer != 0 {
                assert_eq!(
                    aster_reactor_buffer_release(rig.runtime, rig.reactor, buffer),
                    aster_status_t::ASTER_STATUS_OK as i32
                );
            }
        }

        // No cancel yet, so all three production entries must be present. This
        // also proves the test observed installation before testing cleanup.
        assert_eq!(debug_call_state_len(rig.reactor), Some((1, 1, 1)));
        rig.cancel_and_wait_for_reset(send, recv);

        wait_for_call_state(rig.reactor, (0, 0, 0));
        assert_eq!(
            aster_reactor_check_cancelled(rig.runtime, rig.reactor, call_id),
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );
        rig.close();
    }
}
