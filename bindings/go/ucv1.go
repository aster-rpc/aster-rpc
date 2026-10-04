package aster

// UCv1 (C ABI 4.1 / 4.2), the canonical upcall payload encoding: every
// request and response of an upcall kind is one UCv1 message. Spec:
// docs/_internal/bindings/upcall-design.md section 3; conformance vectors:
// conformance/vectors/upcalls.json (every valid case round-trips byte for
// byte, every invalid input is refused).
//
//	message := tag:u8 0x01 field*
//	uint    := LEB128 varint, minimal length, range-checked to the field's width
//	u8enum  := one raw byte
//	bool    := 0x00 | 0x01
//	string  := varint((utf8_len << 2) | 2) utf8       (minimal header)
//	bytes   := varint(len) raw
//	opt<T>  := 0xFD (absent) | 0x00 T
//	list<T> := varint(count) 0x0C T{count}
//
// Every field is present in a fixed order with no trailing bytes, so each
// value has exactly one encoding. This file is plain Go (no cgo). Placement
// messages (BC..BF) are not part of the C ABI and are not implemented.

import (
	"errors"
	"fmt"
	"unicode/utf8"
)

// ErrUCv1 matches (errors.Is) every UCv1 decode failure.
var ErrUCv1 = errors.New("ucv1: malformed payload")

// UCv1Error is the decode failure of a UCv1 message.
type UCv1Error struct{ Msg string }

func (e *UCv1Error) Error() string { return "ucv1: " + e.Msg }

// Is makes errors.Is(err, ErrUCv1) true.
func (e *UCv1Error) Is(target error) bool { return target == ErrUCv1 }

// UCv1 message tags (upcall-design.md section 3).
const (
	ucv1Version = 0x01

	TagLeaseTransitionRequest   = 0xB1
	TagTransitionOutcome        = 0xB2
	TagLeaseSnapshotRequest     = 0xB3
	TagLeaseSnapshot            = 0xB4
	TagLeaseAuthorizeRequest    = 0xB5
	TagFencedMutateRequest      = 0xB6
	TagFencedMutateResponse     = 0xB7
	TagTopologyAdmittedRequest  = 0xB8
	TagTopologyAdmittedResponse = 0xB9
	TagTopologySnapshotRequest  = 0xBA
	TagTopologySnapshot         = 0xBB
	TagExposeAuthorizeRequest   = 0xC0
	TagAdmissionConnectRequest  = 0xC1
	TagAdmissionHandshakeReq    = 0xC2
	TagAdmissionHandshakeResp   = 0xC3
	TagExposeHTTPRequest        = 0xC4
	TagExposeHTTPResponse       = 0xC5
	TagExposeHTTPTrailers       = 0xC6
	TagExposeRoutePolicyRequest = 0xC7
	TagExposeRouteGrant         = 0xC8
	TagDecision                 = 0xCF

	nullFlag   = 0xFD
	listHeader = 0x0C

	// httpVersionMax is HTTP/3 (0 = HTTP/0.9 ... 4 = HTTP/3).
	httpVersionMax = 4
)

// Route protocol codes of RouteSpec.Protocol.
const (
	RouteProtocolHTTP = 0
	RouteProtocolTCP  = 1
)

// ─── message types ───────────────────────────────────────────────────────

// LeaseOpKind selects a lease verb.
type LeaseOpKind uint8

const (
	LeaseAcquire LeaseOpKind = 1
	LeaseRenew   LeaseOpKind = 2
	LeaseRelease LeaseOpKind = 3
	LeaseRevoke  LeaseOpKind = 4
)

// LeaseOp is a requested lease transition. Actor is the candidate (acquire),
// holder (renew, release) or authority (revoke); TTLMs is set for acquire,
// Epoch for renew and release.
type LeaseOp struct {
	Kind  LeaseOpKind
	Actor string
	TTLMs uint64
	Epoch uint64
}

// Fence is the proof a protected action carries: the resource, the epoch it
// was issued at and its holder.
type Fence struct {
	Resource string
	Epoch    uint64
	Holder   string
}

// LeaseSnapshot is an advisory view of one resource at the serializer.
type LeaseSnapshot struct {
	Resource    string
	Epoch       uint64
	Holder      *string // nil when free
	Held        bool
	RemainingMs *uint64 // nil when free
}

// DenyReason says why a lease transition was refused (UCv1 codes 1..6).
type DenyReason uint8

const (
	DenyHeldByOther   DenyReason = 1
	DenyEpochMismatch DenyReason = 2
	DenyNotHolder     DenyReason = 3
	DenyExpired       DenyReason = 4
	DenyNotHeld       DenyReason = 5
	DenyUnauthorized  DenyReason = 6
)

