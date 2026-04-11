//go:build cgo

// Simple stream test - tests OpenBi, Write, Read on Go side
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

	fmt.Println("=== Stream Test ===")

	// Create runtime A and endpoint A (listener)
	fmt.Println("1. Creating endpoint A (listener)...")
	runtimeA, err := aster.NewRuntime(ctx, aster.DefaultRuntimeConfig())
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error creating runtime A: %v\n", err)
		os.Exit(1)
	}
	defer runtimeA.Close()

	cfgA := aster.EndpointConfig{
		ALPNs: []string{ALPN},
	}
	endpointA, err := aster.NewEndpoint(ctx, runtimeA, cfgA)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error creating endpoint A: %v\n", err)
		os.Exit(1)
	}
	defer endpointA.Close(ctx)

	addrA, _ := endpointA.AddrInfo()
	fmt.Printf("   Endpoint A created! Relay: %s\n", addrA.RelayURL)

	// Create runtime B and endpoint B (dialer)
	fmt.Println("2. Creating endpoint B (dialer)...")
	runtimeB, err := aster.NewRuntime(ctx, aster.DefaultRuntimeConfig())
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error creating runtime B: %v\n", err)
		os.Exit(1)
	}
	defer runtimeB.Close()

	cfgB := aster.EndpointConfig{
		ALPNs: []string{ALPN},
	}
	endpointB, err := aster.NewEndpoint(ctx, runtimeB, cfgB)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error creating endpoint B: %v\n", err)
		os.Exit(1)
	}
	defer endpointB.Close(ctx)

	// A accepts in background
	connACh := make(chan *aster.Connection, 1)
	go func() {
		fmt.Println("3. A accepting connections...")
		conn, err := endpointA.Accept(ctx)
		if err != nil {
			fmt.Fprintf(os.Stderr, "   Accept error: %v\n", err)
			return
		}
		fmt.Println("   Connection accepted!")
		connACh <- conn
	}()

	// Give A time to start accepting
	time.Sleep(100 * time.Millisecond)

	// B connects to A
	fmt.Println("4. B connecting to A...")
	connB, err := endpointB.ConnectNodeAddr(ctx, addrA, ALPN)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error connecting B to A: %v\n", err)
		os.Exit(1)
	}
	fmt.Println("   Connected!")

	// Wait for accept
	connA := <-connACh
	defer connA.Close("test complete")

	// B opens a bidirectional stream and sends data
	fmt.Println("5. B opening bidirectional stream...")
	sendB, recvB, err := connB.OpenBi(ctx)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error opening bi on B: %v\n", err)
		os.Exit(1)
	}
	fmt.Println("   Stream opened!")

	// B sends data to A
	testData := []byte("Hello Stream!")
	fmt.Printf("6. B sending: %q\n", testData)
	err = sendB.Write(ctx, testData)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error writing: %v\n", err)
		os.Exit(1)
	}
	fmt.Println("   Write completed!")

	// A accepts a bidirectional stream
	fmt.Println("7. A accepting bidirectional stream...")
	sendA, recvA, err := connA.AcceptBi(ctx)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error accepting bi on A: %v\n", err)
		os.Exit(1)
	}
	fmt.Println("   Stream accepted!")

	// A reads the data
	fmt.Println("8. A reading data...")
	data, err := recvA.Read(ctx)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error reading: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("   A received: %q\n", data)

	// Verify data matches
	if string(data) == string(testData) {
		fmt.Println("   ✓ Data matches!")
	} else {
		fmt.Fprintf(os.Stderr, "   ✗ Data mismatch!\n")
		os.Exit(1)
	}

	// A sends response back
	response := []byte("Hello from A!")
	fmt.Printf("9. A sending response: %q\n", response)
	err = sendA.Write(ctx, response)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error writing response: %v\n", err)
		os.Exit(1)
	}

	// B reads the response
	fmt.Println("10. B reading response...")
	resp, err := recvB.Read(ctx)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error reading response: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("   B received: %q\n", resp)

	if string(resp) == string(response) {
		fmt.Println("   ✓ Response matches!")
	} else {
		fmt.Fprintf(os.Stderr, "   ✗ Response mismatch!\n")
		os.Exit(1)
	}

	// Finish streams
	fmt.Println("11. Finishing streams...")
	sendB.Finish(ctx)
	sendA.Finish(ctx)
	fmt.Println("   Streams finished!")

	// Close streams
	sendB.Close()
	recvB.Close()
	sendA.Close()
	recvA.Close()
	fmt.Println("   Streams closed!")

	fmt.Println("\n=== SUCCESS: Bidirectional stream works! ===")
}