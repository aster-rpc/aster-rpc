package aster

// UCv1 tags D0-DE: the C ABI 4.4 coordination client's requests and results
// (docs/_internal/bindings/coordination-abi-design.md section 2; vectors in
// conformance/vectors/coordination.json). Plain Go, no cgo. Node ids are 32
// raw bytes, client / operation ids 16; the request encoders refuse a wrong
// length, so a malformed request never reaches the C ABI.

import "fmt"

// Tags of the coordination messages.
const (
	TagMutationRequest        = 0xD0
	TagReadRequest            = 0xD1
	TagMutationReply          = 0xD2
	TagReadReply              = 0xD3
	TagAclStatus              = 0xD4
	TagAdminStatus            = 0xD5
	TagMembershipStatus       = 0xD6
	TagGroupStatus            = 0xD7
	TagClientProbe            = 0xD8
	TagAuditRecords           = 0xD9
	TagRecoveryExport         = 0xDA
	TagRecoveryExportManifest = 0xDB
	TagRetireReply            = 0xDC
	TagCoordinationFence      = 0xDD
	TagPlacementReport        = 0xDE
)

// MutationOp selects a sequenced mutation (aster_coordination_mutation_op_t).
type MutationOp uint8

const (
	MutateAcquire          MutationOp = 1
	MutateRenew            MutationOp = 2
	MutateCompareAndSet    MutationOp = 3
	MutateRelease          MutationOp = 4
	MutateReleaseWithValue MutationOp = 5
	MutateRevoke           MutationOp = 6
	MutateSetGrant         MutationOp = 7
	MutateSetAdmin         MutationOp = 8
	MutateChangeMembership MutationOp = 9
	MutateCancelMembership MutationOp = 10
	mutateMax                         = MutateCancelMembership
	auditLimitMax                     = 10000
	nodeIDLen                         = 32
	clientIDLen                       = 16
)

// ReadOp selects a read (aster_coordination_read_op_t).
type ReadOp uint8

const (
	ReadLinearizable    ReadOp = 1
	ReadInspectACL      ReadOp = 2
	ReadInspectAdmins   ReadOp = 3
	ReadMembershipState ReadOp = 4
	ReadGroupStatus     ReadOp = 5
	ReadProbeClient     ReadOp = 6
	ReadAuditStream     ReadOp = 7
	ReadRecoveryExport  ReadOp = 8
)

// ResourceKey names a coordinated resource.
type ResourceKey struct {
	Namespace string
	Key       []byte
}

// CoordinationFence proves a holder's right to act on a resource at an epoch
// (UCv1 CoordFence, reply DD).
type CoordinationFence struct {
	GroupID  []byte // 32 bytes
	Resource ResourceKey
	Epoch    uint64
	Holder   []byte // 32 bytes
}

// MutationRequest is the D0 request. Op selects which fields are used:
// Acquire (Resource, TTLMs), Renew / Release (Fence), CompareAndSet (Fence,
// ExpectedValueRevision, Value = the new value), ReleaseWithValue (Fence,
// ExpectedValueRevision, Value = the final value), Revoke (Resource,
// ExpectedEpoch), SetGrant (Resource, TargetNodeID, Rights,
// ExpectedACLRevision), SetAdmin (TargetNodeID, Enabled,
// ExpectedAdminRevision), ChangeMembership (OperationID,
// ExpectedMembershipLogID, DesiredVoters, RetainedLearners, MemberSeeds),
// CancelMembership (OperationID, ExpectedPhaseCode).
type MutationRequest struct {
	Op                      MutationOp
	Resource                ResourceKey
	TTLMs                   uint64
	Fence                   CoordinationFence
	ExpectedValueRevision   uint64
	Value                   []byte
	ExpectedEpoch           *uint64
	TargetNodeID            []byte
	Rights                  uint64
	ExpectedACLRevision     uint64
	Enabled                 bool
	ExpectedAdminRevision   uint64
	OperationID             []byte // 16 bytes
	ExpectedMembershipLogID *LogID
	DesiredVoters           [][]byte
	RetainedLearners        [][]byte
	MemberSeeds             []MemberSeed
	ExpectedPhaseCode       uint32
}