func (r DenyReason) String() string {
	switch r {
	case DenyHeldByOther:
		return "held_by_other"
	case DenyEpochMismatch:
		return "epoch_mismatch"
	case DenyNotHolder:
		return "not_holder"
	case DenyExpired:
		return "expired"
	case DenyNotHeld:
		return "not_held"
	case DenyUnauthorized:
		return "unauthorized"
	}
	return fmt.Sprintf("deny(%d)", uint8(r))
}

// TransitionKind is the result of a lease transition.
type TransitionKind uint8

const (
	TransitionGranted  TransitionKind = 1
	TransitionRenewed  TransitionKind = 2
	TransitionReleased TransitionKind = 3
	TransitionRevoked  TransitionKind = 4
	TransitionDenied   TransitionKind = 5
)

// TransitionOutcome is the serializer's answer to a LeaseOp. Fence is set
// for Granted; Reason and Snapshot for Denied.
type TransitionOutcome struct {
	Kind     TransitionKind
	Fence    Fence
	Reason   DenyReason
	Snapshot LeaseSnapshot
}

// FenceRejectionReason says why a fenced action was rejected (codes 1..4).
type FenceRejectionReason uint8

const (
	RejectStaleEpoch  FenceRejectionReason = 1
	RejectNotHolder   FenceRejectionReason = 2
	RejectNotHeld     FenceRejectionReason = 3
	RejectFutureEpoch FenceRejectionReason = 4
)

func (r FenceRejectionReason) String() string {
	switch r {
	case RejectStaleEpoch:
		return "stale_epoch"
	case RejectNotHolder:
		return "not_holder"
	case RejectNotHeld:
		return "not_held"
	case RejectFutureEpoch:
		return "future_epoch"
	}
	return fmt.Sprintf("reject(%d)", uint8(r))
}

// FenceRejection is a refused fenced action. Carried and Current are the
// epochs for stale and future rejections, 0 otherwise.
type FenceRejection struct {
	Reason  FenceRejectionReason
	Carried uint64
	Current uint64
}

// CheckFence is the fence rule (aster::lease check_fence): accept iff the
// carried epoch equals the row's, the row is held and writer is its holder.
// Evaluate it inside the resource's own serialization point, atomically with
// the write. nil means the fence passes.
func CheckFence(rowEpoch uint64, rowHeld bool, rowHolder string, fence Fence, writer string) *FenceRejection {
	switch {
	case fence.Epoch < rowEpoch:
		return &FenceRejection{RejectStaleEpoch, fence.Epoch, rowEpoch}
	case fence.Epoch > rowEpoch:
		return &FenceRejection{RejectFutureEpoch, fence.Epoch, rowEpoch}
	case !rowHeld:
		return &FenceRejection{Reason: RejectNotHeld}
	case rowHolder != writer:
		return &FenceRejection{Reason: RejectNotHolder}
	}
	return nil
}

// MutateResult is the answer of a fenced resource: Applied with Output, or
// Rejected (Rejection != nil).
type MutateResult struct {
	Output    []byte
	Rejection *FenceRejection
}

// Applied reports whether the mutation was applied.
func (m MutateResult) Applied() bool { return m.Rejection == nil }

// LeaseTransitionRequest is the kind-1 request.
type LeaseTransitionRequest struct {
	Resource string
	Op       LeaseOp
}

// FencedMutateRequest is the kind-4 request.
type FencedMutateRequest struct {
	Fence  Fence
	Writer string
	Op     []byte
}

// ClusterView is one cluster of a serialized topology view.
type ClusterView struct {
	Members   []string
	Bridge    string
	Witnesses []string
}

// FarPair is a corroborated far pair of a topology view.
type FarPair struct{ A, B string }

// TopologySnapshot is a serialized shared topology view (kind 17; the C ABI
// does not issue it yet).
type TopologySnapshot struct {
	Clusters           []ClusterView
	FarPairs           []FarPair
	SeparationCoverage uint64
}

// ExposeAuthorizeRequest is the kind-48 request.
type ExposeAuthorizeRequest struct {
	ServiceID string
	PeerID    string
	Metadata  []byte
}

// AdmissionConnectRequest is the kind-64 request.
type AdmissionConnectRequest struct {
	Peer string
	ALPN []byte
}

// AdmissionHandshakeRequest is the kind-65 request.
type AdmissionHandshakeRequest struct {
	Peer    string
	ALPN    []byte
	IsAlive bool
}

// AdmissionHandshakeResponse accepts, or rejects with a code and reason.
type AdmissionHandshakeResponse struct {
	Accept bool
	Code   uint32
	Reason []byte
}

// HTTPHeader is one header; a repeated name is one entry per value. Name is
// lowercase and non-empty.
type HTTPHeader struct {
	Name  string
	Value []byte
}

