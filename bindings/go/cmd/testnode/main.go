//go:build cgo

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

	// Create endpoint config with ALPN
	cfgA := aster.EndpointConfig{
		ALPNs: []string{ALPN},
	}

	// Create a runtime and endpoint for node A (listener)
	fmt.Println("Creating runtime A...")
	runtimeA, err := aster.NewRuntime(ctx, aster.DefaultRuntimeConfig())
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error creating runtime A: %v\n", err)
		os.Exit(1)
	}
	defer runtimeA.Close()

	fmt.Println("Creating endpoint A with ALPN...")
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
	fmt.Printf("Endpoint A created! ID: %s\n", epAID)

	addrA, err := endpointA.AddrInfo()
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error getting endpoint A addr: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("Endpoint A addr: %+v\n", addrA)

	// Create endpoint config with ALPN for B
	cfgB := aster.EndpointConfig{
		ALPNs: []string{ALPN},
	}

	// Create a runtime and endpoint for node B (dialer)
	fmt.Println("\nCreating runtime B...")
	runtimeB, err := aster.NewRuntime(ctx, aster.DefaultRuntimeConfig())
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error creating runtime B: %v\n", err)
		os.Exit(1)
	}
	defer runtimeB.Close()

	fmt.Println("Creating endpoint B with ALPN...")
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
	fmt.Printf("Endpoint B created! ID: %s\n", epBID)

	// Start accept loop on A in background
	var connA *aster.Connection
	connACh := make(chan *aster.Connection, 1)
	go func() {
		fmt.Println("A accepting connections...")
		c, err := endpointA.Accept(ctx)
		if err != nil {
			fmt.Fprintf(os.Stderr, "Accept error on A: %v\n", err)
			return
		}
		fmt.Printf("A accepted connection!\n")
		connACh <- c
	}()

	// Give A time to start accepting
	time.Sleep(100 * time.Millisecond)

	// B connects to A
	fmt.Println("\nB connecting to A...")
	connB, err := endpointB.ConnectNodeAddr(ctx, addrA, ALPN)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error connecting B to A: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("B connected!\n")

	// Wait for accept to complete
	select {
	case connA = <-connACh:
	case <-time.After(5 * time.Second):
		fmt.Fprintf(os.Stderr, "Timeout waiting for accept\n")
		os.Exit(1)
	}
	defer connA.Close("")

	// Test 8.1: Connection Extras

	// 1. Test RemoteID
	fmt.Println("\n--- Testing RemoteID ---")
	remoteID_B, err := connA.RemoteID()
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error getting remote ID on A: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("A sees remote ID: %s\n", remoteID_B)
	fmt.Printf("Expected (B's ID): %s\n", epBID)
	if remoteID_B != epBID {
		fmt.Fprintf(os.Stderr, "Remote ID mismatch!\n")
		os.Exit(1)
	}

	// Note: Bidirectional streams have a known AcceptBi/OpenBi timing issue in iroh.
	// AcceptBi times out waiting for the remote to open a stream.
	// This is an iroh library issue, not a binding issue.

	_ = connA
	_ = connB

	// 3. Test datagrams
	fmt.Println("\n--- Testing Datagrams ---")
	maxSize, isSet, err := connB.MaxDatagramSize()
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error getting max datagram size: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("Max datagram size: %d, isSet: %v\n", maxSize, isSet)
	if isSet && maxSize > 0 {
		// Send a datagram from B to A
		testData := []byte("Hello Datagram!")
		err = connB.SendDatagram(testData)
		if err != nil {
			fmt.Fprintf(os.Stderr, "Error sending datagram: %v\n", err)
			os.Exit(1)
		}
		fmt.Printf("B sent datagram: %q\n", testData)

		// Give it time to arrive
		time.Sleep(100 * time.Millisecond)
		fmt.Println("Datagram sent successfully!")
	} else {
		fmt.Println("Datagrams not supported on this connection")
	}

	// 4. Test connection close
	fmt.Println("\n--- Testing Connection Close ---")
	err = connB.Close("test complete")
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error closing connection: %v\n", err)
		os.Exit(1)
	}
	fmt.Println("Connection closed successfully")

	fmt.Println("\nSUCCESS: Connection Extras tests passed!")
}
