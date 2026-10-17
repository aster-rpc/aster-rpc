//go:build cgo

package aster

import (
	"encoding/hex"
	"encoding/json"
	"reflect"
	"strings"
	"testing"

	forygo "github.com/apache/fory/go/fory"
)

// conformance/vectors/contract-types.json `field_names` (Aster-FieldNaming.md):
// Go reproduces the Orders contract as Rust defines it (order_id) and as Java
// defines it (orderId), both through `aster:"name=..."`, exchanges the shared
// payload, pins Fory Go's snake_case conversion, and refuses what the spec
// refuses.

type fnRsOrder struct {
	OrderId    string         `aster:"name=order_id"`
	LineItems  []fnRsLineItem `fory:"type=list(element=_(nullable=false))" aster:"name=line_items"`
	TotalCents int32          `aster:"name=total_cents"`
	Note       *string        `aster:"name=note"`
}

func (fnRsOrder) AsterWireType() string { return "names.v1/Order" }

type fnRsLineItem struct {
	SkuCode   string `aster:"name=sku_code"`
	UnitCount int32  `aster:"name=unit_count"`
}

func (fnRsLineItem) AsterWireType() string { return "names.v1/LineItem" }

type fnJvOrder struct {
	OrderId    string         `aster:"name=orderId"`
	LineItems  []fnJvLineItem `fory:"type=list(element=_(nullable=false))" aster:"name=lineItems"`
	TotalCents int32          `aster:"name=totalCents"`
	Note       *string        `aster:"name=note"`
}

func (fnJvOrder) AsterWireType() string { return "names.v1/Order" }

type fnJvLineItem struct {
	SkuCode   string `aster:"name=skuCode"`
	UnitCount int32  `aster:"name=unitCount"`
}

func (fnJvLineItem) AsterWireType() string { return "names.v1/LineItem" }

type fnCase struct {
	Error  string `json:"error"`
	Fields []struct {
		MemberSnake string `json:"member_snake"`
		Name        string `json:"name"`
	} `json:"fields"`
}

type fnDefined struct {
	Types map[string]struct {
		CanonicalHex string `json:"expected_canonical_type_def_hex"`
		TypeHash     string `json:"expected_type_hash"`
	} `json:"types"`
	ContractID string `json:"expected_contract_id"`
	Payload    struct {
		Type  string `json:"type"`
		Hex   string `json:"hex"`
		Value any    `json:"value"`
	} `json:"payload"`
}

type fnVectors struct {
	SnakeCase map[string]map[string]any `json:"snake_case"`
	Refused   []fnCase                  `json:"refused"`
	Accepted  []fnCase                  `json:"accepted"`
	Rust      fnDefined                 `json:"rust_defined"`
	Java      fnDefined                 `json:"java_defined"`
}

func loadFieldNames(t *testing.T) fnVectors {
	t.Helper()
	var f struct {
		Vector struct {
			FieldNames fnVectors `json:"field_names"`
		} `json:"vector"`
	}
	loadVectors(t, "contract-types.json", &f)
	if len(f.Vector.FieldNames.SnakeCase) == 0 || len(f.Vector.FieldNames.Refused) == 0 {
		t.Fatal("field_names vectors missing")
	}
	return f.Vector.FieldNames
}

func TestFieldNames_ForyGoSnakeCaseAndPlainness(t *testing.T) {
	v := loadFieldNames(t)
	for name, row := range v.SnakeCase {
		if got := forygo.SnakeCase(name); got != row["go"] {
			t.Errorf("Fory Go SnakeCase(%q) = %q, vector %q", name, got, row["go"])
		}
		if got := referenceSnakeCase(name); got != row["rust"] {
			t.Errorf("referenceSnakeCase(%q) = %q, vector %q", name, got, row["rust"])
		}
		if plain := notPlain(name) == ""; plain != row["plain"] {
			t.Errorf("plain(%q) = %v, vector %v", name, plain, row["plain"])
		}
		if row["plain"] == false && notPlain(plainSuggestion(name)) != "" {
			t.Errorf("suggestion for %q (%q) is not plain", name, plainSuggestion(name))
		}
	}
}

func fnPairs(c fnCase) []fieldName {
	out := []fieldName{}
	for _, f := range c.Fields {
		out = append(out, fieldName{memberSnake: f.MemberSnake, name: f.Name})
	}
	return out
}

