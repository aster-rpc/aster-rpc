//go:build cgo

package aster

/*
#include <stdint.h>
#include <stddef.h>
#include <string.h>

#include "aster_ffi.h"
*/
import "C"

import (
	"context"
	"fmt"
	"unsafe"
)

// Connection represents an established connection to a remote node.
// Use Endpoint.Connect or Node.Accept to create one.
type Connection struct {
	handle  uint64
	runtime *Runtime
}

// OpenBi opens a bidirectional stream on this connection.
// Returns send and receive streams as a pair.
func (c *Connection) OpenBi(ctx context.Context) (*SendStream, *RecvStream, error) {
	var opID C.aster_operation_t
	r := C.aster_open_bi(
		C.uint64_t(c.runtime.handle),
		C.uint64_t(c.handle),
		0,
		&opID,
	)
	if r != 0 {
		return nil, nil, fmt.Errorf("aster_open_bi: %w", Error(r))
	}

	ev, err := c.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return nil, nil, fmt.Errorf("open bi poll: %w", err)
	}
	if ev.Kind != ASTER_EVENT_STREAM_OPENED {
		return nil, nil, fmt.Errorf("open bi: unexpected event %d", ev.Kind)
	}

	send := &SendStream{handle: ev.Handle, runtime: c.runtime, isSender: true}
	recv := &RecvStream{handle: ev.Related, runtime: c.runtime}
	return send, recv, nil
}

// AcceptBi accepts a bidirectional stream offered by the remote peer.
func (c *Connection) AcceptBi(ctx context.Context) (*SendStream, *RecvStream, error) {
	var opID C.aster_operation_t
	r := C.aster_accept_bi(
		C.uint64_t(c.runtime.handle),
		C.uint64_t(c.handle),
		0,
		&opID,
	)
	if r != 0 {
		return nil, nil, fmt.Errorf("aster_accept_bi: %w", Error(r))
	}

	ev, err := c.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return nil, nil, fmt.Errorf("accept bi poll: %w", err)
	}
	if ev.Kind != ASTER_EVENT_STREAM_ACCEPTED {
		return nil, nil, fmt.Errorf("accept bi: unexpected event %d", ev.Kind)
	}

	send := &SendStream{handle: ev.Handle, runtime: c.runtime, isSender: true}
	recv := &RecvStream{handle: ev.Related, runtime: c.runtime}
	return send, recv, nil
}

// OpenUni opens a unidirectional send stream on this connection.
// Returns only a SendStream; the remote end receives via AcceptUni.
func (c *Connection) OpenUni(ctx context.Context) (*SendStream, error) {
	var opID C.aster_operation_t
	r := C.aster_open_uni(
		C.uint64_t(c.runtime.handle),
		C.uint64_t(c.handle),
		0,
		&opID,
	)
	if r != 0 {
		return nil, fmt.Errorf("aster_open_uni: %w", Error(r))
	}

	ev, err := c.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return nil, fmt.Errorf("open uni poll: %w", err)
	}
	if ev.Kind != ASTER_EVENT_STREAM_OPENED {
		return nil, fmt.Errorf("open uni: unexpected event %d", ev.Kind)
	}

	return &SendStream{handle: ev.Handle, runtime: c.runtime, isSender: true}, nil
}

// AcceptUni accepts a unidirectional stream offered by the remote peer.
// Returns a RecvStream for reading.
func (c *Connection) AcceptUni(ctx context.Context) (*RecvStream, error) {
	var opID C.aster_operation_t
	r := C.aster_accept_uni(
		C.uint64_t(c.runtime.handle),
		C.uint64_t(c.handle),
		0,
		&opID,
	)
	if r != 0 {
		return nil, fmt.Errorf("aster_accept_uni: %w", Error(r))
	}

	ev, err := c.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return nil, fmt.Errorf("accept uni poll: %w", err)
	}
	if ev.Kind != ASTER_EVENT_STREAM_ACCEPTED {
		return nil, fmt.Errorf("accept uni: unexpected event %d", ev.Kind)
	}

	return &RecvStream{handle: ev.Handle, runtime: c.runtime}, nil
}

// SendDatagram sends a datagram on this connection.
func (c *Connection) SendDatagram(data []byte) error {
	bytes := C.struct_aster_bytes_t{
		ptr: (*C.uint8_t)(unsafe.Pointer(&data[0])),
		len: C.uintptr_t(len(data)),
	}
	r := C.aster_connection_send_datagram(
		C.uint64_t(c.runtime.handle),
		C.uint64_t(c.handle),
		bytes,
	)
	if r != 0 {
		return fmt.Errorf("aster_connection_send_datagram: %w", Error(r))
	}
	return nil
}

