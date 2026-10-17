//go:build cgo

package aster

import (
	"context"
	"encoding/hex"
	"encoding/json"
	"strings"
	"testing"
)

// Settings is the type of conformance/vectors/contract-defaults.json
// (produced by Rust #[derive(AsterType)] with #[aster(default = ...)]).
// The Rust-defined field names are snake_case, so each Go field names its
// contract name with `aster:"name=..."` (Aster-FieldNaming.md).
type defaultsSettings struct {
	Name    string            `aster:"name=name"`
	State   string            `aster:"name=state,default=idle"`
	Note    string            `aster:"name=note,default"`
	Enabled bool              `aster:"name=enabled,default=true"`
	Retries int32             `aster:"name=retries,default=-3"`
	SinceMs int64             `aster:"name=since_ms,default=1700000000000"`
	Ratio   float64           `aster:"name=ratio,default=0.5"`
	Label   *string           `aster:"name=label,default=x"`
	Tags    []string          `aster:"name=tags,default"`
	Attrs   map[string]string `aster:"name=attrs,default"`
	Blob    []byte            `fory:"type=bytes" aster:"name=blob"`
}

func (defaultsSettings) AsterWireType() string { return "defaults.v1/Settings" }

func defaultsService() *Service {
	return NewService("Defaults", 1,
		Unary("put", func(_ context.Context, r *defaultsSettings) (*defaultsSettings, error) { return r, nil }),
	)
}

type contractDefaultsVector struct {
	Vector struct {
		CanonicalTypeDefHex string `json:"expected_canonical_type_def_hex"`
		ContractID          string `json:"expected_contract_id"`
		TypeHash            string `json:"expected_type_hash"`
		TypeDef             struct {
			Fields []struct {
				Name            string `json:"name"`
				Container       int    `json:"container"`
				DefaultValueHex string `json:"default_value_hex"`
				Optional        bool   `json:"optional"`
				Required        bool   `json:"required"`
				TypePrimitive   string `json:"type_primitive"`
			} `json:"fields_in_declaration_order"`
		} `json:"type_def"`
	} `json:"vector"`
}

func TestVectors_ContractDefaults(t *testing.T) {
	var v contractDefaultsVector
	loadVectors(t, "contract-defaults.json", &v)

	// Field-level lowering: every field's required / default_value_hex.
	js, err := TypeDefJSON(defaultsSettings{})
	if err != nil {
		t.Fatal(err)
	}
	var td struct {
		Fields []fieldJSON `json:"fields"`
	}
	if err := json.Unmarshal([]byte(js), &td); err != nil {
		t.Fatal(err)
	}
	got := map[string]fieldJSON{}
	for _, f := range td.Fields {
		got[f.Name] = f
	}
	if len(got) != len(v.Vector.TypeDef.Fields) {
		t.Fatalf("%d fields, vector has %d", len(got), len(v.Vector.TypeDef.Fields))
	}
	for _, want := range v.Vector.TypeDef.Fields {
		f, ok := got[want.Name]
		if !ok {
			t.Fatalf("field %q missing", want.Name)
		}
		if f.Required != want.Required || f.DefaultValue != want.DefaultValueHex || f.Optional != want.Optional {
			t.Errorf("%s: required=%v default=%q optional=%v, want required=%v default=%q optional=%v",
				want.Name, f.Required, f.DefaultValue, f.Optional, want.Required, want.DefaultValueHex, want.Optional)
		}
	}

	// Canonical TypeDef bytes, type hash and contract id match Rust's.
	canon, err := ComputeCanonicalBytes("TypeDef", js)
	if err != nil {
		t.Fatal(err)
	}
	if h := hex.EncodeToString(canon); h != v.Vector.CanonicalTypeDefHex {
		t.Errorf("canonical TypeDef\n got %s\nwant %s", h, v.Vector.CanonicalTypeDefHex)
	}
	if h, err := TypeHash(defaultsSettings{}); err != nil || h != v.Vector.TypeHash {
		t.Errorf("type hash %s (%v), want %s", h, err, v.Vector.TypeHash)
	}
	id, err := defaultsService().ContractID()
	if err != nil {
		t.Fatal(err)
	}
	if id != v.Vector.ContractID {
		t.Errorf("contract id %s, want %s", id, v.Vector.ContractID)
	}
}

type defaultsPlain struct {
	Name  string
	State string
}

func (defaultsPlain) AsterWireType() string { return "defaults.v1/Plain" }

