package aster

import (
	"encoding/hex"
	"fmt"
	"math/big"
	"reflect"
	"strconv"
	"strings"
	"sync"
	"time"

	forygo "github.com/apache/fory/go/fory"
)

// Canonical contract scalars (Aster-ContractIdentity.md, "Time, UUID, and
// Decimal"), the Go mirror of Rust's aster::rpc::{Timestamp, Date, Decimal,
// FixedDecimal, Uuid, Ulid}:
//
//	Go type                              contract        Fory xlang wire
//	time.Time                            timestamp       TIMESTAMP (38), microsecond UTC
//	aster.Date                           date            DATE (39)
//	aster.Decimal                        decimal         DECIMAL (40)
//	aster.Decimal + `aster:"decimal(p,s)"` decimal(p,s)  DECIMAL with scale exactly s
//	aster.UUID / [16]byte + `aster:"uuid"`  uuid         BINARY (41) of 16 bytes
//	aster.ULID / [16]byte + `aster:"ulid"`  ulid         BINARY (41) of 16 bytes
//
// time.Time is truncated to whole microseconds on encode and decoded in UTC.
// A decimal(p,s) field is rescaled to scale s on encode by zero padding, and
// a value that would need rounding or has more than p digits is refused on
// encode and on decode. The tag on a container field applies to its decimal
// elements / values.
//
// Fory Go writes a 16-byte id as BINARY only from a `fory:"type=bytes"` hint,
// so uuid / ulid fields need one, at the position of the id:
//
//	ID      aster.UUID            `fory:"type=bytes"`
//	Parent  *aster.UUID           `fory:"type=bytes"`
//	History []aster.ULID          `fory:"type=list(element=bytes(nullable=false))"`
//	Names   map[aster.UUID]string `fory:"type=map(key=bytes(nullable=false),value=_(nullable=false))"`
//
// Building the contract checks the hint is there. Fory Go marks list
// elements and map keys / values nullable unless told otherwise; Rust and
// Python do not. `nullable=false` on them makes Go's type metadata
// byte-identical to Rust's, which Fory Go 1.7.6 needs to read a Rust
// list<uuid> or map<uuid, _> (from different metadata it refuses the list
// and fails building the map), and which keeps every other Go payload
// byte-identical to Rust's.

// Date is a calendar date: contract type date, Fory DATE (days since
// 1970-01-01). It is Fory Go's date type.
type Date = forygo.Date

// Decimal is an arbitrary-precision decimal, Unscaled × 10^-Scale: contract
// type decimal (or decimal(p,s) with an `aster:"decimal(p,s)"` tag), Fory
// DECIMAL. It is Fory Go's decimal type; build one with NewDecimal or
// ParseDecimal.
type Decimal = forygo.Decimal

// NewDecimal returns unscaled × 10^-scale (unscaled is copied; nil is 0).
func NewDecimal(unscaled *big.Int, scale int32) Decimal { return forygo.NewDecimal(unscaled, scale) }

// DateOf returns the calendar date of t in t's location.
func DateOf(t time.Time) Date {
	y, m, d := t.Date()
	return Date{Year: y, Month: m, Day: d}
}

// ParseDate parses an ISO 8601 calendar date (2024-03-01).
func ParseDate(s string) (Date, error) {
	t, err := time.Parse(time.DateOnly, s)
	if err != nil {
		return Date{}, fmt.Errorf("aster: date: %w", err)
	}
	return DateOf(t), nil
}

// ── uuid / ulid ─────────────────────────────────────────────────────────────

// UUID is a 16-byte RFC 9562 UUID: contract type uuid. String is the
// lowercase 8-4-4-4-12 form. Tag the field `fory:"type=bytes"` (see above).
type UUID [16]byte

// ULID is a 16-byte ULID (48-bit big-endian millisecond timestamp, then 80
// random bits): contract type ulid, distinct from uuid. String is 26
// characters of Crockford base32. Tag the field `fory:"type=bytes"`.
type ULID [16]byte