// HTTPBody says where an HTTP message body travels: inline in the message
// (possibly empty) or streamed through the exchange's body handle.
type HTTPBody struct {
	Stream bool
	Inline []byte
}

// ExposeHTTPRequest is the kind-49 request. HasPeer is false when the
// request did not arrive over the relay (PeerID is then empty). Version is
// 0 HTTP/0.9, 1 HTTP/1.0, 2 HTTP/1.1, 3 HTTP/2, 4 HTTP/3.
type ExposeHTTPRequest struct {
	ServiceID string
	PeerID    string
	HasPeer   bool
	StreamID  uint64
	Version   uint8
	Method    string
	URI       string
	Headers   []HTTPHeader
	Body      HTTPBody
}

// ExposeHTTPResponse is the head of a kind-49 answer; Status is 100..=999.
type ExposeHTTPResponse struct {
	Status  uint16
	Headers []HTTPHeader
	Body    HTTPBody
}

// RouteSpec is one route a peer asks an edge to serve.
type RouteSpec struct {
	Host      string
	Port      uint16
	Protocol  uint8 // RouteProtocolHTTP or RouteProtocolTCP
	ServiceID string
}

// ExposeRoutePolicyRequest is the kind-50 request.
type ExposeRoutePolicyRequest struct {
	PeerID   string
	Metadata []byte
	Specs    []RouteSpec
}

// ─── writer / reader ─────────────────────────────────────────────────────

type ucw struct{ b []byte }

func newUCW(tag byte) *ucw { return &ucw{b: []byte{tag, ucv1Version}} }

func (w *ucw) u8(v uint8) *ucw { w.b = append(w.b, v); return w }

func (w *ucw) uint(v uint64) *ucw {
	for v >= 0x80 {
		w.b = append(w.b, byte(v)|0x80)
		v >>= 7
	}
	w.b = append(w.b, byte(v))
	return w
}

func (w *ucw) bool(v bool) *ucw {
	if v {
		return w.u8(1)
	}
	return w.u8(0)
}

func (w *ucw) str(s string) *ucw {
	w.uint(uint64(len(s))<<2 | 2)
	w.b = append(w.b, s...)
	return w
}

func (w *ucw) bytes(p []byte) *ucw {
	w.uint(uint64(len(p)))
	w.b = append(w.b, p...)
	return w
}

func (w *ucw) list(n int) *ucw { return w.uint(uint64(n)).u8(listHeader) }

func (w *ucw) strs(v []string) *ucw {
	w.list(len(v))
	for _, s := range v {
		w.str(s)
	}
	return w
}

func (w *ucw) optStr(s *string) *ucw {
	if s == nil {
		return w.u8(nullFlag)
	}
	return w.u8(0).str(*s)
}

func (w *ucw) optUint(v *uint64) *ucw {
	if v == nil {
		return w.u8(nullFlag)
	}
	return w.u8(0).uint(*v)
}

// ucr is a sticky-error reader: after the first failure every method
// returns a zero value and err stays set.
type ucr struct {
	b   []byte
	pos int
	err error
}

func newUCR(b []byte, tag byte) *ucr {
	r := &ucr{b: b}
	switch {
	case len(b) == 0:
		r.fail("empty payload")
	case b[0] != tag:
		r.fail("tag 0x%02x, expected 0x%02x", b[0], tag)
	case len(b) < 2:
		r.fail("missing version")
	case b[1] != ucv1Version:
		r.fail("unsupported version 0x%02x", b[1])
	default:
		r.pos = 2
	}
	return r
}

func (r *ucr) fail(format string, a ...any) {
	if r.err == nil {
		r.err = &UCv1Error{Msg: fmt.Sprintf(format, a...)}
	}
}

func (r *ucr) remaining() int { return len(r.b) - r.pos }

func (r *ucr) u8() uint8 {
	if r.err != nil {
		return 0
	}
	if r.remaining() < 1 {
		r.fail("unexpected EOF at offset %d", r.pos)
		return 0
	}
	v := r.b[r.pos]
	r.pos++
	return v
}

// uint reads an unsigned LEB128, minimal, at most 64 bits.
func (r *ucr) uint() uint64 {
	if r.err != nil {
		return 0
	}
	start := r.pos
	var v uint64
	for i := 0; i < 10; i++ {
		b := r.u8()
		if r.err != nil {
			return 0
		}
		if i == 9 && b > 1 {
			r.fail("varint overflows 64 bits at offset %d", start)
			return 0
		}
		v |= uint64(b&0x7f) << (7 * uint(i))
		if b&0x80 == 0 {
			if i > 0 && b == 0 {
				r.fail("non-minimal varint at offset %d", start)
				return 0
			}
			return v
		}
	}
	r.fail("varint too long at offset %d", start)
	return 0
}