type defaultsMarked struct {
	Name  string
	State string `aster:"default=idle"`
}

func (defaultsMarked) AsterWireType() string { return "defaults.v1/Plain" }

// The marker is what changes the contract id: the same struct without it
// (every field required) has a different id from the Rust one.
func TestContractDefaults_MarkerChangesID(t *testing.T) {
	h1, err := TypeHash(defaultsPlain{})
	if err != nil {
		t.Fatal(err)
	}
	h2, err := TypeHash(defaultsMarked{})
	if err != nil {
		t.Fatal(err)
	}
	if h1 == h2 {
		t.Fatal("marking a field with a default must change the type hash")
	}
	// Dropping every marker from the vector type gives a different hash.
	hu, err := TypeHash(unmarkedAdapter{})
	if err != nil {
		t.Fatal(err)
	}
	if hs, _ := TypeHash(defaultsSettings{}); hu == hs {
		t.Fatal("unmarked and marked Settings share a hash")
	}
}

type unmarkedAdapter struct {
	Name    string
	State   string
	Note    string
	Enabled bool
	Retries int32
	SinceMs int64
	Ratio   float64
	Label   *string
	Tags    []string
	Attrs   map[string]string
	Blob    []byte `fory:"type=bytes"`
}

func (unmarkedAdapter) AsterWireType() string { return "defaults.v1/Settings" }

func fieldOf(t *testing.T, v any, name string) (fieldJSON, error) {
	t.Helper()
	typ := structType(v)
	sf, ok := typ.FieldByName(name)
	if !ok {
		t.Fatalf("no field %s", name)
	}
	return newTypeResolver().field(sf)
}

type defaultCases struct {
	Zero      string              `aster:"default"`
	ZeroBool  bool                `aster:"default"`
	ZeroInt   int32               `aster:"default"`
	ZeroFloat float64             `aster:"default"`
	NegInt    int64               `aster:"default=-3"`
	Quoted    string              `aster:"default=\"a,b\""`
	Combined  []byte              `aster:"uint8_list,default"`
	Set       map[string]struct{} `aster:"default"`
	NilPtr    *string             `aster:"default"`
	Fixed     int32               `fory:"encoding=fixed" aster:"default=1"`
	Small     int8                `aster:"default=1"`
	Ratio32   float32             `aster:"default=1"`
	Bin       []byte              `fory:"type=bytes" aster:"default"`
	Msg       defaultsPlain       `aster:"default"`
	FullList  []string            `aster:"default=a"`
	BadInt    int32               `aster:"default=abc"`
	Overflow  int32               `aster:"default=4294967296"`
	NaN       float64             `aster:"default=NaN"`
	Subnormal float64             `aster:"default=1e-310"`
	BadBool   bool                `aster:"default=maybe"`
	BadOpt    bool                `aster:"defualt"`
}

func TestContractDefaults_Lowering(t *testing.T) {
	ok := map[string]string{
		"Zero": "02", "ZeroBool": "00", "ZeroInt": "00", "ZeroFloat": "0000000000000000",
		"NegInt": "05", "Quoted": "0e612c62", "Combined": "00", "Set": "00",
	}
	for name, want := range ok {
		f, err := fieldOf(t, defaultCases{}, name)
		if err != nil {
			t.Errorf("%s: %v", name, err)
			continue
		}
		if f.Required || f.DefaultValue != want {
			t.Errorf("%s: required=%v default=%q, want default %q", name, f.Required, f.DefaultValue, want)
		}
	}
	for _, name := range []string{"NilPtr", "Fixed", "Small", "Ratio32", "Bin", "Msg", "FullList", "BadInt", "Overflow", "NaN", "Subnormal", "BadBool", "BadOpt"} {
		if f, err := fieldOf(t, defaultCases{}, name); err == nil {
			t.Errorf("%s: expected an error, got %+v", name, f)
		} else if testing.Verbose() {
			t.Logf("%s: %v", name, err)
		}
	}
	// -0.0 normalises to +0.0.
	type negz struct {
		F float64 `aster:"default=-0"`
	}
	f, err := fieldOf(t, negz{}, "F")
	if err != nil || f.DefaultValue != "0000000000000000" {
		t.Errorf("-0 default: %+v %v", f, err)
	}
	if _, err := fieldOf(t, defaultCases{}, "BadOpt"); err == nil || !strings.Contains(err.Error(), "unknown aster tag option") {
		t.Errorf("unknown option: %v", err)
	}
}