func (u UUID) String() string {
	h := hex.EncodeToString(u[:])
	return h[0:8] + "-" + h[8:12] + "-" + h[12:16] + "-" + h[16:20] + "-" + h[20:32]
}

// MarshalText returns the 8-4-4-4-12 form.
func (u UUID) MarshalText() ([]byte, error) { return []byte(u.String()), nil }

// UnmarshalText parses the 8-4-4-4-12 or 32-hex form.
func (u *UUID) UnmarshalText(b []byte) error {
	v, err := ParseUUID(string(b))
	if err == nil {
		*u = v
	}
	return err
}

// ParseUUID parses the hyphenated 8-4-4-4-12 form or 32 hex digits (either
// case).
func ParseUUID(s string) (UUID, error) {
	var u UUID
	digits := s
	switch len(s) {
	case 36:
		for _, i := range []int{8, 13, 18, 23} {
			if s[i] != '-' {
				return u, fmt.Errorf("aster: uuid: not a UUID string: %q", s)
			}
		}
		digits = strings.ReplaceAll(s, "-", "")
	case 32:
	default:
		return u, fmt.Errorf("aster: uuid: not a UUID string: %q", s)
	}
	if len(digits) != 32 {
		return u, fmt.Errorf("aster: uuid: not a UUID string: %q", s)
	}
	if _, err := hex.Decode(u[:], []byte(digits)); err != nil {
		return UUID{}, fmt.Errorf("aster: uuid: not a UUID string: %q", s)
	}
	return u, nil
}

const crockford = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"

func (u ULID) String() string {
	var out [26]byte
	// 128 bits as 26 five-bit groups, the first carrying the top 3 bits.
	hi, lo := beUint64(u[0:8]), beUint64(u[8:16])
	for i := 0; i < 26; i++ {
		shift := uint(125 - 5*i)
		var v uint64
		switch {
		case shift >= 64:
			v = hi >> (shift - 64)
		case shift+5 > 64:
			v = hi<<(64-shift) | lo>>shift
		default:
			v = lo >> shift
		}
		out[i] = crockford[v&31]
	}
	return string(out[:])
}

// Time returns the ULID's millisecond timestamp, in UTC.
func (u ULID) Time() time.Time {
	ms := int64(beUint64(u[0:8]) >> 16)
	return time.UnixMilli(ms).UTC()
}

// MarshalText returns the Crockford base32 form.
func (u ULID) MarshalText() ([]byte, error) { return []byte(u.String()), nil }

// UnmarshalText parses the Crockford base32 form.
func (u *ULID) UnmarshalText(b []byte) error {
	v, err := ParseULID(string(b))
	if err == nil {
		*u = v
	}
	return err
}

// ParseULID parses 26 characters of Crockford base32 (case-insensitive; the
// first character is 0-7).
func ParseULID(s string) (ULID, error) {
	var u ULID
	if len(s) != 26 {
		return u, fmt.Errorf("aster: ulid: not a 26-character Crockford base32 ULID: %q", s)
	}
	var hi, lo uint64
	for i := 0; i < 26; i++ {
		v := strings.IndexByte(crockford, upperASCII(s[i]))
		if v < 0 || (i == 0 && v > 7) {
			return ULID{}, fmt.Errorf("aster: ulid: not a 26-character Crockford base32 ULID: %q", s)
		}
		hi = hi<<5 | lo>>59
		lo = lo<<5 | uint64(v)
	}
	putBEUint64(u[0:8], hi)
	putBEUint64(u[8:16], lo)
	return u, nil
}

func upperASCII(c byte) byte {
	if c >= 'a' && c <= 'z' {
		return c - 'a' + 'A'
	}
	return c
}

func beUint64(b []byte) uint64 {
	var n uint64
	for _, c := range b[:8] {
		n = n<<8 | uint64(c)
	}
	return n
}

func putBEUint64(b []byte, n uint64) {
	for i := 7; i >= 0; i-- {
		b[i] = byte(n)
		n >>= 8
	}
}

