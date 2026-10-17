//go:build cgo

package aster

import (
	"encoding/json"
	"fmt"
	"reflect"
	"sort"
	"strings"
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
// (list<uint8>). A field's contract name is the Go field name, verbatim, or
// its `aster:"name=..."` override (field_names.go).
//
// The canonical scalars (scalars.go): time.Time timestamp, aster.Date date,
// aster.Decimal decimal (decimal(p,s) with an `aster:"decimal(p,s)"` tag),
// aster.UUID uuid and aster.ULID ulid (or a [16]byte tagged `aster:"uuid"` /
// `aster:"ulid"`; both need a Fory bytes hint). Recursive type graphs are
// supported (contract_graph.go): back-edges become SELF_REF.

// ServiceContractJSON returns the ServiceContract JSON handed to
// aster_contract_id. Service.ContractID uses the registration form
// (DynamicServiceJSON); both give the same id.
func ServiceContractJSON(svc *Service) (string, error) {
	r := newTypeResolver()
	type methodJSON struct {
		Name           string       `json:"name"`
		Pattern        string       `json:"pattern"`
		RequestType    string       `json:"request_type"`
		ResponseType   string       `json:"response_type"`
		Idempotent     bool         `json:"idempotent"`
		DefaultTimeout float64      `json:"default_timeout"`
		Requires       *Requirement `json:"requires"`
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
			Idempotent: m.Idempotent, DefaultTimeout: m.Timeout.Seconds(), Requires: m.Requires,
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
		Requires           *Requirement `json:"requires"`
		ProducerLanguage   string       `json:"producer_language"`
	}{svc.Name, svc.Version, out, []string{"xlang"}, scope, svc.Requires, ""})
	return string(b), err
}

