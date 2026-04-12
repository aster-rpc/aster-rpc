//go:build cgo

// AsterServer echo example: server accepts Aster RPC, echoes request back.
package main

import (
	"bytes"
	"context"
	"fmt"
	"os"
	"time"

	aster "aster-ffi"
)

func main() {
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()

	fmt.Println("=== AsterServer Echo Example ===")

	// 0. Compute contract_id via the Rust FFI
	fmt.Println("\n0. Computing contract_id via Rust FFI...")
	contractJSON := `{"name": "EchoService", "version": 1, "methods": [], "serialization_modes": ["xlang"], "scoped": "shared", "requires": null, "producer_language": ""}`
	contractID, err := aster.ComputeContractID(contractJSON)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error computing contract_id: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("   contract_id: %s\n", contractID)
	if len(contractID) != 64 {
		fmt.Fprintf(os.Stderr, "   FAIL: Expected 64-char hex, got %d chars\n", len(contractID))
		os.Exit(1)
	}
	fmt.Println("   PASS: Got valid 64-char hex contract_id.")

	// 1. Start the echo server
	fmt.Println("\n1. Starting AsterServer (echo handler)...")
	server, err := aster.NewServer(ctx, aster.ServerConfig{
		Handler: func(call aster.ReactorCall) aster.ReactorResponse {
			fmt.Printf("   Server received call from %s...: header=%d bytes, request=%d bytes\n",
				call.PeerID[:min(8, len(call.PeerID))],
				len(call.Header),
				len(call.Request))
			// Echo the request back
			return aster.ReactorResponse{ResponseFrame: call.Request}
		},
	})
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error creating server: %v\n", err)
		os.Exit(1)
	}
	defer server.Close()

	nodeID, _ := server.NodeID()
	fmt.Printf("   Server started! Node ID: %s...\n", nodeID[:min(16, len(nodeID))])

	// 2. Create a client endpoint
	fmt.Println("\n2. Creating client endpoint...")
	clientRuntime, err := aster.NewRuntime(ctx, aster.DefaultRuntimeConfig())
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		os.Exit(1)
	}
	defer clientRuntime.Close()

	clientEndpoint, err := aster.NewEndpoint(ctx, clientRuntime, aster.EndpointConfig{
		ALPNs: []string{aster.AsterALPN},
	})
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		os.Exit(1)
	}
	defer clientEndpoint.Close(ctx)
	fmt.Println("   Client endpoint created.")

	// 3. Connect client to server
	fmt.Println("\n3. Connecting client to server...")
	serverAddr, err := server.Node().NodeAddr()
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		os.Exit(1)
	}
	conn, err := clientEndpoint.ConnectNodeAddr(ctx, serverAddr, aster.AsterALPN)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		os.Exit(1)
	}
	defer conn.Close("done")
	fmt.Println("   Connected!")

	// 4. Open stream and send Aster-framed RPC
	fmt.Println("\n4. Opening stream and sending RPC...")
	send, recv, err := conn.OpenBi(ctx)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		os.Exit(1)
	}
	defer send.Close()
	defer recv.Close()

	headerPayload := []byte("EchoService.echo")
	requestPayload := []byte("Hello, Aster!")

	headerFrame := aster.EncodeFrame(headerPayload, aster.FlagHeader)
	requestFrame := aster.EncodeFrame(requestPayload, 0)

	combined := append(headerFrame, requestFrame...)
	if err := send.Write(ctx, combined); err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		os.Exit(1)
	}
	if err := send.Finish(ctx); err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		os.Exit(1)
	}
	fmt.Println("   Sent header + request.")

	// 5. Read the response
	fmt.Println("\n5. Reading response...")
	response, err := recv.Read(ctx)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("   Received %d bytes: %s\n", len(response), string(response))

	if bytes.Equal(response, requestPayload) {
		fmt.Println("   PASS: Response matches echoed payload!")
	} else {
		fmt.Fprintf(os.Stderr, "   FAIL: Expected %d bytes, got %d bytes.\n",
			len(requestPayload), len(response))
		os.Exit(1)
	}

	// 6. Cleanup
	fmt.Println("\n6. Cleaning up...")
	fmt.Println("\n=== SUCCESS: AsterServer echo round-trip works! ===")
}

func min(a, b int) int {
	if a < b {
		return a
	}
	return b
}