// ── decimal / decimal(p,s) ──────────────────────────────────────────────────

// MaxDecimalPrecision is the largest precision a decimal(p,s) may declare.
const MaxDecimalPrecision = 1000

// ParseDecimal parses plain decimal notation ("-12.3400", "1500", "+.5");
// the scale is the number of digits after the point. No exponent.
func ParseDecimal(s string) (Decimal, error) {
	body := s
	neg := false
	if body != "" && (body[0] == '-' || body[0] == '+') {
		neg, body = body[0] == '-', body[1:]
	}
	intPart, frac, _ := strings.Cut(body, ".")
	digits := intPart + frac
	if digits == "" || strings.Trim(digits, "0123456789") != "" {
		return Decimal{}, fmt.Errorf("aster: decimal: not a decimal string: %q", s)
	}
	var n big.Int
	n.SetString(digits, 10)
	if neg {
		n.Neg(&n)
	}
	return NewDecimal(&n, int32(len(frac))), nil
}

// FormatDecimal returns d in plain notation: "-12.3400", "1500" (a negative
// scale pads with zeros).
func FormatDecimal(d Decimal) string {
	sign := ""
	if d.Unscaled.Sign() < 0 {
		sign = "-"
	}
	var mag big.Int
	digits := mag.Abs(&d.Unscaled).String()
	if d.Scale <= 0 {
		return sign + digits + strings.Repeat("0", int(-d.Scale))
	}
	scale := int(d.Scale)
	if len(digits) < scale+1 {
		digits = strings.Repeat("0", scale+1-len(digits)) + digits
	}
	return sign + digits[:len(digits)-scale] + "." + digits[len(digits)-scale:]
}

func pow10(n int64) *big.Int {
	return new(big.Int).Exp(big.NewInt(10), big.NewInt(n), nil)
}

// RescaleDecimal rescales d to exactly scale digits after the point and
// checks it has at most precision digits: the decimal(p,s) rule applied on
// encode and on decode. Zero padding is exact; a value whose dropped digits
// are not all zero (it would need rounding) or whose rescaled unscaled value
// has more than precision digits is refused.
func RescaleDecimal(d Decimal, precision, scale int32) (Decimal, error) {
	if precision < 1 || precision > MaxDecimalPrecision || scale < 0 {
		return Decimal{}, fmt.Errorf("decimal(%d,%d): need 1 <= p <= %d and s >= 0", precision, scale, MaxDecimalPrecision)
	}
	var u big.Int
	have, target := int64(d.Scale), int64(scale)
	if have > target {
		var rem big.Int
		u.QuoRem(&d.Unscaled, pow10(have-target), &rem)
		if rem.Sign() != 0 {
			return Decimal{}, fmt.Errorf("decimal(%d,%d): %s needs rounding to %d decimal places",
				precision, scale, FormatDecimal(d), scale)
		}
	} else {
		u.Mul(&d.Unscaled, pow10(target-have))
	}
	var mag big.Int
	if mag.Abs(&u).Cmp(pow10(int64(precision))) >= 0 {
		return Decimal{}, fmt.Errorf("decimal(%d,%d): %s has more than %d digits",
			precision, scale, FormatDecimal(d), precision)
	}
	return NewDecimal(&u, scale), nil
}

func checkDecimalSpec(p, s int32) error {
	if p < 1 || p > MaxDecimalPrecision || s < 0 || s > p {
		return fmt.Errorf("decimal(%d,%d): need 1 <= p <= %d and 0 <= s <= p", p, s, MaxDecimalPrecision)
	}
	return nil
}

// decimalSpec is a decimal(p,s) declaration.
type decimalSpec struct{ p, s int32 }

func (d decimalSpec) primitive() string { return fmt.Sprintf("decimal(%d,%d)", d.p, d.s) }

