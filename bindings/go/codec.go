package aster

import (
	"fmt"
	"reflect"
	"strings"
	"sync"
	"sync/atomic"

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
//
// Safe for concurrent use without a global lock: a Fory instance is not
// thread-safe, so a codec from NewForyCodec keeps a pool of identically
// configured instances, each catching up on registrations (replayed in
// order) when it is taken from the pool. A codec from NewForyCodecFrom, or
// one whose Fory() was taken, serializes through that single instance under
// a mutex.
type ForyCodec struct {
	mu         sync.Mutex   // serializes registration
	fory       *forygo.Fory // registration target; the instance in single mode
	registered map[reflect.Type]string
	order      []foryReg // registrations in order, replayed on pooled instances
	// snap is the read-side copy of (registered, order), replaced on every
	// registration, so encode/decode never take a lock.
	snap atomic.Pointer[regSnapshot]

	newFory func() *forygo.Fory // nil: single-instance mode
	single  atomic.Bool
	singleM sync.Mutex
	pool    sync.Pool // *pooledFory
}

type regSnapshot struct {
	registered map[reflect.Type]struct{}
	order      []foryReg
}

type foryReg struct {
	t    reflect.Type
	name string
}

type pooledFory struct {
	f *forygo.Fory
	n int // registrations applied
}

// NewForyCodec returns a Fory codec configured like every other binding:
// xlang=true, compatible=true, ref tracking off.
func NewForyCodec() *ForyCodec {
	newFory := func() *forygo.Fory {
		return forygo.New(forygo.WithXlang(true), forygo.WithCompatible(true), forygo.WithTrackRef(false))
	}
	return &ForyCodec{fory: newFory(), newFory: newFory, registered: make(map[reflect.Type]string)}
}

// NewForyCodecFrom wraps an externally-built Fory instance (the caller is
// responsible for its xlang/compatible settings). Payloads go through that
// one instance, serialized by a mutex.
func NewForyCodecFrom(fory *forygo.Fory) *ForyCodec {
	c := &ForyCodec{fory: fory, registered: make(map[reflect.Type]string)}
	c.single.Store(true)
	return c
}

// Fory returns the underlying Fory instance. It is not thread-safe: use it
// only for registration before the codec is shared. Taking it switches the
// codec to that single instance (registrations made on it directly are not
// known to the pool).
func (c *ForyCodec) Fory() *forygo.Fory {
	c.single.Store(true)
	return c.fory
}

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
	c.singleM.Lock()
	err := c.fory.RegisterStructByName(t, foryName(tag))
	c.singleM.Unlock()
	if err != nil {
		return fmt.Errorf("aster: register %v as %q: %w", t, tag, err)
	}
	c.registered[t] = tag
	c.order = append(c.order, foryReg{t, foryName(tag)})
	snap := &regSnapshot{registered: make(map[reflect.Type]struct{}, len(c.registered)), order: c.order[:len(c.order):len(c.order)]}
	for k := range c.registered {
		snap.registered[k] = struct{}{}
	}
	c.snap.Store(snap)
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

// ensureRegistered registers v's type graph on first use.
func (c *ForyCodec) ensureRegistered(v any) error {
	t := structType(v)
	if t == nil {
		return nil
	}
	if snap := c.snap.Load(); snap != nil {
		if _, ok := snap.registered[t]; ok {
			return nil
		}
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.registerGraph(t, map[reflect.Type]bool{})
}

// with runs fn on a Fory instance: a pooled one brought up to date with the
// registrations, or the single instance under its mutex.
func (c *ForyCodec) with(fn func(*forygo.Fory) error) error {
	if c.newFory == nil || c.single.Load() {
		c.singleM.Lock()
		defer c.singleM.Unlock()
		return fn(c.fory)
	}
	p, _ := c.pool.Get().(*pooledFory)
	if p == nil {
		p = &pooledFory{f: c.newFory()}
	}
	var pending []foryReg
	if snap := c.snap.Load(); snap != nil && p.n < len(snap.order) {
		pending = snap.order[p.n:]
	}
	for _, r := range pending {
		if err := p.f.RegisterStructByName(r.t, r.name); err != nil {
			return fmt.Errorf("aster: register %v on a pooled Fory: %w", r.t, err)
		}
		p.n++
	}
	err := fn(p.f)
	if err != nil {
		p.f.Reset()
	}
	c.pool.Put(p)
	return err
}

func (c *ForyCodec) Encode(value any) ([]byte, error) {
	if value == nil {
		return []byte{}, nil
	}
	if err := c.ensureRegistered(value); err != nil {
		return nil, err
	}
	if rv := reflect.ValueOf(value); rv.Kind() == reflect.Struct {
		// Fory serializes structs through a pointer.
		p := reflect.New(rv.Type())
		p.Elem().Set(rv)
		value = p.Interface()
	}
	var out []byte
	err := c.with(func(f *forygo.Fory) error {
		b, err := f.Marshal(value)
		if err != nil {
			return err
		}
		out = append([]byte(nil), b...) // the instance reuses its buffer
		return nil
	})
	return out, err
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
	if err := c.ensureRegistered(dest); err != nil {
		return err
	}
	return c.with(func(f *forygo.Fory) error { return f.Unmarshal(payload, dest) })
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
