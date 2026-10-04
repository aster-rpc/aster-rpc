package aster

import (
	"fmt"
	"reflect"
	"strings"
	"sync"

	forygo "github.com/apache/fory/go/fory"
)

// Codec is the payload serialization used by AsterServer / AsterClient.
// Envelopes (StreamHeader / RpcStatus) never go through it: they are AEv1,
// encoded by the C ABI.
type Codec interface {
	// Mode tag (e.g. "xlang", "raw").
	Mode() string

	// Encode a value to bytes.
	Encode(value any) ([]byte, error)

	// Decode bytes into dest, a non-nil pointer.
	Decode(payload []byte, dest any) error
}

// WireTyped is implemented by payload types to declare their Aster wire name,
// "namespace/Name" (e.g. "interop/EchoRequest"). The name is both the Fory
// registration name and the contract TypeDef (package, name). Implement it
// on the value receiver:
//
//	func (EchoRequest) AsterWireType() string { return "interop/EchoRequest" }
type WireTyped interface {
	AsterWireType() string
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
	if b, ok := value.(*[]byte); ok && b != nil {
		return *b, nil
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

// ForyCodec is the Apache Fory 1.7.6 payload codec every Aster binding uses:
// xlang, compatible (schema evolution), no reference tracking. Struct types
// are registered by name ("namespace/Name" -> Fory namespace + type name),
// either explicitly with Register or automatically for WireTyped values.
// Safe for concurrent use.
type ForyCodec struct {
	mu         sync.Mutex
	fory       *forygo.Fory
	registered map[reflect.Type]string
}

// NewForyCodec returns a Fory codec configured like every other binding:
// xlang=true, compatible=true, ref tracking off.
func NewForyCodec() *ForyCodec {
	return &ForyCodec{
		fory:       forygo.New(forygo.WithXlang(true), forygo.WithCompatible(true), forygo.WithTrackRef(false)),
		registered: make(map[reflect.Type]string),
	}
}

// NewForyCodecFrom wraps an externally-built Fory instance (the caller is
// responsible for its xlang/compatible settings).
func NewForyCodecFrom(fory *forygo.Fory) *ForyCodec {
	return &ForyCodec{fory: fory, registered: make(map[reflect.Type]string)}
}

// Fory returns the underlying Fory instance. It is not thread-safe: use it
// only for registration before the codec is shared.
func (c *ForyCodec) Fory() *forygo.Fory { return c.fory }

func (c *ForyCodec) Mode() string { return "xlang" }

// Register registers the struct type of v (a value, pointer or reflect.Type)
// under the Aster wire name tag ("namespace/Name"). Registering the same type
// again under the same name is a no-op.
func (c *ForyCodec) Register(v any, tag string) error {
	t := structType(v)
	if t == nil {
		return fmt.Errorf("aster: Register needs a struct type, got %T", v)
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.registerLocked(t, tag)
}

func (c *ForyCodec) registerLocked(t reflect.Type, tag string) error {
	if prev, ok := c.registered[t]; ok {
		if prev == tag {
			return nil
		}
		return fmt.Errorf("aster: %v already registered as %q", t, prev)
	}
	if err := c.fory.RegisterStructByName(t, foryName(tag)); err != nil {
		return fmt.Errorf("aster: register %v as %q: %w", t, tag, err)
	}
	c.registered[t] = tag
	return nil
}

// RegisterWireTypes registers t and every WireTyped struct reachable from its
// fields.
func (c *ForyCodec) RegisterWireTypes(v any) error {
	t := structType(v)
	if t == nil {
		return nil
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.registerGraph(t, map[reflect.Type]bool{})
}

func (c *ForyCodec) registerGraph(t reflect.Type, seen map[reflect.Type]bool) error {
	for t.Kind() == reflect.Pointer || t.Kind() == reflect.Slice || t.Kind() == reflect.Array || t.Kind() == reflect.Map {
		if t.Kind() == reflect.Map {
			if err := c.registerGraph(t.Key(), seen); err != nil {
				return err
			}
		}
		t = t.Elem()
	}
	if t.Kind() != reflect.Struct || seen[t] {
		return nil
	}
	seen[t] = true
	if tag, ok := wireTypeOf(t); ok {
		if err := c.registerLocked(t, tag); err != nil {
			return err
		}
	}
	for i := 0; i < t.NumField(); i++ {
		f := t.Field(i)
		if f.IsExported() {
			if err := c.registerGraph(f.Type, seen); err != nil {
				return err
			}
		}
	}
	return nil
}

func (c *ForyCodec) Encode(value any) ([]byte, error) {
	if value == nil {
		return []byte{}, nil
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	if t := structType(value); t != nil {
		if _, ok := c.registered[t]; !ok {
			if err := c.registerGraph(t, map[reflect.Type]bool{}); err != nil {
				return nil, err
			}
		}
	}
	if rv := reflect.ValueOf(value); rv.Kind() == reflect.Struct {
		// Fory serializes structs through a pointer.
		p := reflect.New(rv.Type())
		p.Elem().Set(rv)
		value = p.Interface()
	}
	b, err := c.fory.Marshal(value)
	if err != nil {
		return nil, err
	}
	return append([]byte(nil), b...), nil
}

func (c *ForyCodec) Decode(payload []byte, dest any) error {
	if len(payload) == 0 {
		// Nothing to decode; leave dest at its zero value.
		v := reflect.ValueOf(dest)
		if v.Kind() == reflect.Pointer && !v.IsNil() {
			v.Elem().Set(reflect.Zero(v.Elem().Type()))
		}
		return nil
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	if t := structType(dest); t != nil {
		if _, ok := c.registered[t]; !ok {
			if err := c.registerGraph(t, map[reflect.Type]bool{}); err != nil {
				return err
			}
		}
	}
	return c.fory.Unmarshal(payload, dest)
}

// foryName maps "namespace/Name" to Fory's "namespace.Name" (Fory splits the
// namespace at the last '.').
func foryName(tag string) string {
	if i := strings.LastIndexByte(tag, '/'); i >= 0 {
		return tag[:i] + "." + tag[i+1:]
	}
	return tag
}

// splitWireType splits "namespace/Name" into (namespace, Name).
func splitWireType(tag string) (string, string) {
	if i := strings.LastIndexByte(tag, '/'); i >= 0 {
		return tag[:i], tag[i+1:]
	}
	return "", tag
}

func structType(v any) reflect.Type {
	t, ok := v.(reflect.Type)
	if !ok {
		t = reflect.TypeOf(v)
	}
	for t != nil && t.Kind() == reflect.Pointer {
		t = t.Elem()
	}
	if t == nil || t.Kind() != reflect.Struct {
		return nil
	}
	return t
}

var wireTypedType = reflect.TypeOf((*WireTyped)(nil)).Elem()

// wireTypeOf returns the AsterWireType of struct type t, if it has one.
func wireTypeOf(t reflect.Type) (string, bool) {
	if t.Implements(wireTypedType) {
		return reflect.Zero(t).Interface().(WireTyped).AsterWireType(), true
	}
	if reflect.PointerTo(t).Implements(wireTypedType) {
		return reflect.New(t).Interface().(WireTyped).AsterWireType(), true
	}
	return "", false
}
