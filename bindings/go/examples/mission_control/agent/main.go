//go:build cgo

// Mission Control agent: connects with StartClient and calls getStatus,
// submitLog and ingestMetrics (Python's agent.py, Rust's bin/agent.rs).
//
//	go run ./examples/mission_control/agent <server address>
//
// Configuration comes from aster.ConfigFromEnv. Against the -auth server,
// set this agent's key and credential (ASTER_SECRET_KEY_FILE,
// ASTER_ENROLLMENT_CREDENTIAL) and the same ASTER_ROOT_PUBKEY,
// ASTER_ENROLLMENT_AUDIENCE and ASTER_ENROLLMENT_FLEET_NAMESPACE as the
// server; the roles in the credential decide which calls succeed. The
// address defaults to ASTER_ENDPOINT_ADDR.
package main

import (
	"context"
	"errors"
	"fmt"
	"log"
	"os"
	"slices"
	"time"

	aster "sdk.getaster.now/go"
	"sdk.getaster.now/go/examples/mission_control/mc"
)

func main() {
	if err := run(); err != nil {
		log.Fatal(err)
	}
}

func run() error {
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	cfg, err := aster.ConfigFromEnv()
	if err != nil {
		return err
	}
	address := cfg.EndpointAddr
	if len(os.Args) > 1 {
		address = os.Args[1]
	}
	if address == "" {
		return errors.New("usage: agent <server address> (or ASTER_ENDPOINT_ADDR)")
	}
	client, err := aster.StartClient(ctx, cfg)
	if err != nil {
		return err
	}
	defer client.Close()
	// Enrolls with the server first when enrollment is configured.
	conn, err := client.ConnectTicket(ctx, address)
	if err != nil {
		return err
	}

	status, err := aster.Call[mc.StatusRequest, mc.StatusResponse](ctx, conn, "MissionControl", "getStatus",
		&mc.StatusRequest{AgentId: "go-agent-1"})
	if err != nil {
		fmt.Println("getStatus refused:", err)
	} else {
		fmt.Printf("Status: %+v\n", *status)
	}

	now := float64(time.Now().UnixMilli()) / 1000
	if _, err := aster.Call[mc.LogEntry, mc.SubmitLogResult](ctx, conn, "MissionControl", "submitLog",
		&mc.LogEntry{Timestamp: now, Level: "info", Message: "agent started", AgentId: "go-agent-1"}); err != nil {
		return err
	}
	fmt.Println("Log submitted")

	points := make([]*mc.MetricPoint, 100)
	for i := range points {
		points[i] = &mc.MetricPoint{Name: "cpu.usage", Value: float64(i % 100), Timestamp: now}
	}
	result, err := aster.CallClientStream[mc.MetricPoint, mc.IngestResult](ctx, conn, "MissionControl", "ingestMetrics",
		slices.Values(points))
	if err != nil {
		fmt.Println("ingestMetrics refused:", err)
	} else {
		fmt.Println("Metrics accepted:", result.Accepted)
	}
	return nil
}
