//go:build cgo

package aster

import (
	"context"
	"encoding/binary"
	"fmt"
	"log"
	"sync"
	"sync/atomic"
)

const (
	// AsterALPN is the default ALPN for Aster RPC.
	AsterALPN = "aster/1"

	// Aster wire framing flags.
	FlagCompressed = 0x01
	FlagTrailer    = 0x02
	FlagHeader     = 0x04
	FlagRowSchema  = 0x08
	FlagCall       = 0x10
	FlagCancel     = 0x20
)

// CallHandler is the function signature for handling incoming RPC calls.
// It receives the call and returns a response.
type CallHandler func(call ReactorCall) ReactorResponse

// ServerConfig configures an AsterServer.
type ServerConfig struct {
	// Handler is the function that processes incoming calls (required).
	Handler CallHandler
	// RingCapacity is the SPSC ring buffer size (default 256).
	RingCapacity uint32
	// ExtraALPNs are additional ALPNs beyond "aster/1".
	ExtraALPNs []string
	// PollBatchSize is the max calls to drain per poll (default 32).
	PollBatchSize int
	// OnConnectionClosed, if set, is invoked on the poll goroutine for each
	// ASTER_REACTOR_EVENT_CONNECTION_CLOSED so per-connection state can be
	// dropped. Must not block.
	OnConnectionClosed func(connectionID uint64, peerID string)
	// OnSubmitError, if set, receives errors from submitting a handler's
	// response (e.g. the call's response channel closed while waiting for
	// capacity). Defaults to logging via the standard logger.
	OnSubmitError func(call ReactorCall, err error)
}

// Server is a high-level Aster RPC server.
// It creates a node, attaches a reactor, and runs a poll loop
// that dispatches incoming calls to the configured handler.
type Server struct {
	node        *Node
	reactor     *Reactor
	handler     CallHandler
	onClosed    func(connectionID uint64, peerID string)
	onSubmitErr func(call ReactorCall, err error)
	ctx         context.Context
	cancel      context.CancelFunc
	calls       sync.WaitGroup
	running     atomic.Bool
	wg          sync.WaitGroup

	pollBatch int
}

// NewServer creates and starts an AsterServer.
// The server is ready to accept calls when this function returns.
func NewServer(ctx context.Context, cfg ServerConfig) (*Server, error) {
	if cfg.Handler == nil {
		return nil, fmt.Errorf("handler is required")
	}
	if cfg.RingCapacity == 0 {
		cfg.RingCapacity = 256
	}
	if cfg.PollBatchSize <= 0 {
		cfg.PollBatchSize = 32
	}

	alpns := []string{AsterALPN}
	for _, a := range cfg.ExtraALPNs {
		if a != AsterALPN {
			alpns = append(alpns, a)
		}
	}

	node, err := MemoryWithAlpns(ctx, alpns)
	if err != nil {
		return nil, fmt.Errorf("create node: %w", err)
	}

	reactor, err := NewReactor(node.runtime, node, cfg.RingCapacity)
	if err != nil {
		node.Close()
		return nil, fmt.Errorf("create reactor: %w", err)
	}

	onSubmitErr := cfg.OnSubmitError
	if onSubmitErr == nil {
		onSubmitErr = func(call ReactorCall, err error) {
			log.Printf("aster: submit response for call %d failed: %v", call.CallID, err)
		}
	}
	sctx, cancel := context.WithCancel(context.Background())
	s := &Server{
		node:        node,
		reactor:     reactor,
		handler:     cfg.Handler,
		onClosed:    cfg.OnConnectionClosed,
		onSubmitErr: onSubmitErr,
		ctx:         sctx,
		cancel:      cancel,
		pollBatch:   cfg.PollBatchSize,
	}
	s.running.Store(true)

	s.wg.Add(1)
	go s.pollLoop()

	return s, nil
}

// NodeID returns the server's node ID as a hex string.
func (s *Server) NodeID() (string, error) {
	return s.node.NodeID()
}

// Node returns the underlying Iroh node.
func (s *Server) Node() *Node {
	return s.node
}

// Close stops the server, reactor, and node.
func (s *Server) Close() error {
	if !s.running.Swap(false) {
		return nil
	}
	// Abort submits blocked on response capacity (the poll loop that would
	// wake them is stopping), then wait for the poll loop and handlers.
	s.cancel()
	s.wg.Wait()
	s.calls.Wait()

	var firstErr error
	if err := s.reactor.Close(); err != nil && firstErr == nil {
		firstErr = err
	}
	if err := s.node.Close(); err != nil && firstErr == nil {
		firstErr = err
	}
	return firstErr
}

func (s *Server) pollLoop() {
	defer s.wg.Done()

	for s.running.Load() {
		// Poll also delivers RESPONSE_WRITABLE/RESPONSE_CLOSED events to
		// handler goroutines blocked in SubmitContext on QUEUE_FULL.
		events, err := s.reactor.Poll(s.pollBatch, 100)
		if err != nil {
			continue
		}

		for _, ev := range events {
			switch ev.Kind {
			case ReactorEventConnectionClosed:
				if s.onClosed != nil {
					s.onClosed(ev.ConnectionID, ev.PeerID)
				}
			case ReactorEventCall:
				c := ev
				s.calls.Add(1)
				go func() {
					defer s.calls.Done()
					resp := s.handler(c)
					if err := s.reactor.SubmitContext(s.ctx, c.CallID, resp); err != nil {
						s.onSubmitErr(c, err)
					}
				}()
			}
		}
	}
}

// EncodeFrame encodes payload with flags into the Aster wire format:
// [4-byte LE frame_body_len][1-byte flags][payload]
func EncodeFrame(payload []byte, flags byte) []byte {
	frameBodyLen := uint32(1 + len(payload))
	frame := make([]byte, 4+frameBodyLen)
	binary.LittleEndian.PutUint32(frame[0:4], frameBodyLen)
	frame[4] = flags
	copy(frame[5:], payload)
	return frame
}