func (r *ucr) uintMax(max uint64, what string) uint64 {
	v := r.uint()
	if r.err == nil && v > max {
		r.fail("%s %d out of range", what, v)
		return 0
	}
	return v
}

func (r *ucr) bool() bool {
	b := r.u8()
	if r.err == nil && b > 1 {
		r.fail("invalid bool byte %d", b)
	}
	return b == 1
}

func (r *ucr) str() string {
	if r.err != nil {
		return ""
	}
	start := r.pos
	h := r.uint()
	if r.err != nil {
		return ""
	}
	if h&3 != 2 {
		r.fail("string header has wrong encoding bits at offset %d", start)
		return ""
	}
	n := h >> 2
	if n > uint64(r.remaining()) {
		r.fail("unexpected EOF at offset %d", r.pos)
		return ""
	}
	s := r.b[r.pos : r.pos+int(n)]
	if !utf8.Valid(s) {
		r.fail("invalid UTF-8 at offset %d", start)
		return ""
	}
	r.pos += int(n)
	return string(s)
}

func (r *ucr) bytes() []byte {
	if r.err != nil {
		return nil
	}
	n := r.uint()
	if r.err != nil {
		return nil
	}
	if n > uint64(r.remaining()) {
		r.fail("bytes length %d exceeds payload", n)
		return nil
	}
	out := make([]byte, n)
	copy(out, r.b[r.pos:r.pos+int(n)])
	r.pos += int(n)
	return out
}

func (r *ucr) list() int {
	n := r.uint()
	if r.err != nil {
		return 0
	}
	if h := r.u8(); r.err == nil && h != listHeader {
		r.fail("list element header mismatch")
		return 0
	}
	if r.err == nil && n > uint64(r.remaining()) {
		r.fail("list count %d exceeds payload", n)
		return 0
	}
	return int(n)
}

func (r *ucr) present() bool {
	b := r.u8()
	if r.err != nil {
		return false
	}
	switch b {
	case 0:
		return true
	case nullFlag:
		return false
	}
	r.fail("invalid optional marker %d", b)
	return false
}

func (r *ucr) strs() []string {
	n := r.list()
	out := make([]string, 0, n)
	for i := 0; i < n && r.err == nil; i++ {
		out = append(out, r.str())
	}
	return out
}

func (r *ucr) end() {
	if r.err == nil && r.pos != len(r.b) {
		r.fail("%d trailing bytes", len(r.b)-r.pos)
	}
}

// ─── leases ──────────────────────────────────────────────────────────────

func putOp(w *ucw, op LeaseOp) {
	w.u8(uint8(op.Kind))
	switch op.Kind {
	case LeaseAcquire:
		w.str(op.Actor).uint(op.TTLMs)
	case LeaseRenew, LeaseRelease:
		w.str(op.Actor).uint(op.Epoch)
	default: // LeaseRevoke
		w.str(op.Actor)
	}
}

func getOp(r *ucr) LeaseOp {
	k := LeaseOpKind(r.u8())
	switch k {
	case LeaseAcquire:
		return LeaseOp{Kind: k, Actor: r.str(), TTLMs: r.uint()}
	case LeaseRenew, LeaseRelease:
		return LeaseOp{Kind: k, Actor: r.str(), Epoch: r.uint()}
	case LeaseRevoke:
		return LeaseOp{Kind: k, Actor: r.str()}
	}
	r.fail("unknown lease op %d", uint8(k))
	return LeaseOp{}
}

func putFence(w *ucw, f Fence) { w.str(f.Resource).uint(f.Epoch).str(f.Holder) }

func getFence(r *ucr) Fence {
	var f Fence
	f.Resource = r.str()
	f.Epoch = r.uint()
	f.Holder = r.str()
	return f
}

func putSnapshot(w *ucw, s LeaseSnapshot) {
	w.str(s.Resource).uint(s.Epoch).optStr(s.Holder).bool(s.Held).optUint(s.RemainingMs)
}

func getSnapshot(r *ucr) LeaseSnapshot {
	var s LeaseSnapshot
	s.Resource = r.str()
	s.Epoch = r.uint()
	if r.present() {
		h := r.str()
		s.Holder = &h
	}
	s.Held = r.bool()
	if r.present() {
		v := r.uint()
		s.RemainingMs = &v
	}
	return s
}

// EncodeLeaseTransitionRequest encodes kind 1's request (B1).
func EncodeLeaseTransitionRequest(v LeaseTransitionRequest) []byte {
	w := newUCW(TagLeaseTransitionRequest).str(v.Resource)
	putOp(w, v.Op)
	return w.b
}

// DecodeLeaseTransitionRequest decodes kind 1's request (B1).
func DecodeLeaseTransitionRequest(b []byte) (LeaseTransitionRequest, error) {
	r := newUCR(b, TagLeaseTransitionRequest)
	v := LeaseTransitionRequest{Resource: r.str(), Op: getOp(r)}
	r.end()
	return v, r.err
}