// ReadRequest is the D1 request. Op selects which fields are used:
// Linearizable / InspectACL (Resource), MembershipState (OperationID, nil =
// the current operation), ProbeClient (TargetNodeID, TargetClientID),
// AuditStream (After, Limit 1..=10000).
type ReadRequest struct {
	Op             ReadOp
	Resource       ResourceKey
	OperationID    []byte // 16 bytes or nil
	TargetNodeID   []byte // 32 bytes
	TargetClientID []byte // 16 bytes
	After          *LogID
	Limit          uint64
}

// ResourceSnapshot is a linearizable view of one resource (D3).
type ResourceSnapshot struct {
	GroupID       []byte
	Resource      ResourceKey
	Epoch         uint64
	Holder        []byte // nil when free
	Held          bool
	TTLMs         uint64
	LeaseRevision uint64
	ValueRevision uint64
	ACLRevision   uint64
	Value         []byte // nil when absent
	ValueRedacted bool
}

// MutationReply is a resolved mutation (D2). A committed typed rejection
// (OutcomeCode 100) is a result, not an error.
type MutationReply struct {
	ClientID        []byte
	Sequence        uint64
	RequestHash     []byte
	DispositionCode uint32
	OutcomeCode     uint32
	ReasonCode      uint32
	Snapshot        *ResourceSnapshot
	ACLRevision     uint64
	AdminRevision   uint64
	OperationID     []byte
	MembershipLogID *LogID
}

// AclGrant is one subject's rights on a resource.
type AclGrant struct {
	SubjectNodeID []byte
	Rights        uint64
}

// AclStatus is D4.
type AclStatus struct {
	Resource    ResourceKey
	ACLRevision uint64
	Grants      []AclGrant
}

// AdminStatus is D5.
type AdminStatus struct {
	AdminRevision  uint64
	Administrators [][]byte
}

// MembershipStatus is D6.
type MembershipStatus struct {
	OperationID             []byte
	StatusCode              uint32
	ExpectedMembershipLogID *LogID
	ObservedMembershipLogID *LogID
	DesiredVoters           [][]byte
	RetainedLearners        [][]byte
	ErrorCode               uint32
}

// RecoveredFrom says which group this one was recovered from.
type RecoveredFrom struct {
	SourceGroupID            []byte
	SourceGenesisHash        []byte
	SourceAuthorityPublicKey []byte
	SourceAuthorityDomain    string
	ExportHash               []byte
	SourceLastApplied        *LogID
}

// GroupStatus is D7.
type GroupStatus struct {
	LifecycleCode         uint32
	LocalNodeID           []byte
	LocalRoleCode         uint32
	LeaderNodeID          []byte // nil when unknown
	Term                  uint64
	LastLogID             *LogID
	CommittedLogID        *LogID
	AppliedLogID          *LogID
	DeploymentProfileCode uint32
	MembershipLogID       *LogID
	MembershipJoint       bool
	Voters                [][]byte
	Learners              [][]byte
	PendingOperationID    []byte
	QuorumAvailable       *bool
	FatalCode             uint32
	AuditFloor            *LogID
	GenesisHash           []byte
	AuthorityPublicKey    []byte
	AuthorityDomain       string
	RecoveredFrom         *RecoveredFrom
}

// ClientProbe is D8.
type ClientProbe struct {
	TargetNodeID    []byte
	TargetClientID  []byte
	Exists          bool
	LastSequence    uint64
	RequestHash     []byte
	DispositionCode uint32
	Retired         bool
}

// AuditRecord is one entry of D9.
type AuditRecord struct {
	LogID         LogID
	ActorNodeID   []byte // nil when absent
	OperationCode uint32
	Resource      *ResourceKey
	ResultCode    uint32
	Epoch         uint64
	ValueRevision uint64
}

