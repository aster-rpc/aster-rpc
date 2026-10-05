//go:build cgo

package aster

import (
	"context"
	"errors"
	"testing"
)

func TestReviewBreakerRefusalReleasesEarlierProbe(t *testing.T) {
	probe := NewCircuitBreakerInterceptor()
	probe.FailureThreshold, probe.RecoveryTimeout = 1, 0
	probe.RecordFailure(&RpcError{Code: StatusUnavailable})
	refusing := NewCircuitBreakerInterceptor()
	refusing.FailureThreshold, refusing.RecoveryTimeout = 1, 3600
	refusing.RecordFailure(&RpcError{Code: StatusUnavailable})
	ch := &callChain{breakers: []*CircuitBreakerInterceptor{probe, refusing}}
	_, err := retryLoop(context.Background(), ch, &CallContext{}, &callOptions{}, func() (*int, error) {
		t.Fatal("refused call reached transport")
		return nil, nil
	})
	if !errors.Is(err, ErrRpcUnavailable) {
		t.Fatalf("expected second breaker refusal, got %v", err)
	}
	if err := probe.BeforeCall(); err != nil {
		t.Fatalf("unused probe slot leaked after another breaker refused: %v", err)
	}
}
