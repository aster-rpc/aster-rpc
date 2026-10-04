//go:build cgo

package aster

import (
	"encoding/json"
	"fmt"
	"reflect"
	"sort"
	"strings"

	forygo "github.com/apache/fory/go/fory"
)

// Contract identity (Aster-ContractIdentity §11.3): the TypeDef /
// ServiceContract JSON is built here from Go types and handed to Rust, which
// canonicalises and hashes it (aster_canonical_bytes, aster_blake3_hex,
// aster_contract_id). No canonical bytes or hashes are computed in Go.
//
// Go -> contract primitive (the same table as Rust, Java and .NET):
//
//	string string, bool bool, int8 int8, int16 int16, int32 varint32,
//	int64/int varint64, uint8 uint8, uint16 uint16, uint32 var_uint32,
//	uint64/uint var_uint64, float32 float32, float64 float64, []byte binary.
//
// A `fory:"encoding=fixed"` tag selects int32/int64/uint32/uint64 (and
// "tagged" tagged_int64/tagged_uint64), matching what Fory writes. Slices and
// arrays become lists, maps maps; WireTyped structs become refs; pointer
// fields are optional. A []byte field must be tagged `fory:"type=bytes"`
// (binary; untagged, Fory Go writes list<uint8>) or `aster:"uint8_list"`
// (list<uint8>). Field names are Fory's snake_case of the Go name.

// ServiceContractJSON returns the ServiceContract JSON handed to aster_contract_id.
func ServiceContractJSON(svc *Service) (string, error) {
	r := newTypeResolver()
	type methodJSON struct {
		Name           string  `json:"name"`
		Pattern        string  `json:"pattern"`
		RequestType    string  `json:"request_type"`
		ResponseType   string  `json:"response_type"`
		Idempotent     bool    `json:"idempotent"`
		DefaultTimeout float64 `json:"default_timeout"`
		Requires       *string `json:"requires"`
	}
	methods := make([]*Method, 0, len(svc.methods))
	for _, m := range svc.methods {
		methods = append(methods, m)
	}
	// Rust canonicalisation re-sorts methods by NFC name; sort for stable JSON.
	sort.Slice(methods, func(i, j int) bool { return methods[i].Name < methods[j].Name })
	out := make([]methodJSON, 0, len(methods))
	for _, m := range methods {
		req, err := r.hash(m.reqType)
		if err != nil {
			return "", fmt.Errorf("%s.%s request: %w", svc.Name, m.Name, err)
		}
		resp, err := r.hash(m.respType)
		if err != nil {
			return "", fmt.Errorf("%s.%s response: %w", svc.Name, m.Name, err)
		}
		out = append(out, methodJSON{
			Name: m.Name, Pattern: m.Pattern.String(), RequestType: req, ResponseType: resp,
			Idempotent: m.Idempotent,
		})
	}
	scope := "shared"
	if svc.Scope == ScopeSession {
		scope = "session"
	}
	b, err := json.Marshal(struct {
		Name               string       `json:"name"`
		Version            int32        `json:"version"`
		Methods            []methodJSON `json:"methods"`
		SerializationModes []string     `json:"serialization_modes"`
		Scoped             string       `json:"scoped"`
		Requires           *string      `json:"requires"`
		ProducerLanguage   string       `json:"producer_language"`
	}{svc.Name, svc.Version, out, []string{"xlang"}, scope, nil, ""})
	return string(b), err
}

// TypeDefJSON returns the TypeDef JSON of a WireTyped struct type (v may be a
// value, pointer or reflect.Type).
func TypeDefJSON(v any) (string, error) {
	t := structType(v)
	if t == nil {
		return "", fmt.Errorf("aster: TypeDefJSON needs a struct type, got %T", v)
	}
	return newTypeResolver().typeDefJSON(t)
}

// TypeHash returns the 64-hex BLAKE3 of the canonical TypeDef of a WireTyped
// struct type.
func TypeHash(v any) (string, error) {
	t := structType(v)
	if t == nil {
		return "", fmt.Errorf("aster: TypeHash needs a struct type, got %T", v)
	}
	return newTypeResolver().hash(t)
}

