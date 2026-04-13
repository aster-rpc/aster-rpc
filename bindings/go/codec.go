package aster

import (
	"fmt"
	"reflect"

	forygo "github.com/apache/fory/go/fory"
)

// Codec is the wire serialization indirection. Aster supports multiple
// modes (raw bytes, Fory cross-language, JSON for tests). The active
// codec advertises its mode string in the registry lease's
// SerializationModes list so callers know how to encode requests.
type Codec interface {
	// Mode tag matching the registry contract (e.g. "raw", "fory-xlang").
	// AsterServer publishes this in its lease so clients can pick a
	// compatible codec via the standard mandatory filters.
	Mode() string

	// Encode a value to bytes.
	Encode(value any) ([]byte, error)

	// Decode bytes back into the destination pointer. dest must be a
	// non-nil pointer to a value of the expected type.
	Decode(payload []byte, dest any) error
}

// RawBytesCodec is a pass-through codec: only accepts []byte values,
// returns them as-is. Useful for opaque-payload services and tests
// where the host owns the wire format end-to-end.
type RawBytesCodec struct{}

func (RawBytesCodec) Mode() string { return "raw" }

func (RawBytesCodec) Encode(value any) ([]byte, error) {
	if value == nil {
		return []byte{}, nil
	}
	if b, ok := value.([]byte); ok {
		return b, nil
	}
	return nil, fmt.Errorf("RawBytesCodec only accepts []byte; got %T", value)
}

func (RawBytesCodec) Decode(payload []byte, dest any) error {
	bp, ok := dest.(*[]byte)
	if !ok {
		return fmt.Errorf("RawBytesCodec only decodes to *[]byte; got %T", dest)
	}
	*bp = payload
	return nil
}

// ForyCodec is an Apache Fory v0.16 backed codec. Exposes the
// underlying Fory instance so the host (or eventually a
// decorator-driven generator) can register the contract types it needs
// to serialize. Type registration is the caller's responsibility — this
// struct only owns the encode/decode pump and the mode tag the registry
// advertises.
type ForyCodec struct {
	fory *forygo.Fory
}

// NewForyCodec returns a Fory codec configured for cross-language mode
// with reference tracking enabled.
func NewForyCodec() *ForyCodec {
	return &ForyCodec{fory: forygo.NewFory(forygo.WithXlang(true))}
}

// NewForyCodecFrom wraps an externally-built Fory instance.
func NewForyCodecFrom(fory *forygo.Fory) *ForyCodec {
	return &ForyCodec{fory: fory}
}

// Fory returns the underlying Fory instance. Use this to register
// contract types via fory.RegisterNamedStruct / RegisterStruct before
// serializing them.
func (c *ForyCodec) Fory() *forygo.Fory { return c.fory }

func (c *ForyCodec) Mode() string { return "fory-xlang" }

func (c *ForyCodec) Encode(value any) ([]byte, error) {
	if value == nil {
		return []byte{}, nil
	}
	return c.fory.Marshal(value)
}

func (c *ForyCodec) Decode(payload []byte, dest any) error {
	if len(payload) == 0 {
		// Nothing to decode; leave dest at its zero value via reflect.
		v := reflect.ValueOf(dest)
		if v.Kind() == reflect.Ptr && !v.IsNil() {
			v.Elem().Set(reflect.Zero(v.Elem().Type()))
		}
		return nil
	}
	return c.fory.Unmarshal(payload, dest)
}
