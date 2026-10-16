package aster

import (
	"encoding/hex"
	"math/big"
	"strings"
	"testing"
	"time"
)

// The string forms and the decimal(p,s) rule, mirroring
// aster/src/rpc/scalars.rs's unit tests.

func TestScalars_UUIDStringForms(t *testing.T) {
	const s = "0190f2a8-1b2c-7d3e-8f40-123456789abc"
	u, err := ParseUUID(s)
	if err != nil {
		t.Fatal(err)
	}
	if hex.EncodeToString(u[:]) != "0190f2a81b2c7d3e8f40123456789abc" || u.String() != s {
		t.Fatalf("%x %s", u[:], u)
	}
	if v, err := ParseUUID("0190F2A81B2C7D3E8F40123456789ABC"); err != nil || v != u {
		t.Fatalf("32 hex: %v %v", v, err)
	}
	for _, bad := range []string{"", "0190f2a8-1b2c-7d3e-8f40-123456789ab", "0190f2a8x1b2c-7d3e-8f40-123456789abc", strings.Repeat("g", 32)} {
		if _, err := ParseUUID(bad); err == nil {
			t.Errorf("accepted %q", bad)
		}
	}
	var back UUID
	if text, _ := u.MarshalText(); back.UnmarshalText(text) != nil || back != u {
		t.Fatal("text round trip")
	}
}

func TestScalars_ULIDCrockford(t *testing.T) {
	const s = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
	u, err := ParseULID(s)
	if err != nil {
		t.Fatal(err)
	}
	if hex.EncodeToString(u[:]) != "01563e3ab5d3d6764c61efb99302bd5b" || u.String() != s {
		t.Fatalf("%x %s", u[:], u)
	}
	if v, err := ParseULID(strings.ToLower(s)); err != nil || v != u {
		t.Fatal("case-insensitive input")
	}
	if u.Time() != time.UnixMilli(0x01563e3ab5d3).UTC() {
		t.Fatalf("time %v", u.Time())
	}
	if (ULID{}).String() != strings.Repeat("0", 26) {
		t.Fatal("zero")
	}
	var max ULID
	for i := range max {
		max[i] = 0xff
	}
	if max.String() != "7ZZZZZZZZZZZZZZZZZZZZZZZZZ" {
		t.Fatalf("max %s", max)
	}
	for _, bad := range []string{"01ARZ3NDEKTSV4RRFFQ69G5FA", "81ARZ3NDEKTSV4RRFFQ69G5FAV", "01ARZ3NDEKTSV4RRFFQ69G5FAU"} {
		if _, err := ParseULID(bad); err == nil {
			t.Errorf("accepted %q", bad)
		}
	}
}

func TestScalars_DecimalRescaleRule(t *testing.T) {
	d := func(s string) Decimal {
		v, err := ParseDecimal(s)
		if err != nil {
			t.Fatal(err)
		}
		return v
	}
	money := func(v Decimal) (string, error) {
		r, err := RescaleDecimal(v, 18, 4)
		return FormatDecimal(r), err
	}
	for in, want := range map[string]string{
		"1.5": "1.5000", "1.500000": "1.5000", "99999999999999.9999": "99999999999999.9999",
		"-0.0001": "-0.0001", "0": "0.0000",
	} {
		if got, err := money(d(in)); err != nil || got != want {
			t.Errorf("%s: %s %v, want %s", in, got, err, want)
		}
	}
	if got, err := money(NewDecimal(big.NewInt(15), -2)); err != nil || got != "1500.0000" {
		t.Errorf("negative scale: %s %v", got, err)
	}
	if _, err := money(d("1.23456")); err == nil || !strings.Contains(err.Error(), "rounding") {
		t.Errorf("rounding: %v", err)
	}
	if _, err := money(d("123456789012345")); err == nil || !strings.Contains(err.Error(), "more than 18") {
		t.Errorf("digits: %v", err)
	}
	if r, err := RescaleDecimal(d("0.05"), 1, 2); err != nil || r.Scale != 2 || r.Unscaled.Int64() != 5 {
		t.Errorf("0.05: %v %v", r, err)
	}
	if _, err := RescaleDecimal(d("0.10"), 1, 2); err == nil {
		t.Error("0.10 into decimal(1,2)")
	}
	for _, bad := range []string{"1e5", ".", "", "-", "1.2.3", "+-1"} {
		if _, err := ParseDecimal(bad); err == nil {
			t.Errorf("accepted %q", bad)
		}
	}
	if FormatDecimal(d("-12.3400")) != "-12.3400" || FormatDecimal(d("+.5")) != "0.5" {
		t.Error("format")
	}
	if _, err := parseDecimalSpec("decimal(0,0)"); err == nil {
		t.Error("decimal(0,0)")
	}
	if s, err := parseDecimalSpec("decimal( 18 , 4 )"); err != nil || s.primitive() != "decimal(18,4)" {
		t.Errorf("spec %v %v", s, err)
	}
}