type typeResolver struct {
	hashes     map[reflect.Type]string
	inProgress map[reflect.Type]bool
}

func newTypeResolver() *typeResolver {
	return &typeResolver{hashes: map[reflect.Type]string{}, inProgress: map[reflect.Type]bool{}}
}

func (r *typeResolver) hash(t reflect.Type) (string, error) {
	for t.Kind() == reflect.Pointer {
		t = t.Elem()
	}
	if h, ok := r.hashes[t]; ok {
		return h, nil
	}
	if r.inProgress[t] {
		return "", fmt.Errorf("recursive type graphs are not supported yet (%v)", t)
	}
	r.inProgress[t] = true
	defer delete(r.inProgress, t)
	js, err := r.typeDefJSON(t)
	if err != nil {
		return "", err
	}
	canon, err := ComputeCanonicalBytes("TypeDef", js)
	if err != nil {
		return "", err
	}
	h, err := Blake3Hex(canon)
	if err != nil {
		return "", err
	}
	r.hashes[t] = h
	return h, nil
}

type fieldJSON struct {
	ID                    int    `json:"id"`
	Name                  string `json:"name"`
	TypeKind              string `json:"type_kind"`
	TypePrimitive         string `json:"type_primitive"`
	TypeRef               string `json:"type_ref"`
	SelfRefName           string `json:"self_ref_name"`
	Optional              bool   `json:"optional"`
	RefTracked            bool   `json:"ref_tracked"`
	Container             string `json:"container"`
	ContainerKeyKind      string `json:"container_key_kind"`
	ContainerKeyPrimitive string `json:"container_key_primitive"`
	ContainerKeyRef       string `json:"container_key_ref"`
	Required              bool   `json:"required"`
	DefaultValue          string `json:"default_value"`
}

func (r *typeResolver) typeDefJSON(t reflect.Type) (string, error) {
	tag, ok := wireTypeOf(t)
	if !ok {
		return "", fmt.Errorf("%v does not implement AsterWireType() (aster.WireTyped)", t)
	}
	pkg, name := splitWireType(tag)
	fields := []fieldJSON{}
	for i := 0; i < t.NumField(); i++ {
		sf := t.Field(i)
		if !sf.IsExported() || foryIgnored(sf) {
			continue
		}
		f, err := r.field(sf)
		if err != nil {
			return "", fmt.Errorf("%v.%s: %w", t, sf.Name, err)
		}
		fields = append(fields, f)
	}
	b, err := json.Marshal(struct {
		Kind          string      `json:"kind"`
		Package       string      `json:"package"`
		Name          string      `json:"name"`
		Fields        []fieldJSON `json:"fields"`
		EnumValues    []struct{}  `json:"enum_values"`
		UnionVariants []struct{}  `json:"union_variants"`
	}{"message", pkg, name, fields, []struct{}{}, []struct{}{}})
	return string(b), err
}

func foryIgnored(sf reflect.StructField) bool {
	tag, ok := sf.Tag.Lookup("fory")
	if !ok {
		return false
	}
	if tag == "-" {
		return true
	}
	for _, part := range strings.Split(tag, ",") {
		switch strings.TrimSpace(part) {
		case "ignore", "ignore=true":
			return true
		}
	}
	return false
}

// foryTypeHint reads a `fory:"type=..."` tag.
func foryTypeHint(sf reflect.StructField) string {
	for _, part := range strings.Split(sf.Tag.Get("fory"), ",") {
		k, v, ok := strings.Cut(strings.TrimSpace(part), "=")
		if ok && strings.TrimSpace(k) == "type" {
			return strings.ToLower(strings.TrimSpace(v))
		}
	}
	return ""
}

func foryEncoding(sf reflect.StructField) string {
	for _, part := range strings.Split(sf.Tag.Get("fory"), ",") {
		k, v, ok := strings.Cut(strings.TrimSpace(part), "=")
		if ok && strings.TrimSpace(k) == "encoding" {
			return strings.ToLower(strings.TrimSpace(v))
		}
	}
	return ""
}