func TestFieldNames_RefusedAndAccepted(t *testing.T) {
	v := loadFieldNames(t)
	for _, c := range v.Refused {
		err := checkFieldNames(fnPairs(c))
		if err == nil || !strings.Contains(err.Error(), c.Error) {
			t.Errorf("%+v: %v", c, err)
		}
	}
	for _, c := range v.Accepted {
		if err := checkFieldNames(fnPairs(c)); err != nil {
			t.Errorf("%+v: %v", c, err)
		}
	}
}

// End to end: a Go type is refused when its contract is built.
type fnAcronym struct{ ID string }

func (fnAcronym) AsterWireType() string { return "names.v1/Bad" }

type fnMismatch struct {
	Author string `aster:"name=writtenBy"`
}

func (fnMismatch) AsterWireType() string { return "names.v1/Bad" }

type fnCollide struct {
	UserId  string
	User_id string
}

func (fnCollide) AsterWireType() string { return "names.v1/Bad" }

func TestFieldNames_GoTypesAreRefused(t *testing.T) {
	for typ, want := range map[any]string{
		fnAcronym{}:  "not a plain field name",
		fnMismatch{}: "does not match",
		fnCollide{}:  "collide",
	} {
		_, err := TypeHash(typ)
		if err == nil || !strings.Contains(err.Error(), want) {
			t.Errorf("%T: %v, want %q", typ, err, want)
		}
	}
	// Without an override a Go contract's names are the Go names, verbatim.
	js, err := TypeDefJSON(fnCollideFree{})
	if err != nil || !strings.Contains(js, `"name":"WrittenBy"`) {
		t.Errorf("%s %v", js, err)
	}
}

type fnCollideFree struct{ WrittenBy string }

func (fnCollideFree) AsterWireType() string { return "names.v1/Free" }

func TestFieldNames_RustAndJavaDefinedContracts(t *testing.T) {
	v := loadFieldNames(t)
	for _, tc := range []struct {
		name        string
		d           fnDefined
		order, item any
		svc         *Service
	}{
		{"rust_defined", v.Rust, fnRsOrder{}, fnRsLineItem{}, NewService("Orders", 1, ctNoop[fnRsOrder, fnRsOrder]("place"))},
		{"java_defined", v.Java, fnJvOrder{}, fnJvLineItem{}, NewService("Orders", 1, ctNoop[fnJvOrder, fnJvOrder]("place"))},
	} {
		t.Run(tc.name, func(t *testing.T) {
			for fqn, typ := range map[string]any{"names.v1.Order": tc.order, "names.v1.LineItem": tc.item} {
				js, err := TypeDefJSON(typ)
				if err != nil {
					t.Fatal(err)
				}
				canon, err := ComputeCanonicalBytes("TypeDef", js)
				if err != nil {
					t.Fatal(err)
				}
				if got := hex.EncodeToString(canon); got != tc.d.Types[fqn].CanonicalHex {
					t.Errorf("%s canonical bytes\n got %s\nwant %s", fqn, got, tc.d.Types[fqn].CanonicalHex)
				}
				if h, err := TypeHash(typ); err != nil || h != tc.d.Types[fqn].TypeHash {
					t.Errorf("%s: %s (%v), want %s", fqn, h, err, tc.d.Types[fqn].TypeHash)
				}
			}
			if id, err := tc.svc.ContractID(); err != nil || id != tc.d.ContractID {
				t.Errorf("contract id %s (%v), want %s", id, err, tc.d.ContractID)
			}
			c := NewForyCodec()
			if err := c.RegisterWireTypes(tc.item); err != nil {
				t.Fatal(err)
			}
			if err := c.RegisterWireTypes(tc.order); err != nil {
				t.Fatal(err)
			}
			dest := reflect.New(reflect.TypeOf(tc.order))
			if err := c.Decode(mustHex(t, tc.d.Payload.Hex), dest.Interface()); err != nil {
				t.Fatalf("decode: %v", err)
			}
			// The decoded value, keyed by contract names, is the vector's.
			b, _ := json.Marshal(ctJSON(dest))
			var got any
			_ = json.Unmarshal(b, &got)
			if !reflect.DeepEqual(got, tc.d.Payload.Value) {
				t.Fatalf("value\n got %s\nwant %#v", b, tc.d.Payload.Value)
			}
			out, err := c.Encode(dest.Interface())
			if err != nil {
				t.Fatal(err)
			}
			if hex.EncodeToString(out) != tc.d.Payload.Hex {
				t.Fatalf("re-encode\n got %x\nwant %s", out, tc.d.Payload.Hex)
			}
		})
	}
}
