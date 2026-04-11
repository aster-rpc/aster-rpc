//go:build cgo

package aster

import (
	"testing"
)

func TestRemoteEndpointInfo(t *testing.T) {
	info := &RemoteEndpointInfo{
		NodeID:          "abcd1234" + string(make([]byte, 56)),
		IsConnected:     true,
		ConnectionType:  1,
		RelayURL:        "https://relay.example.com",
		LastHandshakeNS: 1234567890,
		BytesSent:       1024,
		BytesReceived:   2048,
	}

	if info.NodeID != "abcd1234"+string(make([]byte, 56)) {
		t.Errorf("expected NodeID with 64 chars, got %s", info.NodeID)
	}
	if !info.IsConnected {
		t.Errorf("expected IsConnected to be true")
	}
	if info.ConnectionType != 1 {
		t.Errorf("expected ConnectionType 1, got %d", info.ConnectionType)
	}
	if info.RelayURL != "https://relay.example.com" {
		t.Errorf("expected RelayURL, got %s", info.RelayURL)
	}
	if info.LastHandshakeNS != 1234567890 {
		t.Errorf("expected LastHandshakeNS 1234567890, got %d", info.LastHandshakeNS)
	}
	if info.BytesSent != 1024 {
		t.Errorf("expected BytesSent 1024, got %d", info.BytesSent)
	}
	if info.BytesReceived != 2048 {
		t.Errorf("expected BytesReceived 2048, got %d", info.BytesReceived)
	}
}

func TestTransportMetrics(t *testing.T) {
	metrics := &TransportMetrics{
		SendIPv4:          100,
		SendIPv6:          200,
		SendRelay:         300,
		RecvDataIPv4:      400,
		RecvDataIPv6:      500,
		RecvDataRelay:     600,
		RecvDatagrams:     700,
		NumConnsDirect:    10,
		NumConnsOpened:    20,
		NumConnsClosed:    5,
		PathsDirect:       3,
		PathsRelay:        2,
		HolepunchAttempts: 1,
		RelayHomeChange:   0,
	}

	if metrics.SendIPv4 != 100 {
		t.Errorf("expected SendIPv4 100, got %d", metrics.SendIPv4)
	}
	if metrics.SendIPv6 != 200 {
		t.Errorf("expected SendIPv6 200, got %d", metrics.SendIPv6)
	}
	if metrics.SendRelay != 300 {
		t.Errorf("expected SendRelay 300, got %d", metrics.SendRelay)
	}
	if metrics.RecvDataIPv4 != 400 {
		t.Errorf("expected RecvDataIPv4 400, got %d", metrics.RecvDataIPv4)
	}
	if metrics.RecvDataIPv6 != 500 {
		t.Errorf("expected RecvDataIPv6 500, got %d", metrics.RecvDataIPv6)
	}
	if metrics.RecvDataRelay != 600 {
		t.Errorf("expected RecvDataRelay 600, got %d", metrics.RecvDataRelay)
	}
	if metrics.RecvDatagrams != 700 {
		t.Errorf("expected RecvDatagrams 700, got %d", metrics.RecvDatagrams)
	}
	if metrics.NumConnsDirect != 10 {
		t.Errorf("expected NumConnsDirect 10, got %d", metrics.NumConnsDirect)
	}
	if metrics.NumConnsOpened != 20 {
		t.Errorf("expected NumConnsOpened 20, got %d", metrics.NumConnsOpened)
	}
	if metrics.NumConnsClosed != 5 {
		t.Errorf("expected NumConnsClosed 5, got %d", metrics.NumConnsClosed)
	}
	if metrics.PathsDirect != 3 {
		t.Errorf("expected PathsDirect 3, got %d", metrics.PathsDirect)
	}
	if metrics.PathsRelay != 2 {
		t.Errorf("expected PathsRelay 2, got %d", metrics.PathsRelay)
	}
	if metrics.HolepunchAttempts != 1 {
		t.Errorf("expected HolepunchAttempts 1, got %d", metrics.HolepunchAttempts)
	}
	if metrics.RelayHomeChange != 0 {
		t.Errorf("expected RelayHomeChange 0, got %d", metrics.RelayHomeChange)
	}
}

func TestRemoteEndpointInfoNotConnected(t *testing.T) {
	info := &RemoteEndpointInfo{
		NodeID:      "notconnected" + string(make([]byte, 51)),
		IsConnected: false,
	}

	if info.IsConnected {
		t.Errorf("expected IsConnected to be false")
	}
}
