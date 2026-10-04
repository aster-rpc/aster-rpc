package aster

import (
	"bytes"
	"encoding/hex"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"reflect"
	"testing"
)

// The D0-DE codec against conformance/vectors/coordination.json: every valid
// vector decodes to its JSON rendering and re-encodes to the same bytes, every
// invalid one is refused. Plain Go: runs with CGO_ENABLED=0.

type coordVectors struct {
	Tags []struct {
		Name string `json:"name"`
		Tag  int    `json:"tag"`
	} `json:"tags"`
	MutationOps []struct {
		Code int    `json:"code"`
		Name string `json:"name"`
	} `json:"mutation_ops"`
	ReadOps []struct {
		Code int    `json:"code"`
		Name string `json:"name"`
	} `json:"read_ops"`
	Valid []struct {
		Hex   string          `json:"hex"`
		Name  string          `json:"name"`
		Type  string          `json:"type"`
		Value json.RawMessage `json:"value"`
	} `json:"valid"`
	Invalid []struct {
		Hex  string `json:"hex"`
		Name string `json:"name"`
		Type string `json:"type"`
	} `json:"invalid"`
}

func loadCoordVectors(t testing.TB) coordVectors {
	t.Helper()
	data, err := os.ReadFile(filepath.Join("..", "..", "conformance", "vectors", "coordination.json"))
	if err != nil {
		t.Fatal(err)
	}
	var v coordVectors
	if err := json.Unmarshal(data, &v); err != nil {
		t.Fatal(err)
	}
	return v
}

func hx(b []byte) any {
	if b == nil {
		return nil
	}
	return hex.EncodeToString(b)
}

func hxs(l [][]byte) any {
	out := []any{}
	for _, b := range l {
		out = append(out, hex.EncodeToString(b))
	}
	return out
}

func jlog(l *LogID) any {
	if l == nil {
		return nil
	}
	return jm{"index": l.Index, "leader_node_id": hx(l.Leader), "term": l.Term}
}

func jres(k ResourceKey) any { return jm{"key": hx(k.Key), "namespace": k.Namespace} }

func jfence(f CoordinationFence) any {
	return jm{"epoch": f.Epoch, "group_id": hx(f.GroupID), "holder": hx(f.Holder), "resource": jres(f.Resource)}
}

func jsnap(s ResourceSnapshot) any {
	return jm{"acl_revision": s.ACLRevision, "epoch": s.Epoch, "group_id": hx(s.GroupID), "held": s.Held,
		"holder": hx(s.Holder), "lease_revision": s.LeaseRevision, "resource": jres(s.Resource), "ttl_ms": s.TTLMs,
		"value": hx(s.Value), "value_redacted": s.ValueRedacted, "value_revision": s.ValueRevision}
}

func jseeds(v []MemberSeed) any {
	out := []any{}
	for _, m := range v {
		out = append(out, jseed(m))
	}
	return out
}

func jseed(m MemberSeed) any {
	return jm{"node_id": hx(m.NodeID), "signed_join_permit": hx(m.SignedJoinPermit), "ticket": m.Ticket}
}

var mutationNames = map[MutationOp]string{1: "acquire", 2: "renew", 3: "compare_and_set", 4: "release",
	5: "release_with_value", 6: "revoke", 7: "set_grant", 8: "set_admin", 9: "change_membership", 10: "cancel_membership"}

var readNames = map[ReadOp]string{1: "read_linearizable", 2: "inspect_acl", 3: "inspect_admins", 4: "membership_status",
	5: "group_status", 6: "probe_client", 7: "audit_stream", 8: "recovery_export"}

func renderMutationRequest(m MutationRequest) any {
	out := jm{"op": mutationNames[m.Op]}
	switch m.Op {
	case MutateAcquire:
		out["resource"], out["ttl_ms"] = jres(m.Resource), m.TTLMs
	case MutateRenew, MutateRelease:
		out["fence"] = jfence(m.Fence)
	case MutateCompareAndSet:
		out["fence"], out["expected_value_revision"], out["new_value"] = jfence(m.Fence), m.ExpectedValueRevision, hx(m.Value)
	case MutateReleaseWithValue:
		out["fence"], out["expected_value_revision"], out["final_value"] = jfence(m.Fence), m.ExpectedValueRevision, hx(m.Value)
	case MutateRevoke:
		var e any
		if m.ExpectedEpoch != nil {
			e = *m.ExpectedEpoch
		}
		out["resource"], out["expected_epoch"] = jres(m.Resource), e
	case MutateSetGrant:
		out["resource"], out["target_node_id"], out["rights"], out["expected_acl_revision"] =
			jres(m.Resource), hx(m.TargetNodeID), m.Rights, m.ExpectedACLRevision
	case MutateSetAdmin:
		out["target_node_id"], out["enabled"], out["expected_admin_revision"] = hx(m.TargetNodeID), m.Enabled, m.ExpectedAdminRevision
	case MutateChangeMembership:
		out["operation_id"], out["expected_membership_log_id"], out["desired_voters"] = hx(m.OperationID), jlog(m.ExpectedMembershipLogID), hxs(m.DesiredVoters)
		out["retained_learners"], out["member_seeds"] = hxs(m.RetainedLearners), jseeds(m.MemberSeeds)
	case MutateCancelMembership:
		out["operation_id"], out["expected_phase_code"] = hx(m.OperationID), m.ExpectedPhaseCode
	}
	return out
}

