//go:build cgo

package aster

import (
	"context"
	"iter"
	"testing"
	"time"
)

// Per-call options (Rust's CallOptions { deadline, metadata }) reach the
// handler on all four patterns.

type seenCall struct {
	metadata []MetadataEntry
	deadline time.Time
}

func optionsService(seen chan<- seenCall) *Service {
	record := func(ctx context.Context) {
		info, _ := CallInfoFromContext(ctx)
		seen <- seenCall{info.Metadata, info.Deadline}
	}
	reply := &testEchoReply{Data: []byte{}, ServerLang: "go"}
	return NewService("Options", 1,
		Unary("unary", func(ctx context.Context, _ *testEchoRequest) (*testEchoReply, error) {
			record(ctx)
			return reply, nil
		}),
		ServerStream("server", func(ctx context.Context, _ *testEchoRequest, send func(*testEchoReply) error) error {
			record(ctx)
			return send(reply)
		}),
		ClientStream("client", func(ctx context.Context, reqs iter.Seq2[*testEchoRequest, error]) (*testEchoReply, error) {
			record(ctx)
			for _, err := range reqs {
				if err != nil {
					return nil, err
				}
			}
			return reply, nil
		}),
		BidiStream("bidi", func(ctx context.Context, reqs iter.Seq2[*testEchoRequest, error], send func(*testEchoReply) error) error {
			record(ctx)
			for _, err := range reqs {
				if err != nil {
					return err
				}
				if err := send(reply); err != nil {
					return err
				}
			}
			return nil
		}),
	)
}

func TestCallOptions_DeadlineAndMetadataOnEveryPattern(t *testing.T) {
	seen := make(chan seenCall, 8)
	f := newRPCFixture(t, ServerOptions{Services: []*Service{optionsService(seen)}})
	ctx := context.Background() // no ctx deadline: the option is the only one
	opts := func() []CallOption {
		return []CallOption{WithMetadata("tenant", "t1"), WithTimeout(20 * time.Second), WithMetadata("trace", "x")}
	}
	one := func(yield func(*testEchoRequest) bool) { yield(&testEchoRequest{Message: "m"}) }
	calls := map[string]func() error{
		"unary": func() error {
			_, err := Call[testEchoRequest, testEchoReply](ctx, f.conn, "Options", "unary", &testEchoRequest{}, opts()...)
			return err
		},
		"server": func() error {
			for _, err := range CallServerStream[testEchoRequest, testEchoReply](ctx, f.conn, "Options", "server", &testEchoRequest{}, opts()...) {
				if err != nil {
					return err
				}
			}
			return nil
		},
		"client": func() error {
			_, err := CallClientStream[testEchoRequest, testEchoReply](ctx, f.conn, "Options", "client", one, opts()...)
			return err
		},
		"bidi": func() error {
			for _, err := range CallBidiStream[testEchoRequest, testEchoReply](ctx, f.conn, "Options", "bidi", one, opts()...) {
				if err != nil {
					return err
				}
			}
			return nil
		},
	}
	for name, call := range calls {
		start := time.Now()
		if err := call(); err != nil {
			t.Fatalf("%s: %v", name, err)
		}
		got := <-seen
		md := map[string]string{}
		for _, e := range got.metadata {
			md[e.Key] = e.Value
		}
		if md["tenant"] != "t1" || md["trace"] != "x" {
			t.Errorf("%s: metadata %v", name, got.metadata)
		}
		// The wire deadline is rounded up to whole seconds.
		if got.deadline.IsZero() || got.deadline.Before(start.Add(18*time.Second)) || got.deadline.After(time.Now().Add(22*time.Second)) {
			t.Errorf("%s: deadline %v (start %v)", name, got.deadline, start)
		}
	}
}

func TestCallOptions_EarliestDeadlineWins(t *testing.T) {
	near, far := time.Now().Add(time.Second), time.Now().Add(time.Hour)
	o := buildOptions([]CallOption{WithDeadline(far), WithDeadline(near), WithTimeout(2 * time.Hour)})
	ctx, cancel := o.callContext(context.Background())
	defer cancel()
	if d, ok := ctx.Deadline(); !ok || !d.Equal(near) {
		t.Fatalf("deadline %v", d)
	}
	o = buildOptions([]CallOption{WithTimeout(time.Hour), WithTimeout(time.Minute)})
	ctx2, cancel2 := o.callContext(context.Background())
	defer cancel2()
	if d, ok := ctx2.Deadline(); !ok || time.Until(d) > time.Minute {
		t.Fatalf("timeout deadline %v", d)
	}
	// A ctx deadline earlier than the option's stays.
	// short-wait-ok: the parent's short deadline must survive callContext; it is the value under test
	parent, cancelParent := context.WithTimeout(context.Background(), time.Millisecond)
	defer cancelParent()
	ctx3, cancel3 := buildOptions([]CallOption{WithTimeout(time.Hour)}).callContext(parent)
	defer cancel3()
	if d, _ := ctx3.Deadline(); time.Until(d) > time.Second {
		t.Fatalf("parent deadline lost: %v", d)
	}
	// No option: ctx is used as is.
	ctx4, cancel4 := buildOptions(nil).callContext(context.Background())
	cancel4()
	if _, ok := ctx4.Deadline(); ok || ctx4.Err() != nil {
		t.Fatal("no-option context changed")
	}
}