// RecoveryExportManifest is DB (and the head of DA).
type RecoveryExportManifest struct {
	TotalLen       uint64
	Blake3Checksum []byte
	AppliedLogID   *LogID
}

// RecoveryExport is DA.
type RecoveryExport struct {
	RecoveryExportManifest
	Body []byte
}

// RetireReply is DC.
type RetireReply struct {
	OutcomeCode uint32
	ReasonCode  uint32
}

// PlacementSeparation is the separation evidence of a placement report.
type PlacementSeparation struct {
	SeparatedPairs      uint8
	ConnectedPairs      uint8
	UnknownPairs        uint8
	TotalPairs          uint8
	RepresentedClusters uint8
	DiversityProved     bool
}

// PlacementReport is DE: what one reconciliation pass decided and did.
type PlacementReport struct {
	PolicyRevision     uint64
	PolicyDigest       []byte // 32 bytes
	MembershipLogID    *LogID
	CurrentVoters      [][]byte
	CurrentLearners    [][]byte
	SelectedVoters     [][]byte
	SelectedLearners   [][]byte
	GraceCrossed       [][]byte
	Separation         *PlacementSeparation
	PendingOperationID []byte
	Reason             string
}

// ─── shared structures ───────────────────────────────────────────────────

func (w *ucw) resourceKey(k ResourceKey) *ucw { return w.str(k.Namespace).bytes(k.Key) }

func (r *ucr) resourceKey() ResourceKey {
	var k ResourceKey
	k.Namespace = r.str()
	k.Key = r.bytes()
	return k
}

func (w *ucw) coordFence(f CoordinationFence) *ucw {
	return w.bytes(f.GroupID).resourceKey(f.Resource).uint(f.Epoch).bytes(f.Holder)
}

func (r *ucr) coordFence() CoordinationFence {
	var f CoordinationFence
	f.GroupID = r.fixed(32, "fence group id")
	f.Resource = r.resourceKey()
	f.Epoch = r.uint()
	f.Holder = r.fixed(32, "fence holder")
	return f
}

func (w *ucw) resSnapshot(s ResourceSnapshot) *ucw {
	return w.bytes(s.GroupID).resourceKey(s.Resource).uint(s.Epoch).optBytes(s.Holder).bool(s.Held).uint(s.TTLMs).
		uint(s.LeaseRevision).uint(s.ValueRevision).uint(s.ACLRevision).optBytes(s.Value).bool(s.ValueRedacted)
}

func (r *ucr) resSnapshot() ResourceSnapshot {
	var s ResourceSnapshot
	s.GroupID = r.bytes()
	s.Resource = r.resourceKey()
	s.Epoch = r.uint()
	s.Holder = r.optBytes()
	s.Held = r.bool()
	s.TTLMs = r.uint()
	s.LeaseRevision = r.uint()
	s.ValueRevision = r.uint()
	s.ACLRevision = r.uint()
	s.Value = r.optBytes()
	s.ValueRedacted = r.bool()
	return s
}

func (w *ucw) code(v uint32) *ucw { return w.uint(uint64(v)) }

func (w *ucw) optUintPtr(v *uint64) *ucw { return w.optUint(v) }

func (w *ucw) memberSeeds(v []MemberSeed) *ucw {
	w.list(len(v))
	for _, m := range v {
		w.memberSeed(m)
	}
	return w
}

// ─── requests ────────────────────────────────────────────────────────────