func (r *typeResolver) field(sf reflect.StructField) (fieldJSON, error) {
	t := sf.Type
	optional := false
	if t.Kind() == reflect.Pointer {
		t = t.Elem()
		optional = true
	}
	// Fory Go writes a []byte struct field as list<uint8> unless it is
	// tagged `fory:"type=bytes"` (binary, what every other binding means by
	// bytes). Make the choice explicit so the contract matches the wire.
	if isBytes(t) {
		switch {
		case foryTypeHint(sf) == "bytes":
		case sf.Tag.Get("aster") == "uint8_list":
			return fieldJSON{
				Name: forygo.SnakeCase(sf.Name), TypeKind: "primitive", TypePrimitive: "uint8",
				Optional: optional, Container: "list", ContainerKeyKind: "primitive", Required: true,
			}, nil
		default:
			return fieldJSON{}, fmt.Errorf("[]byte field needs `fory:\"type=bytes\"` (binary, as in the other bindings) " +
				"or `aster:\"uint8_list\"` with no fory type hint (list<uint8>)")
		}
	}
	container := "none"
	value := t
	var key reflect.Type
	if !isBytes(t) {
		switch t.Kind() {
		case reflect.Slice, reflect.Array:
			container, value = "list", t.Elem()
		case reflect.Map:
			if t.Elem().Kind() == reflect.Struct && t.Elem().NumField() == 0 {
				container, value = "set", t.Key()
			} else {
				container, key, value = "map", t.Key(), t.Elem()
			}
		}
	}
	enc := ""
	if container == "none" {
		enc = foryEncoding(sf)
	}
	kind, prim, ref, err := r.leaf(value, enc)
	if err != nil {
		return fieldJSON{}, err
	}
	f := fieldJSON{
		Name: forygo.SnakeCase(sf.Name), TypeKind: kind, TypePrimitive: prim, TypeRef: ref,
		Optional: optional, Container: container,
		ContainerKeyKind: "primitive", Required: true,
	}
	if key != nil {
		kk, kp, kr, err := r.leaf(key, "")
		if err != nil {
			return fieldJSON{}, err
		}
		f.ContainerKeyKind, f.ContainerKeyPrimitive, f.ContainerKeyRef = kk, kp, kr
	}
	return f, nil
}

func isBytes(t reflect.Type) bool {
	return t.Kind() == reflect.Slice && t.Elem().Kind() == reflect.Uint8
}

func (r *typeResolver) leaf(t reflect.Type, enc string) (kind, prim, ref string, err error) {
	if t.Kind() == reflect.Pointer {
		t = t.Elem()
	}
	if p := primitiveName(t, enc); p != "" {
		return "primitive", p, "", nil
	}
	if t.Kind() == reflect.Interface {
		return "any", "", "", nil
	}
	if t.Kind() == reflect.Struct {
		if _, ok := wireTypeOf(t); ok {
			h, err := r.hash(t)
			return "ref", "", h, err
		}
	}
	return "", "", "", fmt.Errorf("type %v has no contract mapping (implement aster.WireTyped or use a supported primitive)", t)
}

func primitiveName(t reflect.Type, enc string) string {
	if isBytes(t) {
		return "binary"
	}
	switch t.Kind() {
	case reflect.String:
		return "string"
	case reflect.Bool:
		return "bool"
	case reflect.Int8:
		return "int8"
	case reflect.Int16:
		return "int16"
	case reflect.Int32:
		if enc == "fixed" {
			return "int32"
		}
		return "varint32"
	case reflect.Int64, reflect.Int:
		switch enc {
		case "fixed":
			return "int64"
		case "tagged":
			return "tagged_int64"
		}
		return "varint64"
	case reflect.Uint8:
		return "uint8"
	case reflect.Uint16:
		return "uint16"
	case reflect.Uint32:
		if enc == "fixed" {
			return "uint32"
		}
		return "var_uint32"
	case reflect.Uint64, reflect.Uint:
		switch enc {
		case "fixed":
			return "uint64"
		case "tagged":
			return "tagged_uint64"
		}
		return "var_uint64"
	case reflect.Float32:
		return "float32"
	case reflect.Float64:
		return "float64"
	}
	return ""
}
