package aster

// Shared UCv1 helpers for the coordination (D0-DE) codec and the placement
// messages BA-BF (kinds 17 / 32 / 33). Plain Go, no cgo.

import "bytes"

// fixed reads a bytes field of exactly n bytes.
func (r *ucr) fixed(n int, what string) []byte {
	b := r.bytes()
	if r.err == nil && len(b) != n {
		r.fail("%s is %d bytes, want %d", what, len(b), n)
		return nil
	}
	return b
}

func (r *ucr) fixedList(n int, what string) [][]byte {
	cnt := r.list()
	out := make([][]byte, 0, cnt)
	for i := 0; i < cnt && r.err == nil; i++ {
		out = append(out, r.fixed(n, what))
	}
	return out
}

func (r *ucr) bytesList() [][]byte {
	cnt := r.list()
	out := make([][]byte, 0, cnt)
	for i := 0; i < cnt && r.err == nil; i++ {
		out = append(out, r.bytes())
	}
	return out
}

// ascending refuses a list that is not in strictly ascending byte order.
func (r *ucr) ascending(list [][]byte, what string) {
	for i := 1; i < len(list) && r.err == nil; i++ {
		if bytes.Compare(list[i-1], list[i]) >= 0 {
			r.fail("%s not in strictly ascending order", what)
		}
	}
}

// code reads an i32 code-space value as its u32 bit pattern.
func (r *ucr) code() uint32 { return uint32(r.uintMax(0xFFFF_FFFF, "code")) }

func (r *ucr) optBytes() []byte {
	if !r.present() {
		return nil
	}
	b := r.bytes()
	if b == nil && r.err == nil {
		b = []byte{}
	}
	return b
}

func (w *ucw) bytesList(v [][]byte) *ucw {
	w.list(len(v))
	for _, b := range v {
		w.bytes(b)
	}
	return w
}

func (w *ucw) optBytes(b []byte) *ucw {
	if b == nil {
		return w.u8(nullFlag)
	}
	return w.u8(0).bytes(b)
}

// LogID identifies a Raft log entry (UCv1 LogId).
type LogID struct {
	Term   uint64
	Leader []byte // 32-byte node id
	Index  uint64
}

func (w *ucw) logID(l LogID) *ucw { return w.uint(l.Term).bytes(l.Leader).uint(l.Index) }

func (w *ucw) optLogID(l *LogID) *ucw {
	if l == nil {
		return w.u8(nullFlag)
	}
	return w.u8(0).logID(*l)
}

func (r *ucr) logID() LogID {
	var l LogID
	l.Term = r.uint()
	l.Leader = r.fixed(32, "log id leader")
	l.Index = r.uint()
	return l
}

func (r *ucr) optLogID() *LogID {
	if !r.present() {
		return nil
	}
	l := r.logID()
	return &l
}

// MemberSeed is how a joining member is reached (UCv1 MemberSeed, BF).
type MemberSeed struct {
	NodeID           []byte // 32 bytes
	Ticket           string
	SignedJoinPermit []byte // nil when absent
}

func (w *ucw) memberSeed(m MemberSeed) *ucw {
	return w.bytes(m.NodeID).str(m.Ticket).optBytes(m.SignedJoinPermit)
}

func (r *ucr) memberSeed() MemberSeed {
	var m MemberSeed
	m.NodeID = r.fixed(32, "member seed node id")
	m.Ticket = r.str()
	m.SignedJoinPermit = r.optBytes()
	return m
}

// EncodeMemberSeed encodes BF.
func EncodeMemberSeed(m MemberSeed) []byte { return newUCW(0xBF).memberSeed(m).b }

// DecodeMemberSeed decodes BF.
func DecodeMemberSeed(b []byte) (MemberSeed, error) {
	r := newUCR(b, 0xBF)
	m := r.memberSeed()
	r.end()
	return m, r.err
}

// ─── placement (kinds 17 / 32 / 33) ──────────────────────────────────────

// PlacementCandidate is an eligible node and, optionally, how to reach it.
type PlacementCandidate struct {
	NodeID     []byte      // 32 bytes
	MemberSeed *MemberSeed // nil when the node is already a member
}

