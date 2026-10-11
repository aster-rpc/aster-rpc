package aster

// Upcall kinds, statuses and cancel reasons (aster_upcall_kind_t,
// aster_upcall_status_t, aster_upcall_cancel_reason_t). Plain Go so the
// tables are checked against conformance/vectors/upcalls.json without cgo;
// the cgo build re-checks them against the header.

// UpcallKind is the kind of an upcall event.
type UpcallKind uint16

const (
	// UpcallCancel withdraws an earlier upcall (invocation_id names it).
	UpcallCancel UpcallKind = 0
	// UpcallLeaseTransition: B1 -> B2.
	UpcallLeaseTransition UpcallKind = 1
	// UpcallLeaseSnapshot: B3 -> B4.
	UpcallLeaseSnapshot UpcallKind = 2
	// UpcallLeaseAuthorize: B5 -> CF (decision).
	UpcallLeaseAuthorize UpcallKind = 3
	// UpcallFencedMutate: B6 -> B7.
	UpcallFencedMutate UpcallKind = 4
	// UpcallTopologyAdmitted: B8 -> B9 (batched decision).
	UpcallTopologyAdmitted UpcallKind = 16
	// UpcallTopologySnapshot: BA -> BB (placement topology source, ABI 4.4).
	UpcallTopologySnapshot UpcallKind = 17
	// UpcallPlacementPolicy: BC -> BD (ABI 4.4).
	UpcallPlacementPolicy UpcallKind = 32
	// UpcallPlacementProvision: BE -> BF (ABI 4.4).
	UpcallPlacementProvision UpcallKind = 33
	// UpcallExposeAuthorize: C0 -> CF (decision).
	UpcallExposeAuthorize UpcallKind = 48
	// UpcallExposeHTTP: C4 -> C5; bodies move through the HTTPBodies table.
	UpcallExposeHTTP UpcallKind = 49
	// UpcallExposeRoutePolicy: C7 -> C8 (decision).
	UpcallExposeRoutePolicy UpcallKind = 50
	// UpcallAdmissionConnect and UpcallAdmissionHandshake are Gate 0 hooks
	// by upcall (not issued by the C ABI; Gate 0 keeps its own events).
	UpcallAdmissionConnect   UpcallKind = 64
	UpcallAdmissionHandshake UpcallKind = 65
	// UpcallRpcAuthenticate: C9 -> CA (typed authentication, never fail-open).
	UpcallRpcAuthenticate UpcallKind = 80
)

// IsDecision reports whether the kind is a decision kind: with no usable
// answer Rust applies the kind's failure mode instead of failing the call.
func (k UpcallKind) IsDecision() bool {
	switch k {
	case UpcallLeaseAuthorize, UpcallTopologyAdmitted, UpcallExposeAuthorize, UpcallExposeRoutePolicy,
		UpcallAdmissionConnect, UpcallAdmissionHandshake:
		return true
	}
	return false
}

// UpcallStatus is the status of a respond.
type UpcallStatus uint32

const (
	// UpcallStatusOK: the payload is the kind's UCv1 response.
	UpcallStatusOK UpcallStatus = 0
	// UpcallStatusError: the payload is a UTF-8 message.
	UpcallStatusError UpcallStatus = 1
	// UpcallStatusUnimplemented: the binding has no handler for the kind.
	UpcallStatusUnimplemented UpcallStatus = 2
)

// CancelReason says why Rust stopped waiting for an upcall.
type CancelReason uint8

const (
	CancelNone      CancelReason = 0
	CancelTimeout   CancelReason = 1
	CancelAbandoned CancelReason = 2
	CancelShutdown  CancelReason = 3
)

func (r CancelReason) String() string {
	switch r {
	case CancelTimeout:
		return "timeout"
	case CancelAbandoned:
		return "abandoned"
	case CancelShutdown:
		return "shutdown"
	}
	return "none"
}