// EncodeTransitionOutcome encodes kind 1's response (B2).
func EncodeTransitionOutcome(o TransitionOutcome) []byte {
	w := newUCW(TagTransitionOutcome).u8(uint8(o.Kind))
	switch o.Kind {
	case TransitionGranted:
		putFence(w, o.Fence)
	case TransitionDenied:
		w.u8(uint8(o.Reason))
		putSnapshot(w, o.Snapshot)
	}
	return w.b
}

// DecodeTransitionOutcome decodes kind 1's response (B2).
func DecodeTransitionOutcome(b []byte) (TransitionOutcome, error) {
	r := newUCR(b, TagTransitionOutcome)
	o := TransitionOutcome{Kind: TransitionKind(r.u8())}
	switch o.Kind {
	case TransitionGranted:
		o.Fence = getFence(r)
	case TransitionRenewed, TransitionReleased, TransitionRevoked:
	case TransitionDenied:
		o.Reason = DenyReason(r.u8())
		if r.err == nil && (o.Reason < DenyHeldByOther || o.Reason > DenyUnauthorized) {
			r.fail("unknown deny reason %d", uint8(o.Reason))
		}
		o.Snapshot = getSnapshot(r)
	default:
		r.fail("unknown transition outcome %d", uint8(o.Kind))
	}
	r.end()
	if r.err != nil {
		return TransitionOutcome{}, r.err
	}
	return o, nil
}

// EncodeLeaseSnapshotRequest encodes kind 2's request (B3).
func EncodeLeaseSnapshotRequest(resource string) []byte {
	return newUCW(TagLeaseSnapshotRequest).str(resource).b
}

// DecodeLeaseSnapshotRequest decodes kind 2's request (B3).
func DecodeLeaseSnapshotRequest(b []byte) (string, error) {
	r := newUCR(b, TagLeaseSnapshotRequest)
	s := r.str()
	r.end()
	return s, r.err
}

// EncodeLeaseSnapshot encodes kind 2's response (B4).
func EncodeLeaseSnapshot(s LeaseSnapshot) []byte {
	w := newUCW(TagLeaseSnapshot)
	putSnapshot(w, s)
	return w.b
}

// DecodeLeaseSnapshot decodes kind 2's response (B4).
func DecodeLeaseSnapshot(b []byte) (LeaseSnapshot, error) {
	r := newUCR(b, TagLeaseSnapshot)
	s := getSnapshot(r)
	r.end()
	return s, r.err
}

// EncodeLeaseAuthorizeRequest encodes kind 3's request (B5).
func EncodeLeaseAuthorizeRequest(op LeaseOp) []byte {
	w := newUCW(TagLeaseAuthorizeRequest)
	putOp(w, op)
	return w.b
}

// DecodeLeaseAuthorizeRequest decodes kind 3's request (B5).
func DecodeLeaseAuthorizeRequest(b []byte) (LeaseOp, error) {
	r := newUCR(b, TagLeaseAuthorizeRequest)
	op := getOp(r)
	r.end()
	return op, r.err
}

// EncodeFencedMutateRequest encodes kind 4's request (B6).
func EncodeFencedMutateRequest(v FencedMutateRequest) []byte {
	w := newUCW(TagFencedMutateRequest)
	putFence(w, v.Fence)
	return w.str(v.Writer).bytes(v.Op).b
}

// DecodeFencedMutateRequest decodes kind 4's request (B6).
func DecodeFencedMutateRequest(b []byte) (FencedMutateRequest, error) {
	r := newUCR(b, TagFencedMutateRequest)
	v := FencedMutateRequest{Fence: getFence(r), Writer: r.str(), Op: r.bytes()}
	r.end()
	return v, r.err
}

// EncodeFencedMutateResponse encodes kind 4's response (B7).
func EncodeFencedMutateResponse(m MutateResult) []byte {
	w := newUCW(TagFencedMutateResponse)
	if m.Rejection == nil {
		return w.u8(0).bytes(m.Output).b
	}
	rej := m.Rejection
	var carried, current uint64
	if rej.Reason == RejectStaleEpoch || rej.Reason == RejectFutureEpoch {
		carried, current = rej.Carried, rej.Current
	}
	return w.u8(1).u8(uint8(rej.Reason)).uint(carried).uint(current).b
}