// PlacementPolicy is the membership policy the placement manager enforces
// (UCv1 BD). Eligible, PinnedVoters and Excluded must be in strictly
// ascending node-id order.
type PlacementPolicy struct {
	Revision                   uint64
	Digest                     []byte // 32 bytes
	AuthorityPublicKey         []byte // 32 bytes
	AuthorityDomain            string
	TargetVoters               uint8
	TargetRetainedLearners     uint8
	Eligible                   []PlacementCandidate
	PinnedVoters               [][]byte
	Excluded                   [][]byte
	MinimumSeparatedVoterPairs uint8
	ReplacementGraceMs         uint64
	AllowHealthyRebalance      bool
}

// ProvisionRequest asks the provisioner to prepare a node to join (BE).
type ProvisionRequest struct {
	GroupID         []byte // 32 bytes
	PolicyDigest    []byte // 32 bytes
	MembershipLogID *LogID
	NodeID          []byte // 32 bytes
}

// EncodePlacementPolicyRequest encodes kind 32's request (BC).
func EncodePlacementPolicyRequest() []byte { return newUCW(0xBC).b }

// DecodePlacementPolicyRequest decodes kind 32's request (BC).
func DecodePlacementPolicyRequest(b []byte) error {
	r := newUCR(b, 0xBC)
	r.end()
	return r.err
}

// EncodePlacementPolicy encodes kind 32's response (BD).
func EncodePlacementPolicy(p PlacementPolicy) []byte {
	w := newUCW(0xBD).uint(p.Revision).bytes(p.Digest).bytes(p.AuthorityPublicKey).str(p.AuthorityDomain)
	w.uint(uint64(p.TargetVoters)).uint(uint64(p.TargetRetainedLearners)).list(len(p.Eligible))
	for _, c := range p.Eligible {
		w.bytes(c.NodeID)
		if c.MemberSeed == nil {
			w.u8(nullFlag)
		} else {
			w.u8(0).memberSeed(*c.MemberSeed)
		}
	}
	w.bytesList(p.PinnedVoters).bytesList(p.Excluded)
	return w.uint(uint64(p.MinimumSeparatedVoterPairs)).uint(p.ReplacementGraceMs).bool(p.AllowHealthyRebalance).b
}

// DecodePlacementPolicy decodes kind 32's response (BD).
func DecodePlacementPolicy(b []byte) (PlacementPolicy, error) {
	r := newUCR(b, 0xBD)
	var p PlacementPolicy
	p.Revision = r.uint()
	p.Digest = r.fixed(32, "policy digest")
	p.AuthorityPublicKey = r.fixed(32, "authority public key")
	p.AuthorityDomain = r.str()
	p.TargetVoters = uint8(r.uintMax(255, "target voters"))
	p.TargetRetainedLearners = uint8(r.uintMax(255, "target retained learners"))
	n := r.list()
	var ids [][]byte
	for i := 0; i < n && r.err == nil; i++ {
		c := PlacementCandidate{NodeID: r.fixed(32, "eligible node id")}
		if r.present() {
			s := r.memberSeed()
			c.MemberSeed = &s
		}
		ids = append(ids, c.NodeID)
		p.Eligible = append(p.Eligible, c)
	}
	r.ascending(ids, "eligible")
	p.PinnedVoters = r.fixedList(32, "pinned voter")
	r.ascending(p.PinnedVoters, "pinned voters")
	p.Excluded = r.fixedList(32, "excluded node")
	r.ascending(p.Excluded, "excluded")
	p.MinimumSeparatedVoterPairs = uint8(r.uintMax(255, "minimum separated voter pairs"))
	p.ReplacementGraceMs = r.uint()
	p.AllowHealthyRebalance = r.bool()
	r.end()
	if r.err != nil {
		return PlacementPolicy{}, r.err
	}
	return p, nil
}

// EncodeProvisionRequest encodes kind 33's request (BE).
func EncodeProvisionRequest(v ProvisionRequest) []byte {
	return newUCW(0xBE).bytes(v.GroupID).bytes(v.PolicyDigest).optLogID(v.MembershipLogID).bytes(v.NodeID).b
}

// DecodeProvisionRequest decodes kind 33's request (BE).
func DecodeProvisionRequest(b []byte) (ProvisionRequest, error) {
	r := newUCR(b, 0xBE)
	var v ProvisionRequest
	v.GroupID = r.fixed(32, "group id")
	v.PolicyDigest = r.fixed(32, "policy digest")
	v.MembershipLogID = r.optLogID()
	v.NodeID = r.fixed(32, "node id")
	r.end()
	if r.err != nil {
		return ProvisionRequest{}, r.err
	}
	return v, nil
}