// EncodeMutationRequest encodes D0, refusing an unknown op or a wrong id length.
func EncodeMutationRequest(m MutationRequest) ([]byte, error) {
	w := newUCW(TagMutationRequest).u8(uint8(m.Op))
	switch m.Op {
	case MutateAcquire:
		w.resourceKey(m.Resource).uint(m.TTLMs)
	case MutateRenew, MutateRelease:
		w.coordFence(m.Fence)
	case MutateCompareAndSet, MutateReleaseWithValue:
		w.coordFence(m.Fence).uint(m.ExpectedValueRevision).bytes(m.Value)
	case MutateRevoke:
		w.resourceKey(m.Resource).optUint(m.ExpectedEpoch)
	case MutateSetGrant:
		w.resourceKey(m.Resource).bytes(m.TargetNodeID).uint(m.Rights).uint(m.ExpectedACLRevision)
	case MutateSetAdmin:
		w.bytes(m.TargetNodeID).bool(m.Enabled).uint(m.ExpectedAdminRevision)
	case MutateChangeMembership:
		w.bytes(m.OperationID).optLogID(m.ExpectedMembershipLogID).bytesList(m.DesiredVoters).
			bytesList(m.RetainedLearners).memberSeeds(m.MemberSeeds)
	case MutateCancelMembership:
		w.bytes(m.OperationID).code(m.ExpectedPhaseCode)
	default:
		return nil, &UCv1Error{Msg: fmt.Sprintf("unknown mutation op %d", m.Op)}
	}
	// Validate by decoding what was written: one rule set for both directions.
	if _, err := DecodeMutationRequest(w.b); err != nil {
		return nil, err
	}
	return w.b, nil
}

// DecodeMutationRequest decodes D0.
func DecodeMutationRequest(b []byte) (MutationRequest, error) {
	r := newUCR(b, TagMutationRequest)
	m := MutationRequest{Op: MutationOp(r.u8())}
	switch m.Op {
	case MutateAcquire:
		m.Resource = r.resourceKey()
		m.TTLMs = r.uint()
	case MutateRenew, MutateRelease:
		m.Fence = r.coordFence()
	case MutateCompareAndSet, MutateReleaseWithValue:
		m.Fence = r.coordFence()
		m.ExpectedValueRevision = r.uint()
		m.Value = r.bytes()
	case MutateRevoke:
		m.Resource = r.resourceKey()
		if r.present() {
			v := r.uint()
			m.ExpectedEpoch = &v
		}
	case MutateSetGrant:
		m.Resource = r.resourceKey()
		m.TargetNodeID = r.fixed(nodeIDLen, "target node id")
		m.Rights = r.uint()
		m.ExpectedACLRevision = r.uint()
	case MutateSetAdmin:
		m.TargetNodeID = r.fixed(nodeIDLen, "target node id")
		m.Enabled = r.bool()
		m.ExpectedAdminRevision = r.uint()
	case MutateChangeMembership:
		m.OperationID = r.fixed(clientIDLen, "operation id")
		m.ExpectedMembershipLogID = r.optLogID()
		m.DesiredVoters = r.fixedList(nodeIDLen, "desired voter")
		m.RetainedLearners = r.fixedList(nodeIDLen, "retained learner")
		n := r.list()
		for i := 0; i < n && r.err == nil; i++ {
			m.MemberSeeds = append(m.MemberSeeds, r.memberSeed())
		}
	case MutateCancelMembership:
		m.OperationID = r.fixed(clientIDLen, "operation id")
		m.ExpectedPhaseCode = r.code()
	default:
		r.fail("unknown mutation op %d", uint8(m.Op))
	}
	r.end()
	if r.err != nil {
		return MutationRequest{}, r.err
	}
	return m, nil
}

// EncodeReadRequest encodes D1, refusing an unknown op, a wrong id length or
// an audit limit outside 1..=10000.
func EncodeReadRequest(q ReadRequest) ([]byte, error) {
	w := newUCW(TagReadRequest).u8(uint8(q.Op))
	switch q.Op {
	case ReadLinearizable, ReadInspectACL:
		w.resourceKey(q.Resource)
	case ReadInspectAdmins, ReadGroupStatus, ReadRecoveryExport:
	case ReadMembershipState:
		w.optBytes(q.OperationID)
	case ReadProbeClient:
		w.bytes(q.TargetNodeID).bytes(q.TargetClientID)
	case ReadAuditStream:
		w.optLogID(q.After).uint(q.Limit)
	default:
		return nil, &UCv1Error{Msg: fmt.Sprintf("unknown read op %d", q.Op)}
	}
	if _, err := DecodeReadRequest(w.b); err != nil {
		return nil, err
	}
	return w.b, nil
}