func renderReadRequest(q ReadRequest) any {
	out := jm{"op": readNames[q.Op]}
	switch q.Op {
	case ReadLinearizable, ReadInspectACL:
		out["resource"] = jres(q.Resource)
	case ReadMembershipState:
		out["operation_id"] = hx(q.OperationID)
	case ReadProbeClient:
		out["target_node_id"], out["target_client_id"] = hx(q.TargetNodeID), hx(q.TargetClientID)
	case ReadAuditStream:
		out["after"], out["limit"] = jlog(q.After), q.Limit
	}
	return out
}

func coordCodecFor(typ string) *ucCodec {
	switch typ {
	case "MutationRequest":
		return &ucCodec{
			func(b []byte) (any, error) { m, err := DecodeMutationRequest(b); return renderMutationRequest(m), err },
			func(b []byte) ([]byte, error) {
				m, err := DecodeMutationRequest(b)
				if err != nil {
					return nil, err
				}
				return EncodeMutationRequest(m)
			}}
	case "ReadRequest":
		return &ucCodec{
			func(b []byte) (any, error) { q, err := DecodeReadRequest(b); return renderReadRequest(q), err },
			func(b []byte) ([]byte, error) {
				q, err := DecodeReadRequest(b)
				if err != nil {
					return nil, err
				}
				return EncodeReadRequest(q)
			}}
	case "MutationReply":
		return &ucCodec{
			func(b []byte) (any, error) {
				m, err := DecodeMutationReply(b)
				var snap any
				if m.Snapshot != nil {
					snap = jsnap(*m.Snapshot)
				}
				return jm{"acl_revision": m.ACLRevision, "admin_revision": m.AdminRevision, "client_id": hx(m.ClientID),
					"disposition_code": m.DispositionCode, "membership_log_id": jlog(m.MembershipLogID),
					"operation_id": hx(m.OperationID), "outcome_code": m.OutcomeCode, "reason_code": m.ReasonCode,
					"request_hash": hx(m.RequestHash), "sequence": m.Sequence, "snapshot": snap}, err
			},
			func(b []byte) ([]byte, error) { m, err := DecodeMutationReply(b); return EncodeMutationReply(m), err }}
	case "ReadReply":
		return &ucCodec{
			func(b []byte) (any, error) { s, err := DecodeReadReply(b); return jsnap(s), err },
			func(b []byte) ([]byte, error) { s, err := DecodeReadReply(b); return EncodeReadReply(s), err }}
	case "AclStatus":
		return &ucCodec{
			func(b []byte) (any, error) {
				a, err := DecodeAclStatus(b)
				grants := []any{}
				for _, g := range a.Grants {
					grants = append(grants, jm{"rights": g.Rights, "subject_node_id": hx(g.SubjectNodeID)})
				}
				return jm{"acl_revision": a.ACLRevision, "grants": grants, "resource": jres(a.Resource)}, err
			},
			func(b []byte) ([]byte, error) { a, err := DecodeAclStatus(b); return EncodeAclStatus(a), err }}
	case "AdminStatus":
		return &ucCodec{
			func(b []byte) (any, error) {
				a, err := DecodeAdminStatus(b)
				return jm{"admin_revision": a.AdminRevision, "administrators": hxs(a.Administrators)}, err
			},
			func(b []byte) ([]byte, error) { a, err := DecodeAdminStatus(b); return EncodeAdminStatus(a), err }}
	case "MembershipStatus":
		return &ucCodec{
			func(b []byte) (any, error) {
				m, err := DecodeMembershipStatus(b)
				return jm{"desired_voters": hxs(m.DesiredVoters), "error_code": m.ErrorCode,
					"expected_membership_log_id": jlog(m.ExpectedMembershipLogID),
					"observed_membership_log_id": jlog(m.ObservedMembershipLogID), "operation_id": hx(m.OperationID),
					"retained_learners": hxs(m.RetainedLearners), "status_code": m.StatusCode}, err
			},
			func(b []byte) ([]byte, error) {
				m, err := DecodeMembershipStatus(b)
				return EncodeMembershipStatus(m), err
			}}
	case "GroupStatus":
		return &ucCodec{
			func(b []byte) (any, error) {
				g, err := DecodeGroupStatus(b)
				var quorum, rec any
				if g.QuorumAvailable != nil {
					quorum = *g.QuorumAvailable
				}
				if rf := g.RecoveredFrom; rf != nil {
					rec = jm{"export_hash": hx(rf.ExportHash), "source_authority_domain": rf.SourceAuthorityDomain,
						"source_authority_public_key": hx(rf.SourceAuthorityPublicKey),
						"source_genesis_hash":         hx(rf.SourceGenesisHash), "source_group_id": hx(rf.SourceGroupID),
						"source_last_applied": jlog(rf.SourceLastApplied)}
				}
				return jm{"applied_log_id": jlog(g.AppliedLogID), "audit_floor": jlog(g.AuditFloor),
					"authority_domain": g.AuthorityDomain, "authority_public_key": hx(g.AuthorityPublicKey),
					"committed_log_id": jlog(g.CommittedLogID), "deployment_profile_code": g.DeploymentProfileCode,
					"fatal_code": g.FatalCode, "genesis_hash": hx(g.GenesisHash), "last_log_id": jlog(g.LastLogID),
					"leader_node_id": hx(g.LeaderNodeID), "learners": hxs(g.Learners), "lifecycle_code": g.LifecycleCode,
					"local_node_id": hx(g.LocalNodeID), "local_role_code": g.LocalRoleCode,
					"membership_joint": g.MembershipJoint, "membership_log_id": jlog(g.MembershipLogID),
					"pending_operation_id": hx(g.PendingOperationID), "quorum_available": quorum,
					"recovered_from": rec, "term": g.Term, "voters": hxs(g.Voters)}, err
			},
			func(b []byte) ([]byte, error) { g, err := DecodeGroupStatus(b); return EncodeGroupStatus(g), err }}
	case "ClientProbe":
		return &ucCodec{
			func(b []byte) (any, error) {
				p, err := DecodeClientProbe(b)
				return jm{"disposition_code": p.DispositionCode, "exists": p.Exists, "last_sequence": p.LastSequence,
					"request_hash": hx(p.RequestHash), "retired": p.Retired, "target_client_id": hx(p.TargetClientID),
					"target_node_id": hx(p.TargetNodeID)}, err
			},
			func(b []byte) ([]byte, error) { p, err := DecodeClientProbe(b); return EncodeClientProbe(p), err }}
	case "AuditRecords":
		return &ucCodec{
			func(b []byte) (any, error) {
				recs, err := DecodeAuditRecords(b)
				out := []any{}
				for _, a := range recs {
					var res any
					if a.Resource != nil {
						res = jres(*a.Resource)
					}
					l := a.LogID
					out = append(out, jm{"actor_node_id": hx(a.ActorNodeID), "epoch": a.Epoch, "log_id": jlog(&l),
						"operation_code": a.OperationCode, "resource": res, "result_code": a.ResultCode,
						"value_revision": a.ValueRevision})
				}
				return out, err
			},
			func(b []byte) ([]byte, error) { a, err := DecodeAuditRecords(b); return EncodeAuditRecords(a), err }}
	case "RecoveryExport":
		return &ucCodec{
			func(b []byte) (any, error) {
				e, err := DecodeRecoveryExport(b)
				return jm{"applied_log_id": jlog(e.AppliedLogID), "blake3_checksum": hx(e.Blake3Checksum),
					"body": hx(e.Body), "total_len": e.TotalLen}, err
			},
			func(b []byte) ([]byte, error) { e, err := DecodeRecoveryExport(b); return EncodeRecoveryExport(e), err }}
	case "RecoveryExportManifest":
		return &ucCodec{
			func(b []byte) (any, error) {
				e, err := DecodeRecoveryExportManifest(b)
				return jm{"applied_log_id": jlog(e.AppliedLogID), "blake3_checksum": hx(e.Blake3Checksum), "total_len": e.TotalLen}, err
			},
			func(b []byte) ([]byte, error) {
				e, err := DecodeRecoveryExportManifest(b)
				return EncodeRecoveryExportManifest(e), err
			}}
	case "RetireReply":
		return &ucCodec{
			func(b []byte) (any, error) {
				v, err := DecodeRetireReply(b)
				return jm{"outcome_code": v.OutcomeCode, "reason_code": v.ReasonCode}, err
			},
			func(b []byte) ([]byte, error) { v, err := DecodeRetireReply(b); return EncodeRetireReply(v), err }}
	case "CoordinationFence":
		return &ucCodec{
			func(b []byte) (any, error) { f, err := DecodeCoordinationFence(b); return jfence(f), err },
			func(b []byte) ([]byte, error) {
				f, err := DecodeCoordinationFence(b)
				return EncodeCoordinationFence(f), err
			}}
	case "PlacementReport":
		return &ucCodec{
			func(b []byte) (any, error) {
				p, err := DecodePlacementReport(b)
				var sep any
				if s := p.Separation; s != nil {
					sep = jm{"connected_pairs": s.ConnectedPairs, "diversity_proved": s.DiversityProved,
						"represented_clusters": s.RepresentedClusters, "separated_pairs": s.SeparatedPairs,
						"total_pairs": s.TotalPairs, "unknown_pairs": s.UnknownPairs}
				}
				return jm{"current_learners": hxs(p.CurrentLearners), "current_voters": hxs(p.CurrentVoters),
					"grace_crossed_members": hxs(p.GraceCrossed), "membership_log_id": jlog(p.MembershipLogID),
					"pending_operation_id": hx(p.PendingOperationID), "policy_digest": hx(p.PolicyDigest),
					"policy_revision": p.PolicyRevision, "reason": p.Reason, "selected_learners": hxs(p.SelectedLearners),
					"selected_voters": hxs(p.SelectedVoters), "separation": sep}, err
			},
			func(b []byte) ([]byte, error) {
				p, err := DecodePlacementReport(b)
				return EncodePlacementReport(p), err
			}}
	}
	return nil
}