// parseDecimalSpec parses "decimal(p,s)" (spaces allowed around the numbers).
func parseDecimalSpec(opt string) (*decimalSpec, error) {
	inner, ok := strings.CutPrefix(opt, "decimal(")
	if inner, ok = strings.CutSuffix(inner, ")"); !ok {
		return nil, fmt.Errorf("malformed %q: want decimal(p,s)", opt)
	}
	ps, ss, ok := strings.Cut(inner, ",")
	if !ok {
		return nil, fmt.Errorf("malformed %q: want decimal(p,s)", opt)
	}
	p, err1 := strconv.ParseInt(strings.TrimSpace(ps), 10, 32)
	s, err2 := strconv.ParseInt(strings.TrimSpace(ss), 10, 32)
	if err1 != nil || err2 != nil {
		return nil, fmt.Errorf("malformed %q: want decimal(p,s)", opt)
	}
	if err := checkDecimalSpec(int32(p), int32(s)); err != nil {
		return nil, err
	}
	return &decimalSpec{int32(p), int32(s)}, nil
}

// splitTagOptions splits on commas outside double quotes and parentheses.
func splitTagOptions(tag string) []string {
	var out []string
	start, inQuote, depth := 0, false, 0
	for i := 0; i < len(tag); i++ {
		switch tag[i] {
		case '\\':
			if inQuote {
				i++
			}
		case '"':
			inQuote = !inQuote
		case '(':
			if !inQuote {
				depth++
			}
		case ')':
			if !inQuote && depth > 0 {
				depth--
			}
		case ',':
			if !inQuote && depth == 0 {
				out = append(out, tag[start:i])
				start = i + 1
			}
		}
	}
	return append(out, tag[start:])
}

// scalarTag is the scalar part of an `aster:"..."` tag: decimal(p,s), uuid
// or ulid. Other options are left to the contract builder.
type scalarTag struct {
	decimal *decimalSpec
	id16    string // "uuid" / "ulid" for a [16]byte field
}

func parseScalarTag(sf reflect.StructField) (scalarTag, error) {
	var t scalarTag
	raw, ok := sf.Tag.Lookup("aster")
	if !ok {
		return t, nil
	}
	for _, opt := range splitTagOptions(raw) {
		opt = strings.TrimSpace(opt)
		switch {
		case opt == "uuid" || opt == "ulid":
			if t.id16 != "" {
				return t, fmt.Errorf("aster tag: both uuid and ulid")
			}
			t.id16 = opt
		case strings.HasPrefix(opt, "decimal("):
			d, err := parseDecimalSpec(opt)
			if err != nil {
				return t, fmt.Errorf("aster tag: %w", err)
			}
			t.decimal = d
		}
	}
	return t, nil
}

var (
	timeType    = reflect.TypeFor[time.Time]()
	dateType    = reflect.TypeFor[Date]()
	decimalType = reflect.TypeFor[Decimal]()
	uuidType    = reflect.TypeFor[UUID]()
	ulidType    = reflect.TypeFor[ULID]()
)

// is16Bytes reports whether t is a [16]byte array (UUID, ULID or plain).
func is16Bytes(t reflect.Type) bool {
	return t.Kind() == reflect.Array && t.Len() == 16 && t.Elem().Kind() == reflect.Uint8
}

// scalarPrimitive is the contract primitive of a canonical scalar leaf, or "".
func scalarPrimitive(t reflect.Type, tag scalarTag) string {
	switch t {
	case timeType:
		return "timestamp"
	case dateType:
		return "date"
	case decimalType:
		if tag.decimal != nil {
			return tag.decimal.primitive()
		}
		return "decimal"
	case uuidType:
		return "uuid"
	case ulidType:
		return "ulid"
	}
	if is16Bytes(t) && tag.id16 != "" {
		return tag.id16
	}
	return ""
}

// ── Encode / decode normalisation ───────────────────────────────────────────
//
// Fory Go writes a time.Time's nanoseconds, decodes it in time.Local, and
// knows nothing of decimal(p,s). The codec walks values whose type holds a
// time.Time or a decimal(p,s) field: on encode it truncates times to whole
// microseconds and rescales decimal(p,s) values (copying what it changes, so
// the caller's value is untouched); on decode it moves times to UTC and
// rescales, refusing what does not fit.

