//go:build cgo

package aster

import (
	"bufio"
	"bytes"
	"context"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

// C ABI 4.4 against a REAL three-voter coordination group over loopback Iroh.
// The voters are Rust-only (plan D7), so the group runs in a child process:
// aster-coordination-fixture (bindings/typescript/native/fixtures), built with
//
//	cargo build -p aster-transport-napi --features coordination-fixture \
//	    --bin aster-coordination-fixture
//
// or point ASTER_COORDINATION_FIXTURE at a binary. Without one these tests
// skip with that notice (the CI job that runs the Go suite builds it).

const coordNS = "go.coordination"

type liveInfo struct {
	GroupID      string `json:"groupId"`
	GenesisHash  string `json:"genesisHash"`
	Domain       string `json:"authorityDomain"`
	AuthorityKey string `json:"authorityKey"`
	RootSecret   string `json:"rootSecret"`
	Voters       []struct {
		NodeID string `json:"nodeId"`
		Ticket string `json:"ticket"`
	} `json:"voters"`
}

type liveGroup struct {
	info    liveInfo
	groupID []byte
	root    []byte // root secret
	auth    []byte // authority public key
	cmd     *exec.Cmd
	stdin   io.WriteCloser
	dir     string
}

var (
	liveOnce  sync.Once
	liveG     *liveGroup
	liveErr   error
	liveSkip  string
	liveNodes sync.Map // name -> *Node, shared across tests
)

func fixtureBinary() string {
	if p := os.Getenv("ASTER_COORDINATION_FIXTURE"); p != "" {
		return p
	}
	target := os.Getenv("CARGO_TARGET_DIR")
	if target == "" {
		target = filepath.Join("..", "..", "target")
	}
	for _, profile := range []string{"debug", "release"} {
		p := filepath.Join(target, profile, "aster-coordination-fixture")
		if st, err := os.Stat(p); err == nil && !st.IsDir() {
			return p
		}
	}
	return ""
}

func TestMain(m *testing.M) {
	code := m.Run()
	liveNodes.Range(func(_, n any) bool { _ = n.(*Node).Close(); return true })
	if liveG != nil {
		liveG.stdin.Close()
		done := make(chan struct{})
		go func() { _ = liveG.cmd.Wait(); close(done) }()
		select {
		case <-done:
		case <-time.After(15 * time.Second):
			_ = liveG.cmd.Process.Kill()
		}
		_ = os.RemoveAll(liveG.dir)
	}
	os.Exit(code)
}

// group starts (once) and returns the voter group. Call it BEFORE leakCheck so
// the fixture's goroutines are part of the baseline.
func group(t *testing.T) *liveGroup {
	t.Helper()
	liveOnce.Do(func() {
		bin := fixtureBinary()
		if bin == "" {
			liveSkip = "no aster-coordination-fixture binary (cargo build -p aster-transport-napi --features " +
				"coordination-fixture --bin aster-coordination-fixture, or set ASTER_COORDINATION_FIXTURE)"
			return
		}
		dir, err := os.MkdirTemp("", "aster-go-coord-")
		if err != nil {
			liveErr = err
			return
		}
		cmd := exec.Command(bin, filepath.Join(dir, "voters"), "3", "0xC7")
		stdin, _ := cmd.StdinPipe()
		stdout, _ := cmd.StdoutPipe()
		cmd.Stderr = os.Stderr
		if err := cmd.Start(); err != nil {
			liveErr = err
			return
		}
		lineCh := make(chan string, 1)
		go func() {
			line, _ := bufio.NewReader(stdout).ReadString('\n')
			lineCh <- line
		}()
		var line string
		select {
		case line = <-lineCh:
		case <-time.After(90 * time.Second):
			_ = cmd.Process.Kill()
			liveErr = errors.New("the voter group did not come up in 90 s")
			return
		}
		var info liveInfo
		if err := json.Unmarshal([]byte(line), &info); err != nil {
			_ = cmd.Process.Kill()
			liveErr = fmt.Errorf("fixture output %q: %w", line, err)
			return
		}
		g := &liveGroup{info: info, cmd: cmd, stdin: stdin, dir: dir}
		g.groupID, _ = hex.DecodeString(info.GroupID)
		g.root, _ = hex.DecodeString(info.RootSecret)
		g.auth, _ = hex.DecodeString(info.AuthorityKey)
		for _, name := range []string{"root", "worker", "contender"} {
			cfg := testCfg()
			cfg.RootPubkey = g.auth
			if name == "root" {
				cfg.SecretKey = g.root
			} else {
				cfg.SecretKey = bytes.Repeat([]byte{byte(len(name)), name[0], 0x42}, 11)[:32]
			}
			n, err := StartNode(context.Background(), cfg, NodeOptions{})
			if err != nil {
				liveErr = err
				return
			}
			liveNodes.Store(name, n)
		}
		liveG = g
	})
	if liveSkip != "" {
		t.Skip(liveSkip)
	}
	if liveErr != nil {
		t.Fatal(liveErr)
	}
	return liveG
}

func (g *liveGroup) tickets() []string {
	var out []string
	for _, v := range g.info.Voters {
		out = append(out, v.Ticket)
	}
	return out
}

// actor returns the shared node for name (a fresh identity, or the ROOT
// identity for "root"); started once and reused by every test.
func (g *liveGroup) actor(t *testing.T, name string) *Node {
	t.Helper()
	if n, ok := liveNodes.Load(name); ok {
		return n.(*Node)
	}
	cfg := testCfg()
	cfg.RootPubkey = g.auth
	if name == "root" {
		cfg.SecretKey = g.root
	} else {
		cfg.SecretKey = bytes.Repeat([]byte{byte(len(name)), name[0], 0x42}, 11)[:32]
	}
	n, err := StartNode(t.Context(), cfg, NodeOptions{})
	if err != nil {
		t.Fatal(err)
	}
	liveNodes.Store(name, n)
	return n
}

// open opens a client and drains whatever an earlier test left unresolved or
// unacknowledged in the journal.
func (g *liveGroup) open(t *testing.T, node *Node, journal string) *CoordinationClient {
	t.Helper()
	c := g.reopen(t, node, journal)
	ctx := coordCtx(t)
	for i := 0; i < 3; i++ {
		r, err := c.Resume(ctx)
		if err == nil && r == nil {
			r, err = c.Unacknowledged(ctx)
		}
		if err != nil {
			t.Fatalf("draining the journal: %v", err)
		}
		if r == nil {
			return c
		}
		if err := r.Acknowledge(ctx); err != nil {
			t.Fatalf("draining the journal: %v", err)
		}
	}
	return c
}

// reopen opens a client WITHOUT draining (redelivery tests).
func (g *liveGroup) reopen(t *testing.T, node *Node, journal string) *CoordinationClient {
	t.Helper()
	ctx, cancel := context.WithTimeout(t.Context(), 60*time.Second)
	defer cancel()
	c, err := OpenCoordinationClient(ctx, node, CoordinationOptions{GroupID: g.groupID, SeedTickets: g.tickets(), JournalDir: journal})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = c.Close() })
	return c
}