func TestCoordinationCodec_ValidVectors(t *testing.T) {
	checked := 0
	for _, v := range loadCoordVectors(t).Valid {
		c := coordCodecFor(v.Type)
		if c == nil {
			t.Errorf("no codec for %s (%s)", v.Type, v.Name)
			continue
		}
		raw, _ := hex.DecodeString(v.Hex)
		got, err := c.render(raw)
		if err != nil {
			t.Errorf("%s: decode: %v", v.Name, err)
			continue
		}
		var want any
		if err := json.Unmarshal(v.Value, &want); err != nil {
			t.Fatal(err)
		}
		if !reflect.DeepEqual(normalize(t, got), want) {
			t.Errorf("%s: decoded\n  %v\nwant\n  %s", v.Name, normalize(t, got), v.Value)
		}
		re, err := c.round(raw)
		if err != nil || !bytes.Equal(re, raw) {
			t.Errorf("%s: re-encode %x (%v), want %x", v.Name, re, err, raw)
		}
		checked++
	}
	if checked != 40 {
		t.Errorf("checked %d valid vectors, want 40", checked)
	}
}

func TestCoordinationCodec_InvalidVectors(t *testing.T) {
	checked := 0
	for _, v := range loadCoordVectors(t).Invalid {
		c := coordCodecFor(v.Type)
		if c == nil {
			t.Fatalf("no codec for %s", v.Type)
		}
		raw, _ := hex.DecodeString(v.Hex)
		if _, err := c.render(raw); !errors.Is(err, ErrUCv1) {
			t.Errorf("%s (%s): err = %v, want ErrUCv1", v.Name, v.Type, err)
		}
		checked++
	}
	if checked != 22 {
		t.Errorf("checked %d invalid vectors, want 22", checked)
	}
}