// ReadDatagram reads a datagram on this connection.
// Returns the datagram bytes.
func (c *Connection) ReadDatagram(ctx context.Context) ([]byte, error) {
	var opID C.aster_operation_t
	r := C.aster_connection_read_datagram(
		C.uint64_t(c.runtime.handle),
		C.uint64_t(c.handle),
		0,
		&opID,
	)
	if r != 0 {
		return nil, fmt.Errorf("aster_connection_read_datagram: %w", Error(r))
	}

	ev, err := c.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return nil, fmt.Errorf("read datagram poll: %w", err)
	}
	if ev.Kind != ASTER_EVENT_BYTES_RESULT {
		return nil, fmt.Errorf("read datagram: unexpected event %d", ev.Kind)
	}

	data := cDataToGoBytes(ev.DataPtr, ev.DataLen)

	if ev.Buffer != 0 {
		c.runtime.ReleaseBuffer(ev.Buffer)
	}

	return data, nil
}

// Close closes this connection with an optional reason string.
func (c *Connection) Close(reason string) error {
	var bytes C.struct_aster_bytes_t
	if len(reason) > 0 {
		reasonBytes := []byte(reason)
		cBuf := C.malloc(C.size_t(len(reasonBytes)))
		C.memcpy(cBuf, unsafe.Pointer(&reasonBytes[0]), C.size_t(len(reasonBytes)))
		bytes = C.struct_aster_bytes_t{
			ptr: (*C.uint8_t)(cBuf),
			len: C.uintptr_t(len(reasonBytes)),
		}
	}
	r := C.aster_connection_close(
		C.uint64_t(c.runtime.handle),
		C.uint64_t(c.handle),
		0,
		bytes,
	)
	if r != 0 {
		return fmt.Errorf("aster_connection_close: %w", Error(r))
	}
	return nil
}

// ConnectionInfo holds metadata about a connection.
type ConnectionInfo struct {
	ConnectionType uint32
	BytesSent      uint64
	BytesReceived  uint64
	RTTNs          uint64
	ALPN           string
	IsConnected    bool
}

// Info returns metadata about this connection.
func (c *Connection) Info() (*ConnectionInfo, error) {
	var info C.aster_connection_info_t
	info.struct_size = C.uint32_t(unsafe.Sizeof(info))

	r := C.aster_connection_info(
		C.uint64_t(c.runtime.handle),
		C.uint64_t(c.handle),
		&info,
	)
	if r != 0 {
		return nil, fmt.Errorf("aster_connection_info: %w", Error(r))
	}

	ci := &ConnectionInfo{
		ConnectionType: uint32(info.connection_type),
		BytesSent:      uint64(info.bytes_sent),
		BytesReceived:  uint64(info.bytes_received),
		RTTNs:          uint64(info.rtt_ns),
		ALPN:           string(takeRustBytes(info.alpn)), // Rust-owned
		IsConnected:    info.is_connected != 0,
	}
	return ci, nil
}

// MaxDatagramSize returns the maximum size of datagrams on this connection.
func (c *Connection) MaxDatagramSize() (uint64, bool, error) {
	var size C.uint64_t
	var isSome C.uint32_t
	r := C.aster_connection_max_datagram_size(
		C.uint64_t(c.runtime.handle),
		C.uint64_t(c.handle),
		&size,
		&isSome,
	)
	if r != 0 {
		return 0, false, fmt.Errorf("aster_connection_max_datagram_size: %w", Error(r))
	}
	return uint64(size), isSome != 0, nil
}

// RemoteID returns the node ID of the remote peer as a hex string.
func (c *Connection) RemoteID() (string, error) {
	var buf [64]byte
	var len C.uintptr_t
	r := C.aster_connection_remote_id(
		C.uint64_t(c.runtime.handle),
		C.uint64_t(c.handle),
		(*C.uint8_t)(unsafe.Pointer(&buf[0])),
		64,
		&len,
	)
	if r != 0 {
		return "", fmt.Errorf("aster_connection_remote_id: %w", Error(r))
	}
	return string(buf[:int(len)]), nil // already hex text
}

// OnClosed returns a channel that receives when the connection is closed.
func (c *Connection) OnClosed(ctx context.Context) error {
	var opID C.aster_operation_t
	r := C.aster_connection_closed(
		C.uint64_t(c.runtime.handle),
		C.uint64_t(c.handle),
		0,
		&opID,
	)
	if r != 0 {
		return fmt.Errorf("aster_connection_closed: %w", Error(r))
	}

	_, err := c.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return fmt.Errorf("on closed poll: %w", err)
	}
	return nil
}

// DatagramBufferSpace returns the available send buffer space for datagrams.
func (c *Connection) DatagramBufferSpace() (uint64, error) {
	var bytes C.uint64_t
	r := C.aster_connection_datagram_send_buffer_space(
		C.uint64_t(c.runtime.handle),
		C.uint64_t(c.handle),
		&bytes,
	)
	if r != 0 {
		return 0, fmt.Errorf("aster_connection_datagram_send_buffer_space: %w", Error(r))
	}
	return uint64(bytes), nil
}