// journal is the actor's persistent journal directory. Every distinct
// journal is a new client id (a retained session) at the group, which caps
// them per actor, so the tests reuse one journal per actor; each test closes
// its client and leaves the journal acknowledged.
func (g *liveGroup) journal(t *testing.T, actor string) string {
	_ = os.MkdirAll(filepath.Join(g.dir, "journals"), 0o755)
	return filepath.Join(g.dir, "journals", actor)
}

func res(key string) ResourceKey { return ResourceKey{Namespace: coordNS, Key: []byte(key)} }

func coordCtx(t *testing.T) context.Context {
	t.Helper()
	ctx, cancel := context.WithTimeout(t.Context(), 90*time.Second)
	t.Cleanup(cancel)
	return ctx
}

// ack acknowledges and checks the outcome.
func ack(t *testing.T, r *ResolvedMutation, err error, outcome uint32) *ResolvedMutation {
	t.Helper()
	if err != nil {
		t.Fatal(err)
	}
	if r.Reply.OutcomeCode != outcome {
		t.Fatalf("outcome %d reason %d, want outcome %d", r.Reply.OutcomeCode, r.Reply.ReasonCode, outcome)
	}
	if err := r.Acknowledge(coordCtx(t)); err != nil {
		t.Fatal(err)
	}
	return r
}

// grant gives node's identity the holder rights on key (root, acknowledged).
func (g *liveGroup) grant(t *testing.T, rootC *CoordinationClient, key string, node *Node) {
	t.Helper()
	ctx := coordCtx(t)
	acl, err := rootC.InspectACL(ctx, res(key))
	if err != nil {
		t.Fatal(err)
	}
	id, _ := node.NodeID()
	target, _ := hex.DecodeString(id)
	r, err := rootC.SetGrant(ctx, res(key), target, RightsHolder, acl.ACLRevision)
	ack(t, r, err, OutcomeCodeGrantUpdated)
}

func TestCoordinationLive_OpenCloseJournalLockAndReadOnly(t *testing.T) {
	g := group(t)
	leakCheck(t)
	ctx := coordCtx(t)
	node := g.actor(t, "root")
	journal := g.journal(t, "root")
	c := g.open(t, node, journal)

	if !bytes.Equal(c.GroupID(), g.groupID) || len(c.LocalID()) != 32 {
		t.Fatalf("ids: group %x local %x", c.GroupID(), c.LocalID())
	}
	if id, err := c.ClientID(ctx); err != nil || len(id) != 16 {
		t.Fatalf("ClientID = %x, %v", id, err)
	}

	// A second durable open over the same journal is refused with the
	// coordination kind, not a generic failure.
	_, err := OpenCoordinationClient(ctx, node, CoordinationOptions{GroupID: g.groupID, SeedTickets: g.tickets(), JournalDir: journal})
	var ce *CoordinationError
	if !errors.Is(err, ErrCoordinationJournalLocked) || !errors.As(err, &ce) || ce.Kind != CoordJournalLocked {
		t.Fatalf("second open: %v", err)
	}
	// Closing releases the lock; Close is idempotent.
	if err := c.Close(); err != nil {
		t.Fatal(err)
	}
	if err := c.Close(); err != nil {
		t.Fatal(err)
	}
	c2 := g.open(t, node, journal)
	_ = c2.Close()

	// A closed client refuses new calls.
	if _, err := c.GroupStatus(ctx); !errors.Is(err, ErrAlreadyClosed) && !errors.Is(err, ErrNotFound) {
		t.Fatalf("closed client: %v", err)
	}

	// Read-only: reads work, anything journaled is UNSUPPORTED, no journal dir needed.
	ro, err := OpenCoordinationClient(ctx, node, CoordinationOptions{GroupID: g.groupID, SeedTickets: g.tickets(), ReadOnly: true})
	if err != nil {
		t.Fatal(err)
	}
	defer ro.Close()
	if st, err := ro.GroupStatus(ctx); err != nil || len(st.Voters) != 3 {
		t.Fatalf("read-only GroupStatus = %+v, %v", st, err)
	}
	if _, err := ro.Acquire(ctx, res("ro"), time.Second); !errors.Is(err, ErrUnsupported) {
		t.Fatalf("read-only mutate: %v", err)
	}
	if _, err := ro.ClientID(ctx); !errors.Is(err, ErrUnsupported) {
		t.Fatalf("read-only ClientID: %v", err)
	}

	// Bad options are refused before anything starts.
	if _, err := OpenCoordinationClient(ctx, node, CoordinationOptions{GroupID: g.groupID[:5]}); !errors.Is(err, ErrInvalidArgument) {
		t.Errorf("short group id: %v", err)
	}
	if _, err := OpenCoordinationClient(ctx, node, CoordinationOptions{GroupID: g.groupID}); !errors.Is(err, ErrInvalidArgument) {
		t.Errorf("durable client without a journal dir: %v", err)
	}
}