func TestScalars_SplitTagOptionsKeepsParentheses(t *testing.T) {
	got := splitTagOptions(`decimal(18,4),default="a,b",uint8_list`)
	if len(got) != 3 || got[0] != "decimal(18,4)" || got[1] != `default="a,b"` {
		t.Fatalf("%q", got)
	}
}

type normInner struct {
	At  time.Time
	Fee Decimal `aster:"decimal(10,2)"`
}

type normOuter struct {
	Inner  normInner
	Ptr    *normInner
	List   []normInner
	ByName map[string]time.Time
	Prices []Decimal `aster:"decimal(10,2)"`
	Plain  Decimal
}

func TestScalars_NormalizeCopiesOnWrite(t *testing.T) {
	at := time.Date(2024, 1, 2, 3, 4, 5, 6007008, time.Local)
	fee, _ := ParseDecimal("1.5")
	in := normOuter{
		Inner: normInner{At: at, Fee: fee}, Ptr: &normInner{At: at, Fee: fee},
		List: []normInner{{At: at, Fee: fee}}, ByName: map[string]time.Time{"a": at},
		Prices: []Decimal{fee}, Plain: fee,
	}
	got, err := normalizeEncode(&in)
	if err != nil {
		t.Fatal(err)
	}
	out := got.(*normOuter)
	want := at.Truncate(time.Microsecond)
	if !out.Inner.At.Equal(want) || !out.Ptr.At.Equal(want) || !out.List[0].At.Equal(want) || !out.ByName["a"].Equal(want) ||
		FormatDecimal(out.Inner.Fee) != "1.50" || FormatDecimal(out.Prices[0]) != "1.50" || FormatDecimal(out.Plain) != "1.5" {
		t.Fatalf("%+v", out)
	}
	if in.Inner.At != at || in.Ptr.At != at || in.List[0].At != at || in.ByName["a"] != at || FormatDecimal(in.Prices[0]) != "1.5" {
		t.Fatal("the caller's value changed")
	}
	// Nothing to change: the same value comes back.
	clean := &normInner{At: want.UTC(), Fee: out.Inner.Fee}
	if v, err := normalizeEncode(clean); err != nil || v.(*normInner) != clean {
		t.Fatalf("%v %v", v, err)
	}
	// Decode moves times to UTC and refuses an over-precise decimal.
	if err := normalizeDecode(out); err != nil || out.Inner.At.Location() != time.UTC || out.ByName["a"].Location() != time.UTC {
		t.Fatalf("decode: %v %v", err, out.Inner.At.Location())
	}
	bad, _ := ParseDecimal("1.234")
	out.List[0].Fee = bad
	if err := normalizeDecode(out); err == nil || !strings.Contains(err.Error(), "List") {
		t.Fatalf("decode refuses: %v", err)
	}
}