func TestCoordinationCodec_TablesMatchTheSpec(t *testing.T) {
	d := loadCoordVectors(t)
	tags := map[string]int{"MutationRequest": TagMutationRequest, "ReadRequest": TagReadRequest, "MutationReply": TagMutationReply,
		"ReadReply": TagReadReply, "AclStatus": TagAclStatus, "AdminStatus": TagAdminStatus, "MembershipStatus": TagMembershipStatus,
		"GroupStatus": TagGroupStatus, "ClientProbe": TagClientProbe, "AuditRecords": TagAuditRecords,
		"RecoveryExport": TagRecoveryExport, "RecoveryExportManifest": TagRecoveryExportManifest,
		"RetireReply": TagRetireReply, "CoordinationFence": TagCoordinationFence, "PlacementReport": TagPlacementReport}
	for _, tg := range d.Tags {
		if tags[tg.Name] != tg.Tag {
			t.Errorf("tag %s = %#x, want %#x", tg.Name, tags[tg.Name], tg.Tag)
		}
	}
	if len(d.Tags) != len(tags) {
		t.Errorf("%d tags in the vectors, %d in the binding", len(d.Tags), len(tags))
	}
	for _, o := range d.MutationOps {
		if mutationNames[MutationOp(o.Code)] != o.Name {
			t.Errorf("mutation op %d = %q, want %q", o.Code, mutationNames[MutationOp(o.Code)], o.Name)
		}
	}
	for _, o := range d.ReadOps {
		if readNames[ReadOp(o.Code)] != o.Name {
			t.Errorf("read op %d = %q, want %q", o.Code, readNames[ReadOp(o.Code)], o.Name)
		}
	}
}