// DecodeReadRequest decodes D1.
func DecodeReadRequest(b []byte) (ReadRequest, error) {
	r := newUCR(b, TagReadRequest)
	q := ReadRequest{Op: ReadOp(r.u8())}
	switch q.Op {
	case ReadLinearizable, ReadInspectACL:
		q.Resource = r.resourceKey()
	case ReadInspectAdmins, ReadGroupStatus, ReadRecoveryExport:
	case ReadMembershipState:
		if r.present() {
			q.OperationID = r.fixed(clientIDLen, "operation id")
		}
	case ReadProbeClient:
		q.TargetNodeID = r.fixed(nodeIDLen, "target node id")
		q.TargetClientID = r.fixed(clientIDLen, "target client id")
	case ReadAuditStream:
		q.After = r.optLogID()
		q.Limit = r.uint()
		if r.err == nil && (q.Limit < 1 || q.Limit > auditLimitMax) {
			r.fail("audit limit %d outside 1..=%d", q.Limit, auditLimitMax)
		}
	default:
		r.fail("unknown read op %d", uint8(q.Op))
	}
	r.end()
	if r.err != nil {
		return ReadRequest{}, r.err
	}
	return q, nil
}

// ─── results ─────────────────────────────────────────────────────────────

// EncodeMutationReply encodes D2.
func EncodeMutationReply(m MutationReply) []byte {
	w := newUCW(TagMutationReply).bytes(m.ClientID).uint(m.Sequence).bytes(m.RequestHash).code(m.DispositionCode).
		code(m.OutcomeCode).code(m.ReasonCode)
	if m.Snapshot == nil {
		w.u8(nullFlag)
	} else {
		w.u8(0).resSnapshot(*m.Snapshot)
	}
	return w.uint(m.ACLRevision).uint(m.AdminRevision).optBytes(m.OperationID).optLogID(m.MembershipLogID).b
}

// DecodeMutationReply decodes D2.
func DecodeMutationReply(b []byte) (MutationReply, error) {
	r := newUCR(b, TagMutationReply)
	var m MutationReply
	m.ClientID = r.bytes()
	m.Sequence = r.uint()
	m.RequestHash = r.bytes()
	m.DispositionCode = r.code()
	m.OutcomeCode = r.code()
	m.ReasonCode = r.code()
	if r.present() {
		s := r.resSnapshot()
		m.Snapshot = &s
	}
	m.ACLRevision = r.uint()
	m.AdminRevision = r.uint()
	m.OperationID = r.optBytes()
	m.MembershipLogID = r.optLogID()
	r.end()
	if r.err != nil {
		return MutationReply{}, r.err
	}
	return m, nil
}

// EncodeReadReply encodes D3.
func EncodeReadReply(s ResourceSnapshot) []byte { return newUCW(TagReadReply).resSnapshot(s).b }

// DecodeReadReply decodes D3.
func DecodeReadReply(b []byte) (ResourceSnapshot, error) {
	r := newUCR(b, TagReadReply)
	s := r.resSnapshot()
	r.end()
	return s, r.err
}

// EncodeAclStatus encodes D4.
func EncodeAclStatus(a AclStatus) []byte {
	w := newUCW(TagAclStatus).resourceKey(a.Resource).uint(a.ACLRevision).list(len(a.Grants))
	for _, g := range a.Grants {
		w.bytes(g.SubjectNodeID).uint(g.Rights)
	}
	return w.b
}

// DecodeAclStatus decodes D4.
func DecodeAclStatus(b []byte) (AclStatus, error) {
	r := newUCR(b, TagAclStatus)
	var a AclStatus
	a.Resource = r.resourceKey()
	a.ACLRevision = r.uint()
	n := r.list()
	for i := 0; i < n && r.err == nil; i++ {
		a.Grants = append(a.Grants, AclGrant{SubjectNodeID: r.bytes(), Rights: r.uint()})
	}
	r.end()
	if r.err != nil {
		return AclStatus{}, r.err
	}
	return a, nil
}

