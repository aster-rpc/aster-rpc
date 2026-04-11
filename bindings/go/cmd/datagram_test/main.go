//go:build cgo

// Test datagram send and receive between two endpoints.
// Validates that iroh_connection_read_datagram emits BYTES_RESULT (not DATAGRAM_RECEIVED).
package main

import (
	"context"
	"fmt"
	"os"
	"strings"
	"time"

	aster "aster-ffi"
)

const ALPN = "aster"

func main() {
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()

	// Create endpoint config with ALPN for A (listener)
	cfgA := aster.EndpointConfig{
		ALPNs: []string{ALPN},
	}

	// Create runtime and endpoint for A
	fmt.Println("1. Creating endpoint A (listener)...")
	runtimeA, err := aster.NewRuntime(ctx, aster.DefaultRuntimeConfig())
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error creating runtime A: %v\n", err)
		os.Exit(1)
	}
	defer runtimeA.Close()

	endpointA, err := aster.NewEndpoint(ctx, runtimeA, cfgA)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error creating endpoint A: %v\n", err)
		os.Exit(1)
	}
	defer endpointA.Close(ctx)

	epAID, err := endpointA.NodeID()
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error getting endpoint A ID: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("   Endpoint A created! ID: %s\n\n", shortID(epAID))

	// Create runtime and endpoint for B (dialer)
	cfgB := aster.EndpointConfig{
		ALPNs: []string{ALPN},
	}

	fmt.Println("2. Creating endpoint B (dialer)...")
	runtimeB, err := aster.NewRuntime(ctx, aster.DefaultRuntimeConfig())
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error creating runtime B: %v\n", err)
		os.Exit(1)
	}
	defer runtimeB.Close()

	endpointB, err := aster.NewEndpoint(ctx, runtimeB, cfgB)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error creating endpoint B: %v\n", err)
		os.Exit(1)
	}
	defer endpointB.Close(ctx)

	epBID, err := endpointB.NodeID()
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error getting endpoint B ID: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("   Endpoint B created! ID: %s\n\n", shortID(epBID))

	// A accepts connections in background
	connACh := make(chan *aster.Connection, 1)
	go func() {
		fmt.Println("3. A accepting connections...")
		c, err := endpointA.Accept(ctx)
		if err != nil {
			fmt.Fprintf(os.Stderr, "Accept error on A: %v\n", err)
			return
		}
		fmt.Printf("   A accepted connection!\n")
		connACh <- c
	}()

	// Give A time to start accepting
	time.Sleep(100 * time.Millisecond)

	// B connects to A
	fmt.Println("4. B connecting to A...")
	addrA, err := endpointA.AddrInfo()
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error getting endpoint A addr: %v\n", err)
		os.Exit(1)
	}

	connB, err := endpointB.ConnectNodeAddr(ctx, addrA, ALPN)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error connecting B to A: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("   B connected!\n\n")

	// Wait for accept to complete
	var connA *aster.Connection
	select {
	case connA = <-connACh:
	case <-time.After(5 * time.Second):
		fmt.Fprintf(os.Stderr, "Timeout waiting for accept\n")
		os.Exit(1)
	}
	defer connA.Close("")

	// Check datagram support
	fmt.Println("5. Checking datagram support...")
	maxSize, isSet, err := connB.MaxDatagramSize()
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error getting max datagram size: %v\n", err)
		os.Exit(1)
	}
	if !isSet || maxSize == 0 {
		fmt.Println("   Datagrams not supported on this connection, skipping test")
		os.Exit(0)
	}
	fmt.Printf("   Max datagram size: %d bytes\n\n", maxSize)

	// Start reading datagram on A (non-blocking)
	fmt.Println("6. A starting datagram reader...")
	datagramCh := make(chan []byte, 1)
	errCh := make(chan error, 1)
	go func() {
		data, err := connA.ReadDatagram(ctx)
		if err != nil {
			errCh <- err
			return
		}
		datagramCh <- data
	}()

	// Give reader time to register
	time.Sleep(50 * time.Millisecond)

	// B sends a datagram to A
	testData := []byte("Hello Datagram!")
	fmt.Printf("7. B sending datagram: %q\n", testData)
	err = connB.SendDatagram(testData)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error sending datagram: %v\n", err)
		os.Exit(1)
	}
	fmt.Println("   Datagram sent!")

	// Wait for A to receive
	select {
	case received := <-datagramCh:
		fmt.Printf("   A received datagram: %q\n", received)
		if string(received) == string(testData) {
			fmt.Println("   ✓ Datagram data matches!")
		} else {
			fmt.Fprintf(os.Stderr, "   ✗ Datagram mismatch! expected %q, got %q\n", testData, received)
			os.Exit(1)
		}
	case err := <-errCh:
		fmt.Fprintf(os.Stderr, "   ✗ ReadDatagram failed: %v\n", err)
		os.Exit(1)
	case <-time.After(5 * time.Second):
		fmt.Fprintf(os.Stderr, "   ✗ Timeout waiting for datagram\n")
		os.Exit(1)
	}

	// Close connections
	fmt.Println("\n8. Closing connections...")
	connB.Close("")
	connA.Close("")

	fmt.Println("\n=== SUCCESS: Datagram send/receive works! ===")
}

func shortID(id string) string {
	if len(id) > 16 {
		return id[:16] + "..."
	}
	return id
}