// DecodeFencedMutateResponse decodes kind 4's response (B7).
func DecodeFencedMutateResponse(b []byte) (MutateResult, error) {
	r := newUCR(b, TagFencedMutateResponse)
	var out MutateResult
	switch kind := r.u8(); kind {
	case 0:
		out.Output = r.bytes()
	case 1:
		reason := FenceRejectionReason(r.u8())
		carried, current := r.uint(), r.uint()
		if r.err == nil && (reason < RejectStaleEpoch || reason > RejectFutureEpoch) {
			r.fail("unknown fence rejection %d", uint8(reason))
		}
		if r.err == nil && (reason == RejectNotHolder || reason == RejectNotHeld) && (carried != 0 || current != 0) {
			r.fail("epochs must be 0 for rejection %d", uint8(reason))
		}
		out.Rejection = &FenceRejection{Reason: reason, Carried: carried, Current: current}
	default:
		r.fail("unknown fenced-mutate result %d", kind)
	}
	r.end()
	if r.err != nil {
		return MutateResult{}, r.err
	}
	return out, nil
}

// ─── decisions ───────────────────────────────────────────────────────────

// EncodeDecision encodes the response of the decision kinds 3, 48, 64 (CF).
func EncodeDecision(allowed bool) []byte { return newUCW(TagDecision).bool(allowed).b }

// DecodeDecision decodes a decision (CF).
func DecodeDecision(b []byte) (bool, error) {
	r := newUCR(b, TagDecision)
	v := r.bool()
	r.end()
	return v, r.err
}

// ─── topology ────────────────────────────────────────────────────────────

// EncodeTopologyAdmittedRequest encodes kind 16's request (B8): lowercase
// hex node ids.
func EncodeTopologyAdmittedRequest(nodeIDs []string) []byte {
	return newUCW(TagTopologyAdmittedRequest).strs(nodeIDs).b
}

// DecodeTopologyAdmittedRequest decodes kind 16's request (B8).
func DecodeTopologyAdmittedRequest(b []byte) ([]string, error) {
	r := newUCR(b, TagTopologyAdmittedRequest)
	v := r.strs()
	r.end()
	return v, r.err
}

// EncodeTopologyAdmittedResponse encodes kind 16's response (B9): one answer
// per requested id, same order.
func EncodeTopologyAdmittedResponse(admitted []bool) []byte {
	w := newUCW(TagTopologyAdmittedResponse).list(len(admitted))
	for _, a := range admitted {
		w.bool(a)
	}
	return w.b
}

// DecodeTopologyAdmittedResponse decodes kind 16's response (B9).
func DecodeTopologyAdmittedResponse(b []byte) ([]bool, error) {
	r := newUCR(b, TagTopologyAdmittedResponse)
	n := r.list()
	out := make([]bool, 0, n)
	for i := 0; i < n && r.err == nil; i++ {
		out = append(out, r.bool())
	}
	r.end()
	return out, r.err
}

// EncodeTopologySnapshotRequest encodes kind 17's request (BA).
func EncodeTopologySnapshotRequest() []byte { return newUCW(TagTopologySnapshotRequest).b }

// DecodeTopologySnapshotRequest decodes kind 17's request (BA).
func DecodeTopologySnapshotRequest(b []byte) error {
	r := newUCR(b, TagTopologySnapshotRequest)
	r.end()
	return r.err
}

// EncodeTopologySnapshot encodes kind 17's response (BB).
func EncodeTopologySnapshot(v TopologySnapshot) []byte {
	w := newUCW(TagTopologySnapshot).list(len(v.Clusters))
	for _, c := range v.Clusters {
		w.strs(c.Members).str(c.Bridge).strs(c.Witnesses)
	}
	w.list(len(v.FarPairs))
	for _, p := range v.FarPairs {
		w.str(p.A).str(p.B)
	}
	return w.uint(v.SeparationCoverage).b
}

// DecodeTopologySnapshot decodes kind 17's response (BB).
func DecodeTopologySnapshot(b []byte) (TopologySnapshot, error) {
	r := newUCR(b, TagTopologySnapshot)
	var v TopologySnapshot
	n := r.list()
	for i := 0; i < n && r.err == nil; i++ {
		v.Clusters = append(v.Clusters, ClusterView{Members: r.strs(), Bridge: r.str(), Witnesses: r.strs()})
	}
	m := r.list()
	for i := 0; i < m && r.err == nil; i++ {
		v.FarPairs = append(v.FarPairs, FarPair{A: r.str(), B: r.str()})
	}
	v.SeparationCoverage = r.uint()
	r.end()
	if r.err != nil {
		return TopologySnapshot{}, r.err
	}
	return v, nil
}

// ─── expose / admission ──────────────────────────────────────────────────

// EncodeExposeAuthorizeRequest encodes kind 48's request (C0).
func EncodeExposeAuthorizeRequest(v ExposeAuthorizeRequest) []byte {
	return newUCW(TagExposeAuthorizeRequest).str(v.ServiceID).str(v.PeerID).bytes(v.Metadata).b
}