// TypeDefJSON returns the TypeDef JSON of a WireTyped struct type (v may be a
// value, pointer or reflect.Type).
func TypeDefJSON(v any) (string, error) {
	t := structType(v)
	if t == nil {
		return "", fmt.Errorf("aster: TypeDefJSON needs a struct type, got %T", v)
	}
	r := newTypeResolver()
	if _, err := r.hash(t); err != nil {
		return "", err
	}
	return r.jsons[t], nil
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
	names := []fieldName{}
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
		names = append(names, memberFieldName(sf.Name, f.Name))
	}
	if err := checkFieldNames(names); err != nil {
		return "", fmt.Errorf("%v: %w", t, err)
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
	tag, err := parseAsterTag(sf)
	if err != nil {
		return fieldJSON{}, err
	}
	// Fory Go writes a []byte struct field as list<uint8> unless it is
	// tagged `fory:"type=bytes"` (binary, what every other binding means by
	// bytes). Make the choice explicit so the contract matches the wire.
	if isBytes(t) {
		switch {
		case foryTypeHint(sf) == "bytes":
		case tag.uint8List:
			f := fieldJSON{
				Name: tag.contractName(sf), TypeKind: "primitive", TypePrimitive: "uint8",
				Optional: optional, Container: "list", ContainerKeyKind: "primitive", Required: true,
			}
			return f, tag.applyDefault(&f)
		default:
			return fieldJSON{}, fmt.Errorf("[]byte field needs `fory:\"type=bytes\"` (binary, as in the other bindings) " +
				"or `aster:\"uint8_list\"` with no fory type hint (list<uint8>)")
		}
	}
	container := "none"
	value := t
	var key reflect.Type
	if !isBytes(t) && scalarPrimitive(t, tag.scalar) == "" {
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
	usedDecimal, usedID := false, false
	resolve := func(lt reflect.Type, pos string) (leafJSON, error) {
		l, err := r.leaf(lt, enc, tag.scalar)
		if err != nil {
			return l, err
		}
		switch {
		case strings.HasPrefix(l.prim, "decimal"):
			usedDecimal = true
		case l.prim == "uuid" || l.prim == "ulid":
			usedID = true
			if err := checkIDHint(sf, container, pos); err != nil {
				return l, err
			}
		}
		return l, nil
	}
	pos := "value"
	if container == "list" || container == "set" {
		pos = "element"
	}
	l, err := resolve(value, pos)
	if err != nil {
		return fieldJSON{}, err
	}
	f := fieldJSON{
		Name: tag.contractName(sf), TypeKind: l.kind, TypePrimitive: l.prim, TypeRef: l.ref,
		SelfRefName: l.selfRef, Optional: optional, Container: container,
		ContainerKeyKind: "primitive", Required: true,
	}
	if key != nil {
		k, err := resolve(key, "key")
		if err != nil {
			return fieldJSON{}, err
		}
		if k.kind == "self_ref" {
			return fieldJSON{}, fmt.Errorf("a recursive map key (%v) is not expressible: container_key has no self_ref_name", key)
		}
		f.ContainerKeyKind, f.ContainerKeyPrimitive, f.ContainerKeyRef = k.kind, k.prim, k.ref
	}
	if tag.scalar.decimal != nil && !usedDecimal {
		return fieldJSON{}, fmt.Errorf("`aster:\"%s\"` needs an aster.Decimal field (or a list / map of them)", tag.scalar.decimal.primitive())
	}
	if tag.scalar.id16 != "" && !usedID {
		return fieldJSON{}, fmt.Errorf("`aster:\"%s\"` needs a [16]byte field (or a list / map of them)", tag.scalar.id16)
	}
	return f, tag.applyDefault(&f)
}

// checkIDHint checks a uuid / ulid at pos ("value", "element" or "key") has
// the `fory:"type=..."` hint that makes Fory Go write it as BINARY.
func checkIDHint(sf reflect.StructField, container, pos string) error {
	hint := strings.ReplaceAll(foryTypeHintFull(sf), " ", "")
	want, example := "", ""
	switch {
	case container == "none":
		want, example = "bytes", `fory:"type=bytes"`
		if strings.HasPrefix(hint, want) {
			return nil
		}
	case pos == "element":
		want, example = "element=bytes", `fory:"type=list(element=bytes(nullable=false))"`
		if container == "set" {
			example = `fory:"type=set(element=bytes(nullable=false))"`
		}
	case pos == "key":
		want, example = "key=bytes", `fory:"type=map(key=bytes(nullable=false),value=_(nullable=false))"`
	default:
		want, example = "value=bytes", `fory:"type=map(key=_(nullable=false),value=bytes(nullable=false))"`
	}
	if container != "none" && strings.Contains(hint, want) {
		return nil
	}
	return fmt.Errorf("a uuid / ulid %s needs a Fory BINARY hint, e.g. `%s` (untagged, Fory Go writes a [16]byte as an array)", pos, example)
}

// foryTypeHintFull returns the whole `type=...` value of the fory tag,
// nested parentheses included.
func foryTypeHintFull(sf reflect.StructField) string {
	for _, part := range splitTagOptions(sf.Tag.Get("fory")) {
		k, v, ok := strings.Cut(strings.TrimSpace(part), "=")
		if ok && strings.TrimSpace(k) == "type" {
			return strings.ToLower(strings.TrimSpace(v))
		}
	}
	return ""
}

func isBytes(t reflect.Type) bool {
	return t.Kind() == reflect.Slice && t.Elem().Kind() == reflect.Uint8
}

type leafJSON struct{ kind, prim, ref, selfRef string }

func (r *typeResolver) leaf(t reflect.Type, enc string, sc scalarTag) (leafJSON, error) {
	if t.Kind() == reflect.Pointer {
		t = t.Elem()
	}
	if (t == uuidType || t == ulidType) && sc.id16 != "" && scalarPrimitive(t, scalarTag{}) != sc.id16 {
		return leafJSON{}, fmt.Errorf("`aster:\"%s\"` on an %v", sc.id16, t)
	}
	if p := scalarPrimitive(t, sc); p != "" {
		return leafJSON{kind: "primitive", prim: p}, nil
	}
	if p := primitiveName(t, enc); p != "" {
		return leafJSON{kind: "primitive", prim: p}, nil
	}
	if t.Kind() == reflect.Interface {
		return leafJSON{kind: "any"}, nil
	}
	if t.Kind() == reflect.Struct {
		if _, ok := wireTypeOf(t); ok {
			fqn, _ := fqnOf(t)
			if r.selfRefs[fqn] {
				return leafJSON{kind: "self_ref", selfRef: fqn}, nil
			}
			h, err := r.hash(t)
			return leafJSON{kind: "ref", ref: h}, err
		}
	}
	return leafJSON{}, fmt.Errorf("type %v has no contract mapping (implement aster.WireTyped or use a supported primitive)", t)
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