// EncodeAdminStatus encodes D5.
func EncodeAdminStatus(a AdminStatus) []byte {
	return newUCW(TagAdminStatus).uint(a.AdminRevision).bytesList(a.Administrators).b
}

// DecodeAdminStatus decodes D5.
func DecodeAdminStatus(b []byte) (AdminStatus, error) {
	r := newUCR(b, TagAdminStatus)
	a := AdminStatus{AdminRevision: r.uint(), Administrators: r.bytesList()}
	r.end()
	return a, r.err
}

// EncodeMembershipStatus encodes D6.
func EncodeMembershipStatus(m MembershipStatus) []byte {
	return newUCW(TagMembershipStatus).optBytes(m.OperationID).code(m.StatusCode).optLogID(m.ExpectedMembershipLogID).
		optLogID(m.ObservedMembershipLogID).bytesList(m.DesiredVoters).bytesList(m.RetainedLearners).code(m.ErrorCode).b
}

// DecodeMembershipStatus decodes D6.
func DecodeMembershipStatus(b []byte) (MembershipStatus, error) {
	r := newUCR(b, TagMembershipStatus)
	var m MembershipStatus
	m.OperationID = r.optBytes()
	m.StatusCode = r.code()
	m.ExpectedMembershipLogID = r.optLogID()
	m.ObservedMembershipLogID = r.optLogID()
	m.DesiredVoters = r.bytesList()
	m.RetainedLearners = r.bytesList()
	m.ErrorCode = r.code()
	r.end()
	if r.err != nil {
		return MembershipStatus{}, r.err
	}
	return m, nil
}

// EncodeGroupStatus encodes D7.
func EncodeGroupStatus(g GroupStatus) []byte {
	w := newUCW(TagGroupStatus).code(g.LifecycleCode).bytes(g.LocalNodeID).code(g.LocalRoleCode).optBytes(g.LeaderNodeID).
		uint(g.Term).optLogID(g.LastLogID).optLogID(g.CommittedLogID).optLogID(g.AppliedLogID).code(g.DeploymentProfileCode).
		optLogID(g.MembershipLogID).bool(g.MembershipJoint).bytesList(g.Voters).bytesList(g.Learners).
		optBytes(g.PendingOperationID)
	if g.QuorumAvailable == nil {
		w.u8(nullFlag)
	} else {
		w.u8(0).bool(*g.QuorumAvailable)
	}
	w.code(g.FatalCode).optLogID(g.AuditFloor).bytes(g.GenesisHash).bytes(g.AuthorityPublicKey).str(g.AuthorityDomain)
	if g.RecoveredFrom == nil {
		return w.u8(nullFlag).b
	}
	rf := g.RecoveredFrom
	return w.u8(0).bytes(rf.SourceGroupID).bytes(rf.SourceGenesisHash).bytes(rf.SourceAuthorityPublicKey).
		str(rf.SourceAuthorityDomain).bytes(rf.ExportHash).optLogID(rf.SourceLastApplied).b
}