// DecodeExposeAuthorizeRequest decodes kind 48's request (C0).
func DecodeExposeAuthorizeRequest(b []byte) (ExposeAuthorizeRequest, error) {
	r := newUCR(b, TagExposeAuthorizeRequest)
	v := ExposeAuthorizeRequest{ServiceID: r.str(), PeerID: r.str(), Metadata: r.bytes()}
	r.end()
	return v, r.err
}

// EncodeAdmissionConnectRequest encodes kind 64's request (C1).
func EncodeAdmissionConnectRequest(v AdmissionConnectRequest) []byte {
	return newUCW(TagAdmissionConnectRequest).str(v.Peer).bytes(v.ALPN).b
}

// DecodeAdmissionConnectRequest decodes kind 64's request (C1).
func DecodeAdmissionConnectRequest(b []byte) (AdmissionConnectRequest, error) {
	r := newUCR(b, TagAdmissionConnectRequest)
	v := AdmissionConnectRequest{Peer: r.str(), ALPN: r.bytes()}
	r.end()
	return v, r.err
}

// EncodeAdmissionHandshakeRequest encodes kind 65's request (C2).
func EncodeAdmissionHandshakeRequest(v AdmissionHandshakeRequest) []byte {
	return newUCW(TagAdmissionHandshakeReq).str(v.Peer).bytes(v.ALPN).bool(v.IsAlive).b
}

// DecodeAdmissionHandshakeRequest decodes kind 65's request (C2).
func DecodeAdmissionHandshakeRequest(b []byte) (AdmissionHandshakeRequest, error) {
	r := newUCR(b, TagAdmissionHandshakeReq)
	v := AdmissionHandshakeRequest{Peer: r.str(), ALPN: r.bytes(), IsAlive: r.bool()}
	r.end()
	return v, r.err
}

// EncodeAdmissionHandshakeResponse encodes kind 65's response (C3). An
// accept carries code 0 and no reason.
func EncodeAdmissionHandshakeResponse(v AdmissionHandshakeResponse) []byte {
	w := newUCW(TagAdmissionHandshakeResp)
	if v.Accept {
		return w.bool(true).uint(0).bytes(nil).b
	}
	return w.bool(false).uint(uint64(v.Code)).bytes(v.Reason).b
}

// DecodeAdmissionHandshakeResponse decodes kind 65's response (C3).
func DecodeAdmissionHandshakeResponse(b []byte) (AdmissionHandshakeResponse, error) {
	r := newUCR(b, TagAdmissionHandshakeResp)
	v := AdmissionHandshakeResponse{Accept: r.bool(), Code: uint32(r.uintMax(0xFFFF_FFFF, "reject code")), Reason: r.bytes()}
	r.end()
	if r.err == nil && v.Accept && (v.Code != 0 || len(v.Reason) != 0) {
		r.fail("accept carries no code or reason")
	}
	return v, r.err
}

func putHeaders(w *ucw, hs []HTTPHeader) {
	w.list(len(hs))
	for _, h := range hs {
		w.str(h.Name).bytes(h.Value)
	}
}

func getHeaders(r *ucr) []HTTPHeader {
	n := r.list()
	var out []HTTPHeader
	for i := 0; i < n && r.err == nil; i++ {
		name := r.str()
		if r.err == nil && !validHeaderName(name) {
			r.fail("header name %q must be non-empty and lowercase", name)
			return nil
		}
		out = append(out, HTTPHeader{Name: name, Value: r.bytes()})
	}
	return out
}

func validHeaderName(s string) bool {
	if s == "" {
		return false
	}
	for i := 0; i < len(s); i++ {
		if s[i] >= 'A' && s[i] <= 'Z' {
			return false
		}
	}
	return true
}

func putBody(w *ucw, b HTTPBody) {
	if b.Stream {
		w.u8(1)
		return
	}
	w.u8(0).bytes(b.Inline)
}

func getBody(r *ucr) HTTPBody {
	switch mode := r.u8(); mode {
	case 0:
		return HTTPBody{Inline: r.bytes()}
	case 1:
		return HTTPBody{Stream: true}
	default:
		r.fail("unknown body mode %d", mode)
	}
	return HTTPBody{}
}

// EncodeExposeHTTPRequest encodes kind 49's request (C4).
func EncodeExposeHTTPRequest(v ExposeHTTPRequest) []byte {
	w := newUCW(TagExposeHTTPRequest).str(v.ServiceID)
	if v.HasPeer {
		w.u8(0).str(v.PeerID)
	} else {
		w.u8(nullFlag)
	}
	w.uint(v.StreamID).u8(v.Version).str(v.Method).str(v.URI)
	putHeaders(w, v.Headers)
	putBody(w, v.Body)
	return w.b
}

