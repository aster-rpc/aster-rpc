//go:build cgo

package aster

import (
	"encoding/binary"
	"fmt"
	"math"
	"reflect"
	"strconv"
	"strings"
)

// Contract defaults (Aster-ContractIdentity §11.3.2.3, "Default Values in
// contract_id"). A field the producer declares as having a default is
// FieldDef.required = false with default_value = the canonical encoding of
// the default; every other field is required. Defaults never change the Fory
// payload bytes, only type_hash / contract_id, so every binding must mark the
// same fields to compute the same ids.
//
// Go struct fields carry no declared default value, so the marker is an
// `aster` struct tag option:
//
//	type Settings struct {
//	    Name    string                      // required (no marker)
//	    State   string   `aster:"default=idle"`
//	    Retries int32    `aster:"default=-3"`
//	    Enabled bool     `aster:"default=true"`
//	    Ratio   float64  `aster:"default=0.5"`
//	    Label   *string  `aster:"default=x"` // optional field with a default
//	    Tags    []string `aster:"default"`   // lists, sets and maps: empty
//	    Note    string   `aster:"default"`   // bare marker: the Go zero value
//	}
//
// `default=VALUE` declares VALUE (a string value may be double-quoted Go
// syntax, e.g. `default="a,b"`, when it holds a comma); a bare `default` means
// the Go zero value ("" / false / 0 / 0.0) for scalars and the empty
// container for slices, arrays and maps. A pointer field has no zero default
// in the contract (None is not expressible in v1), so it needs an explicit
// value. Options are comma separated and combine with the others
// (`aster:"uint8_list,default"`). Only the types the spec table defines
// carry a default: string, bool, varint32 (int32), varint64 (int64 / int),
// float64 and the empty containers; any other combination (fixed or small
// integers, float32, []byte binary, nested messages) is an error when the
// contract is built, like Python's decoration-time TypeError.

// asterTag is a parsed `aster:"..."` struct tag.
type asterTag struct {
	name       string // `name=...`: the contract name, when not the Go field name
	uint8List  bool
	hasDefault bool
	hasValue   bool   // `default=VALUE` rather than a bare `default`
	defValue   string // raw text of VALUE
	scalar     scalarTag
}

func parseAsterTag(sf reflect.StructField) (asterTag, error) {
	var t asterTag
	raw, ok := sf.Tag.Lookup("aster")
	if !ok {
		return t, nil
	}
	for _, opt := range splitTagOptions(raw) {
		opt = strings.TrimSpace(opt)
		k, v, hasEq := strings.Cut(opt, "=")
		switch strings.TrimSpace(k) {
		case "", "uuid", "ulid":
		case "uint8_list":
			t.uint8List = true
		case "name":
			if !hasEq || strings.TrimSpace(v) == "" {
				return t, fmt.Errorf("aster tag option %q needs a name (name=written_by)", opt)
			}
			t.name = strings.TrimSpace(v)
		case "default":
			t.hasDefault = true
			if hasEq {
				t.hasValue, t.defValue = true, strings.TrimSpace(v)
			}
		default:
			if strings.HasPrefix(opt, "decimal(") {
				continue
			}
			return t, fmt.Errorf("unknown aster tag option %q", opt)
		}
	}
	sc, err := parseScalarTag(sf)
	t.scalar = sc
	return t, err
}

// contractName is the field's contract name: the `name=` override, else the
// Go field name verbatim (Aster-FieldNaming.md).
func (t asterTag) contractName(sf reflect.StructField) string {
	if t.name != "" {
		return t.name
	}
	return sf.Name
}

// applyDefault marks f as having a contract default when the tag declares one.
func (t asterTag) applyDefault(f *fieldJSON) error {
	if !t.hasDefault {
		return nil
	}
	v, err := t.lower(f)
	if err != nil {
		return fmt.Errorf("aster contract default: %w", err)
	}
	f.Required = false
	f.DefaultValue = v
	return nil
}

// lower returns the hex of the canonical default_value bytes (§11.3.2.3).
func (t asterTag) lower(f *fieldJSON) (string, error) {
	if f.Container != "none" {
		if t.hasValue {
			return "", fmt.Errorf("a list/set/map default must be the empty container: use a bare `default` (§11.3.2.3 rule 2)")
		}
		return "00", nil
	}
	if f.Optional && !t.hasValue {
		return "", fmt.Errorf("a nil default is not expressible in v1; give the pointer field an explicit `default=VALUE` or drop the marker to keep it required (§11.3.2.3)")
	}
	if f.TypeKind != "primitive" {
		return "", fmt.Errorf("only scalar fields may carry a contract default (§11.3.2.3 rule 3)")
	}
	text := t.defValue
	switch f.TypePrimitive {
	case "string":
		if strings.HasPrefix(text, `"`) {
			u, err := strconv.Unquote(text)
			if err != nil {
				return "", fmt.Errorf("string default %s: %w", text, err)
			}
			text = u
		}
		return hexOf(canonicalString(text)), nil
	case "bool":
		if !t.hasValue {
			return "00", nil
		}
		b, err := strconv.ParseBool(text)
		if err != nil {
			return "", fmt.Errorf("bool default %q: %w", text, err)
		}
		if b {
			return "01", nil
		}
		return "00", nil
	case "varint32", "varint64":
		bits := 32
		if f.TypePrimitive == "varint64" {
			bits = 64
		}
		n := int64(0)
		if t.hasValue {
			var err error
			if n, err = strconv.ParseInt(text, 10, bits); err != nil {
				return "", fmt.Errorf("%s default %q: %w", f.TypePrimitive, text, err)
			}
		}
		return hexOf(binary.AppendUvarint(nil, uint64(n<<1)^uint64(n>>63))), nil
	case "float64":
		x := 0.0
		if t.hasValue {
			var err error
			if x, err = strconv.ParseFloat(text, 64); err != nil {
				return "", fmt.Errorf("float64 default %q: %w", text, err)
			}
		}
		if math.IsNaN(x) || math.IsInf(x, 0) {
			return "", fmt.Errorf("float default must be finite")
		}
		if x != 0 && math.Abs(x) < 2.2250738585072014e-308 {
			return "", fmt.Errorf("float default must not be subnormal")
		}
		if x == 0 {
			x = 0 // normalise -0.0
		}
		return hexOf(binary.LittleEndian.AppendUint64(nil, math.Float64bits(x))), nil
	}
	return "", fmt.Errorf("contract defaults for wire type %q are reserved in v1 (§11.3.2.3); supported: string, bool, int32, int64/int, float64, empty containers", f.TypePrimitive)
}

func canonicalString(s string) []byte {
	return append(binary.AppendUvarint(nil, uint64(len(s))<<2|2), s...)
}

func hexOf(b []byte) string {
	const digits = "0123456789abcdef"
	out := make([]byte, 0, len(b)*2)
	for _, c := range b {
		out = append(out, digits[c>>4], digits[c&15])
	}
	return string(out)
}
