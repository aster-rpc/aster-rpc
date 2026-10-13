//go:build cgo

// Package mc is Mission Control for Go: the MissionControl service of the
// Rust, Python and TypeScript examples (all four RPC patterns), with the
// auth chapter's roles. It mirrors examples/rust/mission-control: the same
// service and method names, wire type names and role names.
package mc

import (
	"context"
	"fmt"
	"iter"

	aster "sdk.getaster.now/go"
)

// Capability roles (the auth chapter). Each is a string in the aster.role
// claim of a node's enrollment credential (comma-separated for several).
// The server maps a peer's verified claims to these roles and Gate 3
// matches them against each method's Requires. Identical to the Rust
// example's roles.rs and Python's roles.py.
const (
	RoleStatus = "ops.status" // may read fleet status (getStatus)
	RoleLogs   = "ops.logs"   // may tail logs (tailLogs)
	RoleAdmin  = "ops.admin"  // administers agents: tails logs, runs commands
	RoleIngest = "ops.ingest" // may push metrics (ingestMetrics)
)

// ─── wire types ───────────────────────────────────────────────────────────

type StatusRequest struct{ AgentID string }

func (StatusRequest) AsterWireType() string { return "mission/StatusRequest" }

type StatusResponse struct {
	AgentID    string
	Status     string
	UptimeSecs int64
}

func (StatusResponse) AsterWireType() string { return "mission/StatusResponse" }

type LogEntry struct {
	Timestamp float64
	Level     string
	Message   string
	AgentID   string
}

func (LogEntry) AsterWireType() string { return "mission/LogEntry" }

type SubmitLogResult struct{ Accepted bool }

func (SubmitLogResult) AsterWireType() string { return "mission/SubmitLogResult" }

type TailRequest struct {
	AgentID string
	Level   string
}

func (TailRequest) AsterWireType() string { return "mission/TailRequest" }

type MetricPoint struct {
	Name      string
	Value     float64
	Timestamp float64
}

func (MetricPoint) AsterWireType() string { return "mission/MetricPoint" }

type IngestResult struct {
	Accepted int32
	Dropped  int32
}

func (IngestResult) AsterWireType() string { return "mission/IngestResult" }

type Command struct{ Command string }

func (Command) AsterWireType() string { return "mission/Command" }

type CommandResult struct {
	Stdout   string
	Stderr   string
	ExitCode int32
}

func (CommandResult) AsterWireType() string { return "mission/CommandResult" }

// ─── service ──────────────────────────────────────────────────────────────

// Service is MissionControl. With auth, each method carries the auth
// chapter's requirement:
//
//	getStatus      ops.status
//	submitLog      any enrolled node
//	tailLogs       ops.logs or ops.admin
//	ingestMetrics  ops.ingest
//	runCommand     ops.admin
//
// No handler reads a credential: the server's [enrollment] configuration
// admits only enrolled nodes and turns their credentials into roles.
func Service(auth bool) *aster.Service {
	req := func(r *aster.Requirement) []aster.MethodOption {
		if !auth {
			return nil
		}
		return []aster.MethodOption{aster.Requires(r)}
	}
	return aster.NewService("MissionControl", 1,
		aster.Unary("getStatus", getStatus, req(aster.RequireRole(RoleStatus))...),
		aster.Unary("submitLog", submitLog),
		aster.ServerStream("tailLogs", tailLogs, req(aster.RequireAnyOf(RoleLogs, RoleAdmin))...),
		aster.ClientStream("ingestMetrics", ingestMetrics, req(aster.RequireRole(RoleIngest))...),
		aster.BidiStream("runCommand", runCommand, req(aster.RequireRole(RoleAdmin))...),
	)
}

func getStatus(_ context.Context, r *StatusRequest) (*StatusResponse, error) {
	return &StatusResponse{AgentID: r.AgentID, Status: "running", UptimeSecs: 3600}, nil
}

func submitLog(_ context.Context, _ *LogEntry) (*SubmitLogResult, error) {
	return &SubmitLogResult{Accepted: true}, nil
}

func tailLogs(_ context.Context, r *TailRequest, send func(*LogEntry) error) error {
	for i := range 3 {
		if err := send(&LogEntry{Timestamp: float64(i), Level: "info", Message: fmt.Sprintf("log %d", i), AgentID: r.AgentID}); err != nil {
			return err
		}
	}
	return nil
}

func ingestMetrics(_ context.Context, points iter.Seq2[*MetricPoint, error]) (*IngestResult, error) {
	var accepted int32
	for _, err := range points {
		if err != nil {
			return nil, err
		}
		accepted++
	}
	return &IngestResult{Accepted: accepted}, nil
}

// runCommand echoes each command; it does not run a shell.
func runCommand(_ context.Context, cmds iter.Seq2[*Command, error], send func(*CommandResult) error) error {
	for cmd, err := range cmds {
		if err != nil {
			return err
		}
		if err := send(&CommandResult{Stdout: "ran: " + cmd.Command}); err != nil {
			return err
		}
	}
	return nil
}