// DecodeExposeHTTPRequest decodes kind 49's request (C4).
func DecodeExposeHTTPRequest(b []byte) (ExposeHTTPRequest, error) {
	r := newUCR(b, TagExposeHTTPRequest)
	var v ExposeHTTPRequest
	v.ServiceID = r.str()
	if r.present() {
		v.HasPeer = true
		v.PeerID = r.str()
	}
	v.StreamID = r.uint()
	v.Version = r.u8()
	if r.err == nil && v.Version > httpVersionMax {
		r.fail("unknown HTTP version %d", v.Version)
	}
	v.Method = r.str()
	if r.err == nil && v.Method == "" {
		r.fail("empty HTTP method")
	}
	v.URI = r.str()
	v.Headers = getHeaders(r)
	v.Body = getBody(r)
	r.end()
	if r.err != nil {
		return ExposeHTTPRequest{}, r.err
	}
	return v, nil
}

// EncodeExposeHTTPResponse encodes kind 49's response (C5).
func EncodeExposeHTTPResponse(v ExposeHTTPResponse) []byte {
	w := newUCW(TagExposeHTTPResponse).uint(uint64(v.Status))
	putHeaders(w, v.Headers)
	putBody(w, v.Body)
	return w.b
}

// DecodeExposeHTTPResponse decodes kind 49's response (C5).
func DecodeExposeHTTPResponse(b []byte) (ExposeHTTPResponse, error) {
	r := newUCR(b, TagExposeHTTPResponse)
	status := r.uintMax(999, "HTTP status")
	if r.err == nil && status < 100 {
		r.fail("HTTP status %d below 100", status)
	}
	v := ExposeHTTPResponse{Status: uint16(status)}
	v.Headers = getHeaders(r)
	v.Body = getBody(r)
	r.end()
	if r.err != nil {
		return ExposeHTTPResponse{}, r.err
	}
	return v, nil
}

// EncodeExposeHTTPTrailers encodes the trailers of aster_http_body_finish (C6).
func EncodeExposeHTTPTrailers(trailers []HTTPHeader) []byte {
	w := newUCW(TagExposeHTTPTrailers)
	putHeaders(w, trailers)
	return w.b
}

// DecodeExposeHTTPTrailers decodes trailers (C6).
func DecodeExposeHTTPTrailers(b []byte) ([]HTTPHeader, error) {
	r := newUCR(b, TagExposeHTTPTrailers)
	v := getHeaders(r)
	r.end()
	return v, r.err
}

// EncodeExposeRoutePolicyRequest encodes kind 50's request (C7).
func EncodeExposeRoutePolicyRequest(v ExposeRoutePolicyRequest) []byte {
	w := newUCW(TagExposeRoutePolicyRequest).str(v.PeerID).bytes(v.Metadata).list(len(v.Specs))
	for _, s := range v.Specs {
		w.str(s.Host).uint(uint64(s.Port)).u8(s.Protocol).str(s.ServiceID)
	}
	return w.b
}

// DecodeExposeRoutePolicyRequest decodes kind 50's request (C7).
func DecodeExposeRoutePolicyRequest(b []byte) (ExposeRoutePolicyRequest, error) {
	r := newUCR(b, TagExposeRoutePolicyRequest)
	v := ExposeRoutePolicyRequest{PeerID: r.str(), Metadata: r.bytes()}
	n := r.list()
	for i := 0; i < n && r.err == nil; i++ {
		s := RouteSpec{Host: r.str(), Port: uint16(r.uintMax(0xFFFF, "port")), Protocol: r.u8()}
		if r.err == nil && s.Protocol > RouteProtocolTCP {
			r.fail("unknown route protocol %d", s.Protocol)
		}
		s.ServiceID = r.str()
		v.Specs = append(v.Specs, s)
	}
	r.end()
	if r.err != nil {
		return ExposeRoutePolicyRequest{}, r.err
	}
	return v, nil
}

// EncodeExposeRouteGrant encodes kind 50's response (C8): the indexes of the
// granted specs, strictly ascending.
func EncodeExposeRouteGrant(granted []uint32) []byte {
	w := newUCW(TagExposeRouteGrant).list(len(granted))
	for _, i := range granted {
		w.uint(uint64(i))
	}
	return w.b
}

// DecodeExposeRouteGrant decodes kind 50's response (C8).
func DecodeExposeRouteGrant(b []byte) ([]uint32, error) {
	r := newUCR(b, TagExposeRouteGrant)
	n := r.list()
	out := make([]uint32, 0, n)
	for i := 0; i < n && r.err == nil; i++ {
		idx := uint32(r.uintMax(0xFFFF_FFFF, "route index"))
		if r.err == nil && len(out) > 0 && out[len(out)-1] >= idx {
			r.fail("granted route indexes not strictly ascending")
		}
		out = append(out, idx)
	}
	r.end()
	if r.err != nil {
		return nil, r.err
	}
	return out, nil
}