const maxNormalizeDepth = 1000

var normNeeds sync.Map // reflect.Type -> bool

// needsNormalize reports whether values of t hold a time.Time or a
// decimal(p,s) field anywhere.
func needsNormalize(t reflect.Type) bool {
	if v, ok := normNeeds.Load(t); ok {
		return v.(bool)
	}
	n := scanNormalize(t, map[reflect.Type]bool{})
	normNeeds.Store(t, n)
	return n
}

func scanNormalize(t reflect.Type, visiting map[reflect.Type]bool) bool {
	switch t {
	case timeType:
		return true
	case dateType, decimalType, uuidType, ulidType:
		return false
	}
	switch t.Kind() {
	case reflect.Pointer, reflect.Slice, reflect.Array:
		return scanNormalize(t.Elem(), visiting)
	case reflect.Map:
		return scanNormalize(t.Key(), visiting) || scanNormalize(t.Elem(), visiting)
	case reflect.Struct:
		if visiting[t] {
			return false
		}
		visiting[t] = true
		for i := 0; i < t.NumField(); i++ {
			sf := t.Field(i)
			if !sf.IsExported() {
				continue
			}
			if tag, err := parseScalarTag(sf); err != nil || tag.decimal != nil {
				return true // an error surfaces when the walk parses the tag
			}
			if scanNormalize(sf.Type, visiting) {
				return true
			}
		}
	}
	return false
}

type normField struct {
	index int
	name  string
	fixed *decimalSpec
}

var normPlans sync.Map // reflect.Type -> []normField or error

// structPlan lists the fields of struct type t the walk visits.
func structPlan(t reflect.Type) ([]normField, error) {
	if v, ok := normPlans.Load(t); ok {
		if err, isErr := v.(error); isErr {
			return nil, err
		}
		return v.([]normField), nil
	}
	var plan []normField
	var planErr error
	for i := 0; i < t.NumField(); i++ {
		sf := t.Field(i)
		if !sf.IsExported() {
			continue
		}
		tag, err := parseScalarTag(sf)
		if err != nil {
			planErr = fmt.Errorf("aster: %v.%s: %w", t, sf.Name, err)
			break
		}
		if tag.decimal != nil || needsNormalize(sf.Type) {
			plan = append(plan, normField{index: i, name: sf.Name, fixed: tag.decimal})
		}
	}
	if planErr != nil {
		normPlans.Store(t, planErr)
		return nil, planErr
	}
	normPlans.Store(t, plan)
	return plan, nil
}

type normalizer struct{ encode bool }