// DecodeGroupStatus decodes D7.
func DecodeGroupStatus(b []byte) (GroupStatus, error) {
	r := newUCR(b, TagGroupStatus)
	var g GroupStatus
	g.LifecycleCode = r.code()
	g.LocalNodeID = r.bytes()
	g.LocalRoleCode = r.code()
	g.LeaderNodeID = r.optBytes()
	g.Term = r.uint()
	g.LastLogID = r.optLogID()
	g.CommittedLogID = r.optLogID()
	g.AppliedLogID = r.optLogID()
	g.DeploymentProfileCode = r.code()
	g.MembershipLogID = r.optLogID()
	g.MembershipJoint = r.bool()
	g.Voters = r.bytesList()
	g.Learners = r.bytesList()
	g.PendingOperationID = r.optBytes()
	if r.present() {
		q := r.bool()
		g.QuorumAvailable = &q
	}
	g.FatalCode = r.code()
	g.AuditFloor = r.optLogID()
	g.GenesisHash = r.bytes()
	g.AuthorityPublicKey = r.bytes()
	g.AuthorityDomain = r.str()
	if r.present() {
		rf := RecoveredFrom{}
		rf.SourceGroupID = r.bytes()
		rf.SourceGenesisHash = r.bytes()
		rf.SourceAuthorityPublicKey = r.bytes()
		rf.SourceAuthorityDomain = r.str()
		rf.ExportHash = r.bytes()
		rf.SourceLastApplied = r.optLogID()
		g.RecoveredFrom = &rf
	}
	r.end()
	if r.err != nil {
		return GroupStatus{}, r.err
	}
	return g, nil
}

// EncodeClientProbe encodes D8.
func EncodeClientProbe(p ClientProbe) []byte {
	return newUCW(TagClientProbe).bytes(p.TargetNodeID).bytes(p.TargetClientID).bool(p.Exists).uint(p.LastSequence).
		bytes(p.RequestHash).code(p.DispositionCode).bool(p.Retired).b
}

// DecodeClientProbe decodes D8.
func DecodeClientProbe(b []byte) (ClientProbe, error) {
	r := newUCR(b, TagClientProbe)
	var p ClientProbe
	p.TargetNodeID = r.bytes()
	p.TargetClientID = r.bytes()
	p.Exists = r.bool()
	p.LastSequence = r.uint()
	p.RequestHash = r.bytes()
	p.DispositionCode = r.code()
	p.Retired = r.bool()
	r.end()
	if r.err != nil {
		return ClientProbe{}, r.err
	}
	return p, nil
}

// EncodeAuditRecords encodes D9.
func EncodeAuditRecords(v []AuditRecord) []byte {
	w := newUCW(TagAuditRecords).list(len(v))
	for _, a := range v {
		w.logID(a.LogID).optBytes(a.ActorNodeID).code(a.OperationCode)
		if a.Resource == nil {
			w.u8(nullFlag)
		} else {
			w.u8(0).resourceKey(*a.Resource)
		}
		w.code(a.ResultCode).uint(a.Epoch).uint(a.ValueRevision)
	}
	return w.b
}

// DecodeAuditRecords decodes D9.
func DecodeAuditRecords(b []byte) ([]AuditRecord, error) {
	r := newUCR(b, TagAuditRecords)
	n := r.list()
	out := make([]AuditRecord, 0, n)
	for i := 0; i < n && r.err == nil; i++ {
		var a AuditRecord
		a.LogID = r.logID()
		a.ActorNodeID = r.optBytes()
		a.OperationCode = r.code()
		if r.present() {
			k := r.resourceKey()
			a.Resource = &k
		}
		a.ResultCode = r.code()
		a.Epoch = r.uint()
		a.ValueRevision = r.uint()
		out = append(out, a)
	}
	r.end()
	if r.err != nil {
		return nil, r.err
	}
	return out, nil
}

// EncodeRecoveryExport encodes DA.
func EncodeRecoveryExport(e RecoveryExport) []byte {
	return newUCW(TagRecoveryExport).uint(e.TotalLen).bytes(e.Blake3Checksum).optLogID(e.AppliedLogID).bytes(e.Body).b
}

// DecodeRecoveryExport decodes DA.
func DecodeRecoveryExport(b []byte) (RecoveryExport, error) {
	r := newUCR(b, TagRecoveryExport)
	var e RecoveryExport
	e.TotalLen = r.uint()
	e.Blake3Checksum = r.bytes()
	e.AppliedLogID = r.optLogID()
	e.Body = r.bytes()
	r.end()
	if r.err != nil {
		return RecoveryExport{}, r.err
	}
	return e, nil
}

