//go:build cgo

package aster

import "encoding/binary"

const (
	// AsterALPN is the default ALPN for Aster RPC.
	AsterALPN = "aster/1"

	// Aster wire framing flags.
	FlagCompressed = 0x01
	FlagTrailer    = 0x02
	FlagHeader     = 0x04
	FlagRowSchema  = 0x08
	FlagCall       = 0x10
	FlagCancel     = 0x20
)

// EncodeFrame encodes payload with flags into the Aster wire format:
// [4-byte LE frame_body_len][1-byte flags][payload]
func EncodeFrame(payload []byte, flags byte) []byte {
	frameBodyLen := uint32(1 + len(payload))
	frame := make([]byte, 4+frameBodyLen)
	binary.LittleEndian.PutUint32(frame[0:4], frameBodyLen)
	frame[4] = flags
	copy(frame[5:], payload)
	return frame
}