// walk returns v normalised and whether it changed (a changed composite is a
// copy; v itself is never modified). fixed is the decimal(p,s) declared by
// the enclosing field.
func (n normalizer) walk(v reflect.Value, fixed *decimalSpec, depth int) (reflect.Value, bool, error) {
	if depth > maxNormalizeDepth {
		return v, false, fmt.Errorf("aster: value nested deeper than %d levels", maxNormalizeDepth)
	}
	t := v.Type()
	switch t {
	case timeType:
		tm := v.Interface().(time.Time)
		if n.encode {
			tr := tm.Truncate(time.Microsecond)
			if tr.Equal(tm) {
				return v, false, nil
			}
			return reflect.ValueOf(tr), true, nil
		}
		if tm.Location() == time.UTC {
			return v, false, nil
		}
		return reflect.ValueOf(tm.UTC()), true, nil
	case decimalType:
		if fixed == nil {
			return v, false, nil
		}
		d := v.Interface().(Decimal)
		r, err := RescaleDecimal(d, fixed.p, fixed.s)
		if err != nil {
			return v, false, err
		}
		if d.Scale == fixed.s {
			return v, false, nil
		}
		return reflect.ValueOf(r), true, nil
	case dateType, uuidType, ulidType:
		return v, false, nil
	}
	switch t.Kind() {
	case reflect.Pointer:
		if v.IsNil() {
			return v, false, nil
		}
		e, changed, err := n.walk(v.Elem(), fixed, depth+1)
		if err != nil || !changed {
			return v, false, err
		}
		p := reflect.New(t.Elem())
		p.Elem().Set(e)
		return p, true, nil
	case reflect.Slice, reflect.Array:
		if t.Elem().Kind() == reflect.Uint8 || (t.Kind() == reflect.Slice && v.IsNil()) {
			return v, false, nil
		}
		if fixed == nil && !needsNormalize(t.Elem()) {
			return v, false, nil
		}
		var out reflect.Value
		for i := 0; i < v.Len(); i++ {
			e, changed, err := n.walk(v.Index(i), fixed, depth+1)
			if err != nil {
				return v, false, err
			}
			if !changed {
				continue
			}
			if !out.IsValid() {
				out = copySeq(v)
			}
			out.Index(i).Set(e)
		}
		if !out.IsValid() {
			return v, false, nil
		}
		return out, true, nil
	case reflect.Map:
		if v.IsNil() || (fixed == nil && !needsNormalize(t.Elem())) {
			return v, false, nil
		}
		type entry struct{ k, v reflect.Value }
		var changedEntries []entry
		it := v.MapRange()
		for it.Next() {
			e, changed, err := n.walk(it.Value(), fixed, depth+1)
			if err != nil {
				return v, false, err
			}
			if changed {
				changedEntries = append(changedEntries, entry{it.Key(), e})
			}
		}
		if len(changedEntries) == 0 {
			return v, false, nil
		}
		out := reflect.MakeMapWithSize(t, v.Len())
		it = v.MapRange()
		for it.Next() {
			out.SetMapIndex(it.Key(), it.Value())
		}
		for _, e := range changedEntries {
			out.SetMapIndex(e.k, e.v)
		}
		return out, true, nil
	case reflect.Struct:
		plan, err := structPlan(t)
		if err != nil {
			return v, false, err
		}
		var out reflect.Value
		for _, f := range plan {
			e, changed, err := n.walk(v.Field(f.index), f.fixed, depth+1)
			if err != nil {
				return v, false, fmt.Errorf("%s: %w", f.name, err)
			}
			if !changed {
				continue
			}
			if !out.IsValid() {
				out = reflect.New(t).Elem()
				out.Set(v)
			}
			out.Field(f.index).Set(e)
		}
		if !out.IsValid() {
			return v, false, nil
		}
		return out, true, nil
	}
	return v, false, nil
}

func copySeq(v reflect.Value) reflect.Value {
	if v.Kind() == reflect.Array {
		out := reflect.New(v.Type()).Elem()
		out.Set(v)
		return out
	}
	out := reflect.MakeSlice(v.Type(), v.Len(), v.Len())
	reflect.Copy(out, v)
	return out
}

// normalizeEncode returns value with its times truncated to microseconds and
// its decimal(p,s) fields rescaled, or an error for a decimal(p,s) value
// that does not fit. value itself is not modified.
func normalizeEncode(value any) (any, error) {
	rv := reflect.ValueOf(value)
	if !rv.IsValid() || !needsNormalize(rv.Type()) {
		return value, nil
	}
	out, changed, err := normalizer{encode: true}.walk(rv, nil, 0)
	if err != nil {
		return nil, fmt.Errorf("aster: encode: %w", err)
	}
	if !changed {
		return value, nil
	}
	return out.Interface(), nil
}

// normalizeDecode moves the times in *dest to UTC and checks / rescales its
// decimal(p,s) fields. dest is a non-nil pointer.
func normalizeDecode(dest any) error {
	rv := reflect.ValueOf(dest)
	if rv.Kind() != reflect.Pointer || rv.IsNil() || !needsNormalize(rv.Type()) {
		return nil
	}
	out, changed, err := normalizer{}.walk(rv.Elem(), nil, 0)
	if err != nil {
		return fmt.Errorf("aster: decode: %w", err)
	}
	if changed {
		rv.Elem().Set(out)
	}
	return nil
}