// EncodeRecoveryExportManifest encodes DB.
func EncodeRecoveryExportManifest(e RecoveryExportManifest) []byte {
	return newUCW(TagRecoveryExportManifest).uint(e.TotalLen).bytes(e.Blake3Checksum).optLogID(e.AppliedLogID).b
}

// DecodeRecoveryExportManifest decodes DB.
func DecodeRecoveryExportManifest(b []byte) (RecoveryExportManifest, error) {
	r := newUCR(b, TagRecoveryExportManifest)
	var e RecoveryExportManifest
	e.TotalLen = r.uint()
	e.Blake3Checksum = r.bytes()
	e.AppliedLogID = r.optLogID()
	r.end()
	if r.err != nil {
		return RecoveryExportManifest{}, r.err
	}
	return e, nil
}

// EncodeRetireReply encodes DC.
func EncodeRetireReply(v RetireReply) []byte {
	return newUCW(TagRetireReply).code(v.OutcomeCode).code(v.ReasonCode).b
}

// DecodeRetireReply decodes DC.
func DecodeRetireReply(b []byte) (RetireReply, error) {
	r := newUCR(b, TagRetireReply)
	v := RetireReply{OutcomeCode: r.code(), ReasonCode: r.code()}
	r.end()
	return v, r.err
}

// EncodeCoordinationFence encodes DD.
func EncodeCoordinationFence(f CoordinationFence) []byte {
	return newUCW(TagCoordinationFence).coordFence(f).b
}

// DecodeCoordinationFence decodes DD.
func DecodeCoordinationFence(b []byte) (CoordinationFence, error) {
	r := newUCR(b, TagCoordinationFence)
	f := r.coordFence()
	r.end()
	return f, r.err
}

// EncodePlacementReport encodes DE.
func EncodePlacementReport(p PlacementReport) []byte {
	w := newUCW(TagPlacementReport).uint(p.PolicyRevision).bytes(p.PolicyDigest).optLogID(p.MembershipLogID).
		bytesList(p.CurrentVoters).bytesList(p.CurrentLearners).bytesList(p.SelectedVoters).
		bytesList(p.SelectedLearners).bytesList(p.GraceCrossed)
	if p.Separation == nil {
		w.u8(nullFlag)
	} else {
		s := p.Separation
		w.u8(0).uint(uint64(s.SeparatedPairs)).uint(uint64(s.ConnectedPairs)).uint(uint64(s.UnknownPairs)).
			uint(uint64(s.TotalPairs)).uint(uint64(s.RepresentedClusters)).bool(s.DiversityProved)
	}
	return w.optBytes(p.PendingOperationID).str(p.Reason).b
}

// DecodePlacementReport decodes DE.
func DecodePlacementReport(b []byte) (PlacementReport, error) {
	r := newUCR(b, TagPlacementReport)
	var p PlacementReport
	p.PolicyRevision = r.uint()
	p.PolicyDigest = r.fixed(32, "policy digest")
	p.MembershipLogID = r.optLogID()
	p.CurrentVoters = r.bytesList()
	p.CurrentLearners = r.bytesList()
	p.SelectedVoters = r.bytesList()
	p.SelectedLearners = r.bytesList()
	p.GraceCrossed = r.bytesList()
	if r.present() {
		s := PlacementSeparation{}
		s.SeparatedPairs = uint8(r.uintMax(255, "separated pairs"))
		s.ConnectedPairs = uint8(r.uintMax(255, "connected pairs"))
		s.UnknownPairs = uint8(r.uintMax(255, "unknown pairs"))
		s.TotalPairs = uint8(r.uintMax(255, "total pairs"))
		s.RepresentedClusters = uint8(r.uintMax(255, "represented clusters"))
		s.DiversityProved = r.bool()
		p.Separation = &s
	}
	p.PendingOperationID = r.optBytes()
	p.Reason = r.str()
	r.end()
	if r.err != nil {
		return PlacementReport{}, r.err
	}
	return p, nil
}