// The request encoders refuse what the C ABI would refuse.
func TestCoordinationCodec_RequestEncodersValidate(t *testing.T) {
	node := bytes.Repeat([]byte{1}, 32)
	good := MutationRequest{Op: MutateSetAdmin, TargetNodeID: node, Enabled: true}
	if _, err := EncodeMutationRequest(good); err != nil {
		t.Fatal(err)
	}
	bad := good
	bad.TargetNodeID = node[:31]
	if _, err := EncodeMutationRequest(bad); !errors.Is(err, ErrUCv1) {
		t.Errorf("31-byte node id: %v", err)
	}
	if _, err := EncodeMutationRequest(MutationRequest{Op: 0}); !errors.Is(err, ErrUCv1) {
		t.Errorf("op 0: %v", err)
	}
	if _, err := EncodeMutationRequest(MutationRequest{Op: MutateCancelMembership, OperationID: make([]byte, 15)}); !errors.Is(err, ErrUCv1) {
		t.Errorf("15-byte operation id: %v", err)
	}
	for _, limit := range []uint64{0, 10001} {
		if _, err := EncodeReadRequest(ReadRequest{Op: ReadAuditStream, Limit: limit}); !errors.Is(err, ErrUCv1) {
			t.Errorf("audit limit %d: %v", limit, err)
		}
	}
	if _, err := EncodeReadRequest(ReadRequest{Op: ReadAuditStream, Limit: 10000}); err != nil {
		t.Errorf("audit limit 10000: %v", err)
	}
	if _, err := EncodeReadRequest(ReadRequest{Op: ReadProbeClient, TargetNodeID: node, TargetClientID: make([]byte, 17)}); !errors.Is(err, ErrUCv1) {
		t.Errorf("17-byte client id: %v", err)
	}
}

// The placement messages BC-BF against the upcalls vectors (also covered by
// TestUCv1_ValidVectors once the codec is registered there).
func TestPlacementCodec_RoundTripAndOrdering(t *testing.T) {
	a, b := bytes.Repeat([]byte{1}, 32), bytes.Repeat([]byte{2}, 32)
	p := PlacementPolicy{Revision: 3, Digest: bytes.Repeat([]byte{9}, 32), AuthorityPublicKey: bytes.Repeat([]byte{8}, 32),
		AuthorityDomain: "example.org", TargetVoters: 3, TargetRetainedLearners: 1,
		Eligible:     []PlacementCandidate{{NodeID: a, MemberSeed: &MemberSeed{NodeID: a, Ticket: "t", SignedJoinPermit: []byte("p")}}, {NodeID: b}},
		PinnedVoters: [][]byte{a}, Excluded: [][]byte{b}, MinimumSeparatedVoterPairs: 1, ReplacementGraceMs: 5, AllowHealthyRebalance: true}
	enc := EncodePlacementPolicy(p)
	got, err := DecodePlacementPolicy(enc)
	if err != nil || !reflect.DeepEqual(got, p) {
		t.Fatalf("round trip: %+v, %v", got, err)
	}
	// Eligible out of order is refused.
	p.Eligible = []PlacementCandidate{{NodeID: b}, {NodeID: a}}
	if _, err := DecodePlacementPolicy(EncodePlacementPolicy(p)); !errors.Is(err, ErrUCv1) {
		t.Errorf("unsorted eligible: %v", err)
	}
	pr := ProvisionRequest{GroupID: bytes.Repeat([]byte{3}, 32), PolicyDigest: bytes.Repeat([]byte{4}, 32), NodeID: a}
	if back, err := DecodeProvisionRequest(EncodeProvisionRequest(pr)); err != nil || !reflect.DeepEqual(back, pr) {
		t.Errorf("provision request: %+v, %v", back, err)
	}
	if _, err := DecodeProvisionRequest(EncodeProvisionRequest(ProvisionRequest{GroupID: a[:5], PolicyDigest: a, NodeID: a})); !errors.Is(err, ErrUCv1) {
		t.Errorf("short group id: %v", err)
	}
}