func TestCoordinationLive_GroupStatusAndACL(t *testing.T) {
	g := group(t)
	leakCheck(t)
	ctx := coordCtx(t)
	rootC := g.open(t, g.actor(t, "root"), g.journal(t, "root"))
	worker := g.actor(t, "worker")
	g.grant(t, rootC, "status-head", worker)

	st, err := rootC.GroupStatus(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if hex.EncodeToString(st.GenesisHash) != g.info.GenesisHash || st.AuthorityDomain != g.info.Domain ||
		hex.EncodeToString(st.AuthorityPublicKey) != g.info.AuthorityKey || st.DeploymentProfileCode != 1 || st.FatalCode != 0 {
		t.Fatalf("status = %+v", st)
	}
	var want, got []string
	for _, v := range g.info.Voters {
		want = append(want, v.NodeID)
	}
	for _, v := range st.Voters {
		got = append(got, hex.EncodeToString(v))
	}
	sort.Strings(want)
	sort.Strings(got)
	if strings.Join(want, ",") != strings.Join(got, ",") {
		t.Fatalf("voters %v, want %v", got, want)
	}
	acl, err := rootC.InspectACL(ctx, res("status-head"))
	workerID, _ := worker.NodeID()
	if err != nil || len(acl.Grants) != 1 || hex.EncodeToString(acl.Grants[0].SubjectNodeID) != workerID || acl.Grants[0].Rights != RightsHolder {
		t.Fatalf("acl = %+v, %v", acl, err)
	}
	if admins, err := rootC.InspectAdmins(ctx); err != nil || len(admins.Administrators) == 0 {
		t.Fatalf("admins = %+v, %v", admins, err)
	}
	// The audit trail and the recovery export.
	rows, err := rootC.AuditStream(ctx, nil, 100)
	if err != nil || len(rows) == 0 {
		t.Fatalf("audit = %d rows, %v", len(rows), err)
	}
	if _, err := rootC.AuditStream(ctx, nil, 0); !errors.Is(err, ErrInvalidArgument) {
		t.Errorf("audit limit 0: %v", err)
	}
	exp, err := rootC.RecoveryExport(ctx)
	if err != nil || exp.TotalLen != uint64(len(exp.Body)) || len(exp.Blake3Checksum) == 0 {
		t.Fatalf("export = %d/%d bytes, %v", exp.TotalLen, len(exp.Body), err)
	}
	target := filepath.Join(t.TempDir(), "export.bin")
	man, err := rootC.RecoveryExportToFile(ctx, target)
	if err != nil {
		t.Fatal(err)
	}
	if fi, err := os.Stat(target); err != nil || uint64(fi.Size()) != man.TotalLen {
		t.Fatalf("exported file: %v, %v", fi, err)
	}
	if _, err := os.Stat(target + ".partial"); err == nil {
		t.Error("the .partial file was left behind")
	}
	// A non-administrator is refused with the RPC permission status.
	workerC := g.open(t, worker, g.journal(t, "worker"))
	_, err = workerC.AuditStream(ctx, nil, 10)
	var ce *CoordinationError
	if !errors.As(err, &ce) || ce.Kind != CoordRPC || !errors.Is(err, ErrCoordinationRPC) {
		t.Fatalf("worker audit: %v (want an RPC CoordinationError)", err)
	}
	t.Logf("non-administrator audit read: kind %d rpc status %d, status %d", ce.Kind, ce.RPCCode, ce.Status)
}

func TestCoordinationLive_MutationsAreResolvedAndAcknowledged(t *testing.T) {
	g := group(t)
	leakCheck(t)
	ctx := coordCtx(t)
	rootC := g.open(t, g.actor(t, "root"), g.journal(t, "root"))
	worker := g.actor(t, "worker")
	g.grant(t, rootC, "head", worker)
	w := g.open(t, worker, g.journal(t, "worker"))

	acq, err := w.Acquire(ctx, res("head"), 30*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	if acq.Reply.OutcomeCode != OutcomeCodeGranted || acq.Reply.Snapshot == nil || !acq.Reply.Snapshot.Held {
		t.Fatalf("acquire = %+v", acq.Reply)
	}
	fence, _ := FenceOf(*acq.Reply.Snapshot)

	// The delivery contract: no new sequence until acknowledged.
	if _, err := w.Acquire(ctx, res("other"), 30*time.Second); !errors.Is(err, ErrCoordinationAwaitingAck) || !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("mutation before ack: %v", err)
	}
	if err := acq.Acknowledge(ctx); err != nil {
		t.Fatal(err)
	}
	if err := acq.Acknowledge(ctx); !errors.Is(err, ErrNotFound) {
		t.Fatalf("second acknowledge: %v", err)
	}

	renewed := ack(t, mustResolved(w.Renew(ctx, fence)), nil, OutcomeCodeRenewed)
	if renewed.Reply.Sequence != acq.Reply.Sequence+1 {
		t.Errorf("sequence %d after %d", renewed.Reply.Sequence, acq.Reply.Sequence)
	}
	before, err := w.ReadLinearizable(ctx, res("head"))
	if err != nil {
		t.Fatal(err)
	}
	ack(t, mustResolved(w.CompareAndSet(ctx, fence, before.ValueRevision, []byte("v1"))), nil, OutcomeCodeMutated)

	// A stale expected revision is a committed typed rejection: still a result.
	stale, err := w.CompareAndSet(ctx, fence, before.ValueRevision, []byte("v2"))
	if err != nil {
		t.Fatal(err)
	}
	if !stale.Reply.Rejected() || stale.Reply.ReasonCode != ReasonCodeValueRevisionMismatch {
		t.Fatalf("stale = %+v", stale.Reply)
	}
	if err := stale.Acknowledge(ctx); err != nil {
		t.Fatal(err)
	}
	mine, err := w.ReadLinearizable(ctx, res("head"))
	if err != nil || string(mine.Value) != "v1" || !mine.Held || mine.Epoch != fence.Epoch || mine.TTLMs == 0 {
		t.Fatalf("read = %+v, %v", mine, err)
	}
	ack(t, mustResolved(w.ReleaseFence(ctx, fence)), nil, OutcomeCodeReleased)
	if s, _ := w.ReadLinearizable(ctx, res("head")); s.Held {
		t.Fatal("still held after release")
	}
	late, err := w.Renew(ctx, fence)
	if err != nil || !late.Reply.Rejected() || (late.Reply.ReasonCode != ReasonCodeNotHeld && late.Reply.ReasonCode != ReasonCodeEpochMismatch) {
		t.Fatalf("renew after release = %+v, %v", late, err)
	}
	_ = late.Acknowledge(ctx)

	// A request the C ABI would refuse never reaches it.
	if _, err := w.Mutate(ctx, MutationRequest{Op: MutateSetAdmin, TargetNodeID: []byte{1, 2}}); !errors.Is(err, ErrInvalidArgument) {
		t.Errorf("short node id: %v", err)
	}
}

// A mutation the replicated ACL refuses is a typed RPC error that maps to
// ErrPermissionDenied -- and it stays journaled as unresolved (the open
// journal-blocks-on-denial question, plan Q2): the same error comes back on
// every later mutation or Resume. Uses its own journal so it cannot block the
// other tests.
func TestCoordinationLive_DeniedMutationIsAPermissionDeniedRPCErrorAndStaysUnresolved(t *testing.T) {
	g := group(t)
	leakCheck(t)
	ctx := coordCtx(t)
	w := g.reopen(t, g.actor(t, "worker"), g.journal(t, "worker-denied"))
	var ce *CoordinationError
	_, err := w.SetAdmin(ctx, bytes.Repeat([]byte{7}, 32), true, 0)
	if !errors.As(err, &ce) || ce.Kind != CoordRPC || ce.RPCCode != 7 || !errors.Is(err, ErrPermissionDenied) || !errors.Is(err, ErrCoordinationRPC) {
		t.Fatalf("denied SetAdmin: %v", err)
	}
	if _, err := w.Resume(ctx); !errors.Is(err, ErrPermissionDenied) {
		t.Fatalf("Resume after a denial: %v", err)
	}
	if _, err := w.Acquire(ctx, res("denied"), 30*time.Second); err == nil {
		t.Fatal("a new mutation was permitted over an unresolved denied one")
	}
}

func mustResolved(r *ResolvedMutation, err error) *ResolvedMutation {
	if err != nil {
		panic(err)
	}
	return r
}

func TestCoordinationLive_UnacknowledgedResultIsRedeliveredAfterReopen(t *testing.T) {
	g := group(t)
	leakCheck(t)
	ctx := coordCtx(t)
	rootC := g.open(t, g.actor(t, "root"), g.journal(t, "root"))
	worker := g.actor(t, "worker")
	g.grant(t, rootC, "restart", worker)
	journal := g.journal(t, "worker")
	w := g.open(t, worker, journal)
	if r, err := w.Unacknowledged(ctx); err != nil || r != nil {
		t.Fatalf("fresh journal: %+v, %v", r, err)
	}
	if r, err := w.Resume(ctx); err != nil || r != nil {
		t.Fatalf("fresh journal resume: %+v, %v", r, err)
	}
	acq, err := w.Acquire(ctx, res("restart"), 30*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	seq := acq.Reply.Sequence
	// Drop WITHOUT acknowledging, then restart the client.
	if err := acq.Release(); err != nil {
		t.Fatal(err)
	}
	if err := w.Close(); err != nil {
		t.Fatal(err)
	}
	w2 := g.reopen(t, worker, journal)
	again, err := w2.Unacknowledged(ctx)
	if err != nil || again == nil {
		t.Fatalf("redelivery: %+v, %v", again, err)
	}
	if again.Reply.Sequence != seq || again.Reply.OutcomeCode != OutcomeCodeGranted {
		t.Fatalf("redelivered %+v, want sequence %d granted", again.Reply, seq)
	}
	// Still no new sequence until it is acknowledged.
	if _, err := w2.Acquire(ctx, res("restart-2"), 30*time.Second); !errors.Is(err, ErrCoordinationAwaitingAck) {
		t.Fatalf("mutation before ack: %v", err)
	}
	if err := again.Acknowledge(ctx); err != nil {
		t.Fatal(err)
	}
	fence, _ := FenceOf(*again.Reply.Snapshot)
	ack(t, mustResolved(w2.ReleaseFence(ctx, fence)), nil, OutcomeCodeReleased)

	// Acknowledging after the client was closed fails closed but consumes the
	// handle; the result stays journaled for the next open.
	last, err := w2.Acquire(ctx, res("restart"), 30*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	if err := w2.Close(); err != nil {
		t.Fatal(err)
	}
	if err := last.Acknowledge(ctx); !errors.Is(err, ErrCoordinationClosed) || !errors.Is(err, ErrAlreadyClosed) {
		t.Fatalf("acknowledge after close: %v", err)
	}
	if err := last.Acknowledge(ctx); !errors.Is(err, ErrNotFound) {
		t.Fatalf("handle not consumed: %v", err)
	}
	w3 := g.reopen(t, worker, journal)
	back, err := w3.Unacknowledged(ctx)
	if err != nil || back == nil || back.Reply.Sequence != last.Reply.Sequence {
		t.Fatalf("redelivery after a failed ack: %+v, %v", back, err)
	}
	fence, _ = FenceOf(*back.Reply.Snapshot)
	_ = back.Acknowledge(ctx)
	ack(t, mustResolved(w3.ReleaseFence(ctx, fence)), nil, OutcomeCodeReleased)
}

func TestCoordinationLive_LeaseRenewsInRustMutatesAndHandsOff(t *testing.T) {
	g := group(t)
	leakCheck(t)
	ctx := coordCtx(t)
	rootC := g.open(t, g.actor(t, "root"), g.journal(t, "root"))
	worker, contender := g.actor(t, "worker"), g.actor(t, "contender")
	g.grant(t, rootC, "lease", worker)
	g.grant(t, rootC, "lease", contender)
	w := g.open(t, worker, g.journal(t, "worker"))

	// Renew every 100 ms so the CompareAndSet and the final handoff land while
	// renewals are in flight.
	lease, err := w.AcquireLease(ctx, coordNS, []byte("lease"), LeaseOptions{TTL: 6 * time.Second, RenewEvery: 100 * time.Millisecond, HolderMargin: 300 * time.Millisecond})
	if err != nil {
		t.Fatal(err)
	}
	first, ok := lease.Fence()
	if !ok || !bytes.Equal(first.Holder, w.LocalID()) || first.Resource.Namespace != coordNS || first.Epoch != lease.GrantedFence().Epoch {
		t.Fatalf("fence = %+v, %v", first, ok)
	}
	if st, err := lease.Status(); err != nil || st != LeaseHeld {
		t.Fatalf("status = %v, %v", st, err)
	}

	// Another entitled actor is refused, with the holder in the snapshot.
	c := g.open(t, contender, g.journal(t, "contender"))
	_, err = c.AcquireLease(ctx, coordNS, []byte("lease"), LeaseOptions{})
	var denied *LeaseDeniedError
	if !errors.Is(err, ErrLeaseDenied) || !errors.As(err, &denied) || denied.Reason != DenyHeldByOther || !denied.Snapshot.Held {
		t.Fatalf("contender: %v", err)
	}
	if denied.Snapshot.Holder == nil || *denied.Snapshot.Holder == "" {
		t.Errorf("denied snapshot lacks the holder: %+v", denied.Snapshot)
	}

	// Outlive the TTL: the renew loop keeps it.
	time.Sleep(7 * time.Second)
	if f, ok := lease.Fence(); !ok || f.Epoch != first.Epoch {
		t.Fatalf("lease lost after a TTL: %+v, %v", f, ok)
	}
	cur, err := w.ReadLinearizable(ctx, res("lease"))
	if err != nil {
		t.Fatal(err)
	}
	mut, err := lease.CompareAndSet(ctx, cur.ValueRevision, []byte("next"))
	if err != nil || mut.OutcomeCode != OutcomeCodeMutated {
		t.Fatalf("CompareAndSet = %+v, %v", mut, err)
	}
	stale, err := lease.CompareAndSet(ctx, cur.ValueRevision, []byte("stale"))
	if err != nil || !stale.Rejected() || stale.ReasonCode != ReasonCodeValueRevisionMismatch {
		t.Fatalf("stale CompareAndSet = %+v, %v", stale, err)
	}
	handed, err := lease.ReleaseWithValue(ctx, mut.Snapshot.ValueRevision, []byte("final"))
	if err != nil || handed.OutcomeCode != OutcomeCodeReleased {
		t.Fatalf("ReleaseWithValue = %+v, %v", handed, err)
	}
	if _, ok := lease.Fence(); ok {
		t.Fatal("fence available after the handoff")
	}
	if st, _ := lease.Status(); st != LeaseStatusReleased {
		t.Fatalf("status after handoff = %v", st)
	}
	if err := lease.Release(ctx); err != nil { // consumed: idempotent
		t.Fatal(err)
	}
	if _, err := lease.CompareAndSet(ctx, 0, []byte("x")); err == nil {
		t.Fatal("CompareAndSet on a released lease succeeded")
	}
	after, _ := w.ReadLinearizable(ctx, res("lease"))
	if after.Held || string(after.Value) != "final" {
		t.Fatalf("after = %+v", after)
	}
	// The journal was left free by the lease's own acknowledgements.
	if r, err := w.Unacknowledged(ctx); err != nil || r != nil {
		t.Fatalf("Unacknowledged = %+v, %v", r, err)
	}

	// Graceful Release, and Close (free without releasing) lapses by TTL.
	again, err := w.AcquireLease(ctx, coordNS, []byte("lease"), LeaseOptions{TTL: 5 * time.Second})
	if err != nil {
		t.Fatal(err)
	}
	if err := again.Release(ctx); err != nil {
		t.Fatal(err)
	}
	if s, _ := w.ReadLinearizable(ctx, res("lease")); s.Held {
		t.Fatal("held after Release")
	}
	dropped, err := w.AcquireLease(ctx, coordNS, []byte("lease"), LeaseOptions{TTL: 5 * time.Second})
	if err != nil {
		t.Fatal(err)
	}
	if err := dropped.Close(); err != nil {
		t.Fatal(err)
	}
	eventually(t, "the dropped lease to lapse by TTL", func() bool {
		s, err := w.ReadLinearizable(ctx, res("lease"))
		return err == nil && !s.Held
	})
}

// A Release waits for an in-flight CompareAndSet and neither is lost; closing
// the client with operations in flight lets them complete and frees the journal
// lock afterwards.
func TestCoordinationLive_CloseAndReleaseWithOperationsInFlight(t *testing.T) {
	g := group(t)
	leakCheck(t)
	ctx := coordCtx(t)
	rootC := g.open(t, g.actor(t, "root"), g.journal(t, "root"))
	worker := g.actor(t, "worker")
	g.grant(t, rootC, "inflight", worker)
	journal := g.journal(t, "worker")
	w := g.open(t, worker, journal)
	lease, err := w.AcquireLease(ctx, coordNS, []byte("inflight"), LeaseOptions{TTL: 5 * time.Second, RenewEvery: 100 * time.Millisecond})
	if err != nil {
		t.Fatal(err)
	}
	var wg sync.WaitGroup
	var casErr, relErr error
	wg.Add(2)
	go func() {
		defer wg.Done()
		_, casErr = lease.CompareAndSet(ctx, 0, []byte("a"))
	}()
	go func() {
		defer wg.Done()
		time.Sleep(20 * time.Millisecond)
		relErr = lease.Release(ctx)
	}()
	wg.Wait()
	if relErr != nil {
		t.Fatalf("Release: %v", relErr)
	}
	// The CompareAndSet either landed before the release or was refused at once
	// because the release had begun; it never hangs or corrupts the journal.
	if casErr != nil && !errors.Is(casErr, ErrAlreadyClosed) {
		t.Fatalf("CompareAndSet: %v", casErr)
	}

	// Close with a mutation in flight: it completes (or is redelivered), the
	// journal lock is released, and a reopen sees a consistent journal.
	mutErr := make(chan error, 1)
	go func() {
		_, err := w.Acquire(ctx, res("inflight"), 5*time.Second)
		mutErr <- err
	}()
	time.Sleep(10 * time.Millisecond)
	if err := w.Close(); err != nil {
		t.Fatal(err)
	}
	if err := <-mutErr; err != nil && !errors.Is(err, ErrCoordinationClosed) {
		t.Fatalf("in-flight mutation: %v", err)
	}
	w2 := g.reopen(t, worker, journal) // the lock is free again
	r, err := w2.Unacknowledged(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if r != nil {
		if r.Reply.Snapshot != nil && r.Reply.OutcomeCode == OutcomeCodeGranted {
			if f, ok := FenceOf(*r.Reply.Snapshot); ok {
				_ = r.Acknowledge(ctx)
				ack(t, mustResolved(w2.ReleaseFence(ctx, f)), nil, OutcomeCodeReleased)
			}
		} else {
			_ = r.Acknowledge(ctx)
		}
	}
}

// A context that ends while a mutation is in flight abandons the wait, not the
// operation: the result is journaled and redelivered.
func TestCoordinationLive_ContextCancelledMutationIsRedelivered(t *testing.T) {
	g := group(t)
	leakCheck(t)
	ctx := coordCtx(t)
	rootC := g.open(t, g.actor(t, "root"), g.journal(t, "root"))
	worker := g.actor(t, "worker")
	g.grant(t, rootC, "ctx-head", worker)
	journal := g.journal(t, "worker")
	w := g.open(t, worker, journal)

	short, cancel := context.WithCancel(ctx)
	cancel() // already ended: the call returns ctx.Err() at once
	if _, err := w.Acquire(short, res("ctx-head"), 30*time.Second); !errors.Is(err, context.Canceled) {
		t.Fatalf("cancelled Acquire = %v", err)
	}
	// The operation was started and runs on; its result is journaled. Wait for
	// the (background-freed) resolution to land, then take it from the journal.
	var got *ResolvedMutation
	eventually(t, "the abandoned mutation to be redelivered", func() bool {
		r, err := w.Unacknowledged(ctx)
		if err != nil {
			return false // still AWAITING... or in flight
		}
		got = r
		return r != nil
	})
	if got.Reply.OutcomeCode != OutcomeCodeGranted {
		t.Fatalf("redelivered %+v", got.Reply)
	}
	fence, _ := FenceOf(*got.Reply.Snapshot)
	_ = got.Acknowledge(ctx)
	ack(t, mustResolved(w.ReleaseFence(ctx, fence)), nil, OutcomeCodeReleased)
}

func TestCoordinationLive_RetireOtherClientAndProbe(t *testing.T) {
	g := group(t)
	leakCheck(t)
	ctx := coordCtx(t)
	node := g.actor(t, "root")
	c := g.open(t, node, g.journal(t, "root"))
	if _, err := c.RetireOtherClient(ctx, make([]byte, 15)); !errors.Is(err, ErrInvalidArgument) {
		t.Errorf("15-byte client id: %v", err)
	}
	other := NewOperationID() // any 16 bytes: a client id this actor never used
	reply, err := c.RetireOtherClient(ctx, other)
	if err != nil {
		t.Fatal(err)
	}
	if reply.OutcomeCode != OutcomeCodeClientRetired && reply.OutcomeCode != OutcomeCodeRejected {
		t.Fatalf("retire = %+v", reply)
	}
	probe, err := c.ProbeClient(ctx, c.LocalID(), other)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(probe.TargetClientID, other) {
		t.Fatalf("probe = %+v", probe)
	}
}

// ─── placement ───────────────────────────────────────────────────────────

func policyFor(st GroupStatus, mods ...func(*PlacementPolicy)) PlacementPolicy {
	p := PlacementPolicy{Revision: 1, Digest: bytes.Repeat([]byte{0x44}, 32), AuthorityPublicKey: st.AuthorityPublicKey,
		AuthorityDomain: st.AuthorityDomain, TargetVoters: 3}
	voters := append([][]byte(nil), st.Voters...)
	sort.Slice(voters, func(i, j int) bool { return bytes.Compare(voters[i], voters[j]) < 0 })
	for _, v := range voters {
		p.Eligible = append(p.Eligible, PlacementCandidate{NodeID: v})
	}
	for _, m := range mods {
		m(&p)
	}
	return p
}

func oneCluster(ids ...[]byte) TopologySnapshot {
	var hexIDs []string
	for _, id := range ids {
		hexIDs = append(hexIDs, hex.EncodeToString(id))
	}
	sort.Strings(hexIDs)
	return TopologySnapshot{Clusters: []ClusterView{{Members: hexIDs, Bridge: hexIDs[0], Witnesses: hexIDs[:1]}}, SeparationCoverage: 1}
}

func TestCoordinationLive_PlacementManagerWithGoProviders(t *testing.T) {
	g := group(t)
	leakCheck(t)
	ctx := coordCtx(t)
	node := g.actor(t, "root")
	rootC := g.open(t, node, g.journal(t, "root"))
	st, err := rootC.GroupStatus(ctx)
	if err != nil {
		t.Fatal(err)
	}
	up := testUpcalls(t, node.Runtime())
	var policyReads, topoReads atomic.Int64
	if err := up.ServePlacementPolicy(func(context.Context) (PlacementPolicy, error) {
		policyReads.Add(1)
		return policyFor(st), nil
	}); err != nil {
		t.Fatal(err)
	}
	if err := up.ServeTopologySnapshot(func(context.Context) (TopologySnapshot, error) {
		topoReads.Add(1)
		return oneCluster(st.Voters...), nil
	}); err != nil {
		t.Fatal(err)
	}
	mgr, err := NewPlacementManager(rootC, up, nil)
	if err != nil {
		t.Fatal(err)
	}
	defer mgr.Close()
	report, err := mgr.Reconcile(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if report.PolicyRevision != 1 || !bytes.Equal(report.PolicyDigest, bytes.Repeat([]byte{0x44}, 32)) || report.Reason == "" ||
		report.PendingOperationID != nil || len(report.CurrentVoters) != 3 {
		t.Fatalf("report = %+v", report)
	}
	if policyReads.Load() < 1 || topoReads.Load() < 1 {
		t.Fatalf("providers asked %d / %d times", policyReads.Load(), topoReads.Load())
	}
	// Passes are serialized: concurrent reconciles all succeed.
	var wg sync.WaitGroup
	for i := 0; i < 4; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			if _, err := mgr.Reconcile(ctx); err != nil {
				t.Errorf("concurrent Reconcile: %v", err)
			}
		}()
	}
	wg.Wait()
	if err := mgr.Close(); err != nil {
		t.Fatal(err)
	}

	// The node's own topology view is not available until it joined a swarm.
	own, err := NewPlacementManager(rootC, up, node)
	if err != nil {
		t.Fatal(err)
	}
	defer own.Close()
	if _, err := own.Reconcile(ctx); err == nil {
		t.Fatal("a pass over a node that never joined a swarm succeeded")
	}
}

func TestCoordinationLive_PlacementProviderFailuresStopThePass(t *testing.T) {
	g := group(t)
	leakCheck(t)
	ctx := coordCtx(t)
	node := g.actor(t, "root")
	rootC := g.open(t, node, g.journal(t, "root"))
	st, err := rootC.GroupStatus(ctx)
	if err != nil {
		t.Fatal(err)
	}
	newcomer := bytes.Repeat([]byte{0xEE}, 32)
	voters := append([][]byte(nil), st.Voters...)
	policy := policyFor(st, func(p *PlacementPolicy) {
		p.Eligible = append(p.Eligible, PlacementCandidate{NodeID: newcomer})
		sort.Slice(p.Eligible, func(i, j int) bool { return bytes.Compare(p.Eligible[i].NodeID, p.Eligible[j].NodeID) < 0 })
		p.Excluded = [][]byte{voters[0]}
	})
	view := oneCluster(append(voters, newcomer)...)
	up := testUpcalls(t, node.Runtime())
	if err := up.ServePlacementPolicy(func(context.Context) (PlacementPolicy, error) { return policy, nil }); err != nil {
		t.Fatal(err)
	}
	if err := up.ServeTopologySnapshot(func(context.Context) (TopologySnapshot, error) { return view, nil }); err != nil {
		t.Fatal(err)
	}
	mgr, err := NewPlacementManager(rootC, up, nil)
	if err != nil {
		t.Fatal(err)
	}
	defer mgr.Close()
	unchanged := func(why string) {
		t.Helper()
		after, err := rootC.GroupStatus(ctx)
		if err != nil || len(after.Voters) != len(st.Voters) || after.PendingOperationID != nil {
			t.Fatalf("%s: membership changed: %+v, %v", why, after, err)
		}
	}

	// No provisioner served: UNIMPLEMENTED fails the pass, never fails open.
	if _, err := mgr.Reconcile(ctx); err == nil {
		t.Fatal("a pass without a provisioner succeeded")
	}
	unchanged("no provisioner")

	// A provisioner error fails the pass and carries its message.
	var asked []ProvisionRequest
	var mu sync.Mutex
	if err := up.ServePlacementProvisioner(func(_ context.Context, r ProvisionRequest) (MemberSeed, error) {
		mu.Lock()
		asked = append(asked, r)
		mu.Unlock()
		return MemberSeed{}, errors.New("no capacity")
	}); err != nil {
		t.Fatal(err)
	}
	_, err = mgr.Reconcile(ctx)
	var ce *CoordinationError
	if err == nil || !strings.Contains(err.Error(), "no capacity") || !errors.As(err, &ce) {
		t.Fatalf("provisioner error: %v", err)
	}
	mu.Lock()
	if len(asked) != 1 || !bytes.Equal(asked[0].NodeID, newcomer) || !bytes.Equal(asked[0].GroupID, g.groupID) ||
		!bytes.Equal(asked[0].PolicyDigest, bytes.Repeat([]byte{0x44}, 32)) || asked[0].MembershipLogID == nil {
		t.Fatalf("provision request = %+v", asked)
	}
	mu.Unlock()
	unchanged("provisioner error")

	// A seed for a different node is refused before anything is submitted.
	if err := up.ServePlacementProvisioner(func(context.Context, ProvisionRequest) (MemberSeed, error) {
		return MemberSeed{NodeID: bytes.Repeat([]byte{0xDD}, 32), Ticket: "aster1ticket"}, nil
	}); err != nil {
		t.Fatal(err)
	}
	if _, err := mgr.Reconcile(ctx); err == nil {
		t.Fatal("a seed for the wrong node was accepted")
	}
	unchanged("wrong seed")

	// A malformed answer (garbage payload) fails the pass too.
	if err := up.HandleKind(UpcallPlacementProvision, func(context.Context, *UpcallRequest) ([]byte, error) {
		return []byte{1, 2, 3}, nil
	}); err != nil {
		t.Fatal(err)
	}
	if _, err := mgr.Reconcile(ctx); err == nil {
		t.Fatal("a malformed provisioner answer was accepted")
	}

	// A broken topology source fails the pass.
	if err := up.ServeTopologySnapshot(func(context.Context) (TopologySnapshot, error) {
		return TopologySnapshot{}, errors.New("topology service down")
	}); err != nil {
		t.Fatal(err)
	}
	if _, err := mgr.Reconcile(ctx); err == nil || !strings.Contains(err.Error(), "topology service down") {
		t.Fatalf("broken topology: %v", err)
	}
	unchanged("broken topology")

	// A policy source that hangs past the kind's timeout fails the pass and is
	// cancelled (CANCEL{timeout}).
	if err := up.SetPolicy(UpcallPlacementPolicy, 300*time.Millisecond, FailureModeOpen); err != nil { // fail-open must NOT apply
		t.Fatal(err)
	}
	cancelled := make(chan struct{}, 1)
	if err := up.ServePlacementPolicy(func(c context.Context) (PlacementPolicy, error) {
		<-c.Done()
		cancelled <- struct{}{}
		return PlacementPolicy{}, c.Err()
	}); err != nil {
		t.Fatal(err)
	}
	if _, err := mgr.Reconcile(ctx); err == nil {
		t.Fatal("a pass with a hung policy source succeeded")
	}
	select {
	case <-cancelled:
	case <-ctx.Done():
		t.Fatal("the hung policy source was never cancelled")
	}
	unchanged("hung policy")
}
