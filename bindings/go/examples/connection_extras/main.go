//go:build cgo

// Example: Connection Extras (8.1)
//
// This example demonstrates how to:
// - Create endpoints with matching ALPNs
// - Connect two endpoints
// - Use RemoteID to identify the peer
// - Send datagrams
// - Close connections cleanly
//
// Run:
//
//	cd bindings/go
//	CGO_CFLAGS="-I$(pwd)/../../ffi" CGO_LDFLAGS="-L$(pwd)/../../target/release/deps -l aster_transport_ffi" go run ./examples/connection_extras/main.go
package main

import (
	"context"
	"fmt"
	"os"
	"time"

	aster "aster-ffi"
)

const ALPN = "aster"

func main() {
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()

	fmt.Println("=== Connection Extras Example (8.1) ===")

	// Create endpoint A (listener) with ALPN
	fmt.Println("1. Creating endpoint A (listener)...")
	runtimeA, err := aster.NewRuntime(ctx, aster.DefaultRuntimeConfig())
	if err != nil {
		fmt.Fprintf(os.Stderr, "   Error: %v\n", err)
		os.Exit(1)
	}
	defer runtimeA.Close()

	cfgA := aster.EndpointConfig{
		ALPNs: []string{ALPN},
	}
	endpointA, err := aster.NewEndpoint(ctx, runtimeA, cfgA)
	if err != nil {
		fmt.Fprintf(os.Stderr, "   Error: %v\n", err)
		os.Exit(1)
	}
	defer endpointA.Close(ctx)

	epAID, _ := endpointA.NodeID()
	addrA, _ := endpointA.AddrInfo()
	fmt.Printf("   Endpoint A created! ID: %.8s...\n", epAID)
	fmt.Printf("   Relay: %s\n\n", addrA.RelayURL)

	// Create endpoint B (dialer) with same ALPN
	fmt.Println("2. Creating endpoint B (dialer)...")
	runtimeB, err := aster.NewRuntime(ctx, aster.DefaultRuntimeConfig())
	if err != nil {
		fmt.Fprintf(os.Stderr, "   Error: %v\n", err)
		os.Exit(1)
	}
	defer runtimeB.Close()

	cfgB := aster.EndpointConfig{
		ALPNs: []string{ALPN},
	}
	endpointB, err := aster.NewEndpoint(ctx, runtimeB, cfgB)
	if err != nil {
		fmt.Fprintf(os.Stderr, "   Error: %v\n", err)
		os.Exit(1)
	}
	defer endpointB.Close(ctx)

	epBID, _ := endpointB.NodeID()
	fmt.Printf("   Endpoint B created! ID: %.8s...\n\n", epBID)

	// A accepts connections in background
	connACh := make(chan *aster.Connection, 1)
	go func() {
		fmt.Println("3. A accepting connections...")
		conn, err := endpointA.Accept(ctx)
		if err != nil {
			fmt.Fprintf(os.Stderr, "   Accept error: %v\n", err)
			return
		}
		connACh <- conn
	}()

	// Give A time to start accepting
	time.Sleep(100 * time.Millisecond)

	// B connects to A
	fmt.Println("4. B connecting to A...")
	connB, err := endpointB.ConnectNodeAddr(ctx, addrA, ALPN)
	if err != nil {
		fmt.Fprintf(os.Stderr, "   Error: %v\n", err)
		os.Exit(1)
	}
	fmt.Println("   Connected!")

	// Wait for A to accept
	connA := <-connACh
	defer connA.Close("example complete")

	fmt.Println("5. Testing Connection Extras...")
	fmt.Println()

	// RemoteID: A gets B's ID
	remoteID, err := connA.RemoteID()
	if err != nil {
		fmt.Fprintf(os.Stderr, "   RemoteID error: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("   RemoteID: %.8s... (expected: %.8s...)\n", remoteID, epBID)
	if remoteID == epBID {
		fmt.Println("   ✓ RemoteID matches!")
	} else {
		fmt.Println("   ✗ RemoteID mismatch!")
	}
	fmt.Println()

	// MaxDatagramSize
	maxSize, isSet, err := connB.MaxDatagramSize()
	if err != nil {
		fmt.Fprintf(os.Stderr, "   MaxDatagramSize error: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("   MaxDatagramSize: %d bytes, configured: %v\n", maxSize, isSet)
	if isSet && maxSize > 0 {
		fmt.Println("   ✓ Datagrams supported!")
	}
	fmt.Println()

	// Connection Info
	info, err := connB.Info()
	if err != nil {
		fmt.Fprintf(os.Stderr, "   ConnectionInfo error: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("   ConnectionInfo: type=%d, sent=%d, received=%d\n",
		info.ConnectionType, info.BytesSent, info.BytesReceived)
	fmt.Println()

	// Close connection
	fmt.Println("6. Closing connection...")
	err = connB.Close("example complete")
	if err != nil {
		fmt.Fprintf(os.Stderr, "   Close error: %v\n", err)
		os.Exit(1)
	}
	fmt.Println("   ✓ Connection closed!")
	fmt.Println()

	fmt.Println("=== SUCCESS ===")
}
