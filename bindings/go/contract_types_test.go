//go:build cgo

package aster

import (
	"context"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"math/big"
	"reflect"
	"sort"
	"strings"
	"testing"
	"time"
)

// Go mirrors of aster/tests/it/contract_types.rs, checked against
// conformance/vectors/contract-types.json (the Rust golden vectors). The
// contract is defined in Rust, so each field names its Rust (snake_case)
// name with `aster:"name=..."` (Aster-FieldNaming.md). The
// `nullable=false` hints make Go's Fory type metadata byte-identical to
// Rust's (see scalars.go).

type ctTemporal struct {
	At      time.Time   `aster:"name=at"`
	Day     Date        `aster:"name=day"`
	Amount  Decimal     `aster:"name=amount"`
	MaybeAt *time.Time  `aster:"name=maybe_at"`
	History []time.Time `fory:"type=list(element=_(nullable=false))" aster:"name=history"`
}

func (ctTemporal) AsterWireType() string { return "types.v1/Temporal" }

type ctTreeNode struct {
	Value    string       `aster:"name=value"`
	Children []ctTreeNode `fory:"type=list(element=_(nullable=false))" aster:"name=children"`
}

func (ctTreeNode) AsterWireType() string { return "types.v1/TreeNode" }

type ctChain struct {
	Value int32    `aster:"name=value"`
	Next  *ctChain `aster:"name=next"`
}

func (ctChain) AsterWireType() string { return "types.v1/Chain" }

type ctAuthor struct {
	Name  string   `aster:"name=name"`
	Books []ctBook `fory:"type=list(element=_(nullable=false))" aster:"name=books"`
}

func (ctAuthor) AsterWireType() string { return "types.v1/Author" }

type ctBook struct {
	Title     string    `aster:"name=title"`
	WrittenBy *ctAuthor `aster:"name=written_by"`
}

func (ctBook) AsterWireType() string { return "types.v1/Book" }

type ctAlpha struct {
	Next *ctBeta `aster:"name=next"`
}

func (ctAlpha) AsterWireType() string { return "types.v1/Alpha" }

type ctBeta struct {
	Next *ctGamma `aster:"name=next"`
}

func (ctBeta) AsterWireType() string { return "types.v1/Beta" }

type ctGamma struct {
	Next []ctAlpha `fory:"type=list(element=_(nullable=false))" aster:"name=next"`
}

func (ctGamma) AsterWireType() string { return "types.v1/Gamma" }

type ctForest struct {
	Trees []ctTreeNode `fory:"type=list(element=_(nullable=false))" aster:"name=trees"`
}

func (ctForest) AsterWireType() string { return "types.v1/Forest" }

type ctIdentified struct {
	Id      UUID            `fory:"type=bytes" aster:"name=id"` // Fory Go writes "ID" as "i_d"
	Trace   ULID            `fory:"type=bytes" aster:"name=trace"`
	Parent  *UUID           `fory:"type=bytes" aster:"name=parent"`
	History []ULID          `fory:"type=list(element=bytes(nullable=false))" aster:"name=history"`
	Names   map[UUID]string `fory:"type=map(key=bytes(nullable=false),value=_(nullable=false))" aster:"name=names"`
}

func (ctIdentified) AsterWireType() string { return "types.v1/Identified" }

type ctPriced struct {
	Price      Decimal  `aster:"decimal(18,4),name=price"`
	Rate       Decimal  `aster:"decimal(5,0),name=rate"`
	MaybePrice *Decimal `aster:"decimal(18,4),name=maybe_price"`
	Amount     Decimal  `aster:"name=amount"`
}

func (ctPriced) AsterWireType() string { return "types.v1/Priced" }

// A cycle whose spanning tree depends on codepoint (not UTF-16) order:
// Ａ (U+FF21) and 𝔸 (U+1D538). Contract identity only, as in Rust.
type ctOrd struct {
	Wide *ctWide `aster:"name=wide"`
	Math *ctMath `aster:"name=math"`
}

func (ctOrd) AsterWireType() string { return "types.v1/Ord" }

type ctWide struct {
	Back *ctOrd  `aster:"name=back"`
	Math *ctMath `aster:"name=math"`
}

func (ctWide) AsterWireType() string { return "types.v1/\uff21" }

type ctMath struct {
	Wide *ctWide `aster:"name=wide"`
}

func (ctMath) AsterWireType() string { return "types.v1/\U0001d538" }

// The spec's B.4 diamond: A -> C is a back-edge though C is hashed first.
type ctDiamondA struct {
	B *ctDiamondB `aster:"name=b"`
	C *ctDiamondC `aster:"name=c"`
}

func (ctDiamondA) AsterWireType() string { return "types.v1/DiamondA" }

type ctDiamondB struct {
	C *ctDiamondC `aster:"name=c"`
}

func (ctDiamondB) AsterWireType() string { return "types.v1/DiamondB" }

type ctDiamondC struct {
	A []ctDiamondA `fory:"type=list(element=_(nullable=false))" aster:"name=a"`
}

func (ctDiamondC) AsterWireType() string { return "types.v1/DiamondC" }

var ctTypes = map[string]any{
	"types.v1.Temporal": ctTemporal{}, "types.v1.TreeNode": ctTreeNode{}, "types.v1.Chain": ctChain{},
	"types.v1.Author": ctAuthor{}, "types.v1.Book": ctBook{}, "types.v1.Alpha": ctAlpha{},
	"types.v1.Beta": ctBeta{}, "types.v1.Gamma": ctGamma{}, "types.v1.Forest": ctForest{},
	"types.v1.Identified": ctIdentified{}, "types.v1.Priced": ctPriced{},
	"types.v1.DiamondA": ctDiamondA{}, "types.v1.DiamondB": ctDiamondB{}, "types.v1.DiamondC": ctDiamondC{},
	"types.v1.Ord": ctOrd{}, "types.v1.\uff21": ctWide{}, "types.v1.\U0001d538": ctMath{},
}

type ctVectors struct {
	Types map[string]struct {
		CanonicalHex string `json:"expected_canonical_type_def_hex"`
		TypeHash     string `json:"expected_type_hash"`
		Fields       []struct {
			Container     int    `json:"container"`
			Name          string `json:"name"`
			Optional      bool   `json:"optional"`
			SelfRefName   string `json:"self_ref_name"`
			TypeKind      int    `json:"type_kind"`
			TypePrimitive string `json:"type_primitive"`
			TypeRef       string `json:"type_ref"`
		} `json:"fields_in_declaration_order"`
	} `json:"types"`
	Payloads map[string]struct {
		Hex   string `json:"hex"`
		Type  string `json:"type"`
		Value any    `json:"value"`
	} `json:"payloads"`
	DecodeCases map[string]struct {
		Hex     string `json:"hex"`
		Type    string `json:"type"`
		Refused bool   `json:"refused"`
		Value   any    `json:"value"`
	} `json:"decode_cases"`
	ContractID    string `json:"expected_contract_id"`
	IdsContractID string `json:"expected_ids_contract_id"`
}

func loadContractTypes(t *testing.T) ctVectors {
	t.Helper()
	var f struct {
		Vector ctVectors `json:"vector"`
	}
	loadVectors(t, "contract-types.json", &f)
	if len(f.Vector.Types) != len(ctTypes) || len(f.Vector.Payloads) == 0 || len(f.Vector.DecodeCases) == 0 {
		t.Fatalf("vectors: %d types (want %d), %d payloads, %d decode cases",
			len(f.Vector.Types), len(ctTypes), len(f.Vector.Payloads), len(f.Vector.DecodeCases))
	}
	return f.Vector
}

func TestContractTypes_TypeHashesMatchRust(t *testing.T) {
	v := loadContractTypes(t)
	kinds := map[string]int{"primitive": 0, "ref": 1, "self_ref": 2, "any": 3}
	containers := map[string]int{"none": 0, "list": 1, "set": 2, "map": 3}
	for fqn, exp := range v.Types {
		t.Run(fqn, func(t *testing.T) {
			goType, ok := ctTypes[fqn]
			if !ok {
				t.Fatalf("no Go mirror for %s", fqn)
			}
			js, err := TypeDefJSON(goType)
			if err != nil {
				t.Fatal(err)
			}
			var td struct {
				Fields []fieldJSON `json:"fields"`
			}
			if err := json.Unmarshal([]byte(js), &td); err != nil {
				t.Fatal(err)
			}
			byName := map[string]fieldJSON{}
			for _, f := range td.Fields {
				byName[f.Name] = f
			}
			for _, ef := range exp.Fields {
				f, ok := byName[ef.Name]
				if !ok {
					t.Fatalf("no field %s", ef.Name)
				}
				if kinds[f.TypeKind] != ef.TypeKind || f.TypePrimitive != ef.TypePrimitive || f.TypeRef != ef.TypeRef ||
					f.SelfRefName != ef.SelfRefName || f.Optional != ef.Optional || containers[f.Container] != ef.Container {
					t.Errorf("field %s: got %+v, want %+v", ef.Name, f, ef)
				}
			}
			canon, err := ComputeCanonicalBytes("TypeDef", js)
			if err != nil {
				t.Fatal(err)
			}
			if got := hex.EncodeToString(canon); got != exp.CanonicalHex {
				t.Errorf("canonical bytes\n got %s\nwant %s", got, exp.CanonicalHex)
			}
			h, err := TypeHash(goType)
			if err != nil {
				t.Fatal(err)
			}
			if h != exp.TypeHash {
				t.Errorf("type hash %s, want %s", h, exp.TypeHash)
			}
		})
	}
}

// The hash of a member of a cycle does not depend on which member is asked
// for first, nor on the resolver's history.
func TestContractTypes_CycleHashIsOrderIndependent(t *testing.T) {
	v := loadContractTypes(t)
	for _, order := range [][]any{{ctAlpha{}, ctBeta{}, ctGamma{}}, {ctGamma{}, ctBeta{}, ctAlpha{}}, {ctBook{}, ctAuthor{}}} {
		r := newTypeResolver()
		for _, typ := range order {
			h, err := r.hash(reflect.TypeOf(typ))
			if err != nil {
				t.Fatal(err)
			}
			fqn, _ := fqnOf(reflect.TypeOf(typ))
			if h != v.Types[fqn].TypeHash {
				t.Errorf("%s: %s, want %s", fqn, h, v.Types[fqn].TypeHash)
			}
		}
	}
}

func ctNoop[Req, Resp any](name string) *Method {
	return Unary(name, func(context.Context, *Req) (*Resp, error) { return new(Resp), nil })
}

func TestContractTypes_ContractIDsMatchRust(t *testing.T) {
	v := loadContractTypes(t)
	types := NewService("Types", 1,
		ctNoop[ctTemporal, ctTemporal]("echo_temporal"),
		ctNoop[ctTreeNode, ctForest]("plant"),
		ctNoop[ctBook, ctAuthor]("author"),
		ctNoop[ctAlpha, ctGamma]("cycle"),
	)
	if id, err := types.ContractID(); err != nil || id != v.ContractID {
		t.Errorf("Types contract id %s (%v), want %s", id, err, v.ContractID)
	}
	ids := NewService("Ids", 1, ctNoop[ctIdentified, ctPriced]("price"))
	if id, err := ids.ContractID(); err != nil || id != v.IdsContractID {
		t.Errorf("Ids contract id %s (%v), want %s", id, err, v.IdsContractID)
	}
}

// ctJSON renders a decoded Go value the way the vectors' JSON writes it.
func ctJSON(v reflect.Value) any {
	switch v.Type() {
	case timeType:
		return v.Interface().(time.Time).UTC().Format("2006-01-02T15:04:05.000000Z")
	case dateType:
		d := v.Interface().(Date)
		return fmt.Sprintf("%04d-%02d-%02d", d.Year, int(d.Month), d.Day)
	case decimalType:
		return FormatDecimal(v.Interface().(Decimal))
	case uuidType:
		return v.Interface().(UUID).String()
	case ulidType:
		return v.Interface().(ULID).String()
	}
	switch v.Kind() {
	case reflect.Pointer:
		if v.IsNil() {
			return nil
		}
		return ctJSON(v.Elem())
	case reflect.Slice:
		out := []any{}
		for i := 0; i < v.Len(); i++ {
			out = append(out, ctJSON(v.Index(i)))
		}
		return out
	case reflect.Map:
		out := map[string]any{}
		it := v.MapRange()
		for it.Next() {
			out[ctJSON(it.Key()).(string)] = ctJSON(it.Value())
		}
		return out
	case reflect.Struct:
		out := map[string]any{}
		for i := 0; i < v.NumField(); i++ {
			sf := v.Type().Field(i)
			tag, _ := parseAsterTag(sf)
			out[tag.contractName(sf)] = ctJSON(v.Field(i))
		}
		return out
	case reflect.String:
		return v.String()
	}
	return v.Interface()
}

func ctCodec(t *testing.T) *ForyCodec {
	t.Helper()
	c := NewForyCodec()
	names := make([]string, 0, len(ctTypes))
	for n := range ctTypes {
		names = append(names, n)
	}
	sort.Strings(names)
	for _, n := range names {
		if err := c.RegisterWireTypes(ctTypes[n]); err != nil {
			t.Fatal(err)
		}
	}
	return c
}

func ctDecode(c *ForyCodec, typ string, payload []byte) (reflect.Value, error) {
	dest := reflect.New(reflect.TypeOf(ctTypes[typ]))
	err := c.Decode(payload, dest.Interface())
	return dest, err
}

func TestContractTypes_RustPayloadsDecodeAndReencode(t *testing.T) {
	v := loadContractTypes(t)
	c := ctCodec(t)
	for name, p := range v.Payloads {
		t.Run(name, func(t *testing.T) {
			payload := mustHex(t, p.Hex)
			got, err := ctDecode(c, p.Type, payload)
			if err != nil {
				t.Fatalf("decode: %v", err)
			}
			if g := ctJSON(got); !reflect.DeepEqual(g, p.Value) {
				t.Fatalf("value\n got %#v\nwant %#v", g, p.Value)
			}
			// Go writes Rust's bytes.
			out, err := c.Encode(got.Interface())
			if err != nil {
				t.Fatal(err)
			}
			if hex.EncodeToString(out) != p.Hex {
				t.Fatalf("re-encode\n got %x\nwant %s", out, p.Hex)
			}
		})
	}
}

func TestContractTypes_DecodeCases(t *testing.T) {
	v := loadContractTypes(t)
	c := ctCodec(t)
	for name, dc := range v.DecodeCases {
		t.Run(name, func(t *testing.T) {
			got, err := ctDecode(c, dc.Type, mustHex(t, dc.Hex))
			if dc.Refused {
				if err == nil {
					t.Fatalf("accepted: %#v", ctJSON(got))
				}
				return
			}
			if err != nil {
				t.Fatalf("decode: %v", err)
			}
			if g := ctJSON(got); !reflect.DeepEqual(g, dc.Value) {
				t.Fatalf("value\n got %#v\nwant %#v", g, dc.Value)
			}
		})
	}
}

func mustDecimal(t *testing.T, s string) Decimal {
	t.Helper()
	d, err := ParseDecimal(s)
	if err != nil {
		t.Fatal(err)
	}
	return d
}

// Unpadded decimal(p,s) values encode exactly as Rust's FixedDecimal does,
// and the caller's value is not modified.
func TestContractTypes_EncodeRescalesToDeclaredScale(t *testing.T) {
	v := loadContractTypes(t)
	c := ctCodec(t)
	maybe := mustDecimal(t, "0.0001")
	in := &ctPriced{Price: mustDecimal(t, "-12345.67"), Rate: NewDecimal(big.NewInt(99999), 0), MaybePrice: &maybe, Amount: mustDecimal(t, "1.5")}
	out, err := c.Encode(in)
	if err != nil {
		t.Fatal(err)
	}
	if hex.EncodeToString(out) != v.Payloads["priced"].Hex {
		t.Fatalf("got %x\nwant %s", out, v.Payloads["priced"].Hex)
	}
	if FormatDecimal(in.Price) != "-12345.67" {
		t.Fatalf("caller's value changed: %s", FormatDecimal(in.Price))
	}
	for _, bad := range []ctPriced{
		{Price: mustDecimal(t, "1.23456")},
		{Price: mustDecimal(t, "123456789012345")},
		{Rate: mustDecimal(t, "123456")},
	} {
		if _, err := c.Encode(&bad); err == nil || !strings.Contains(err.Error(), "decimal(") {
			t.Errorf("encode %+v: %v", bad, err)
		}
	}
}

func TestContractTypes_TimestampMicrosecondsUTC(t *testing.T) {
	c := ctCodec(t)
	loc := time.FixedZone("X", 5*3600)
	at := time.Date(2024, 3, 1, 17, 34, 56, 789012345, loc)
	in := ctTemporal{At: at, Day: DateOf(at), History: []time.Time{at}}
	b, err := c.Encode(in)
	if err != nil {
		t.Fatal(err)
	}
	var got ctTemporal
	if err := c.Decode(b, &got); err != nil {
		t.Fatal(err)
	}
	want := time.Date(2024, 3, 1, 12, 34, 56, 789012000, time.UTC)
	if got.At != want || got.History[0] != want || got.At.Location() != time.UTC {
		t.Fatalf("got %v / %v, want %v", got.At, got.History, want)
	}
	if in.At.Nanosecond() != 789012345 || in.History[0].Nanosecond() != 789012345 {
		t.Fatal("caller's value changed")
	}
}

func TestContractTypes_IdentityDistinctions(t *testing.T) {
	type byUUID struct {
		ID UUID `fory:"type=bytes"`
	}
	type byULID struct {
		ID ULID `fory:"type=bytes"`
	}
	type byTaggedUUID struct {
		ID [16]byte `aster:"uuid" fory:"type=bytes"`
	}
	type byBytes struct {
		ID []byte `fory:"type=bytes"`
	}
	type price184 struct {
		Price Decimal `aster:"decimal(18,4)"`
	}
	type price102 struct {
		Price Decimal `aster:"decimal(10,2)"`
	}
	type price struct{ Price Decimal }
	field := func(v any) fieldJSON {
		f, err := newTypeResolver().field(reflect.TypeOf(v).Field(0))
		if err != nil {
			t.Fatal(err)
		}
		return f
	}
	if a, b, c, d := field(byUUID{}), field(byULID{}), field(byTaggedUUID{}), field(byBytes{}); a.TypePrimitive != "uuid" ||
		b.TypePrimitive != "ulid" || c != a || d.TypePrimitive != "binary" || a.Container != "none" {
		t.Fatalf("ids: %+v %+v %+v %+v", a, b, c, d)
	}
	if a, b, c := field(price184{}), field(price102{}), field(price{}); a.TypePrimitive != "decimal(18,4)" ||
		b.TypePrimitive != "decimal(10,2)" || c.TypePrimitive != "decimal" {
		t.Fatalf("decimals: %+v %+v %+v", a, b, c)
	}
	type at struct {
		At time.Time
		D  Date
	}
	if a := field(at{}); a.TypePrimitive != "timestamp" || a.TypeKind != "primitive" {
		t.Fatalf("time.Time: %+v", a)
	}
	if d, err := newTypeResolver().field(reflect.TypeOf(at{}).Field(1)); err != nil || d.TypePrimitive != "date" {
		t.Fatalf("Date: %+v %v", d, err)
	}
}

func TestContractTypes_TagErrors(t *testing.T) {
	type noHint struct{ ID UUID }
	type listNoHint struct {
		IDs []ULID `fory:"type=list(element=_(nullable=false))"`
	}
	type keyNoHint struct{ M map[UUID]string }
	type decimalOnString struct {
		S string `aster:"decimal(18,4)"`
	}
	type badPrecision struct {
		D Decimal `aster:"decimal(4,5)"`
	}
	type uuidOnString struct {
		S string `aster:"uuid"`
	}
	type wrongID struct {
		ID ULID `aster:"uuid" fory:"type=bytes"`
	}
	for _, c := range []struct {
		v    any
		want string
	}{
		{noHint{}, `type=bytes`},
		{listNoHint{}, `element=bytes`},
		{keyNoHint{}, `key=bytes`},
		{decimalOnString{}, `needs an aster.Decimal`},
		{badPrecision{}, `0 <= s <= p`},
		{uuidOnString{}, `needs a [16]byte`},
		{wrongID{}, `aster.ULID`},
	} {
		_, err := newTypeResolver().field(reflect.TypeOf(c.v).Field(0))
		if err == nil || !strings.Contains(err.Error(), c.want) {
			t.Errorf("%T: %v (want %q)", c.v, err, c.want)
		}
	}
}

// ctLooseIdentified is Identified without history and without the
// nullable=false hints, so its Fory metadata differs from Rust's.
type ctLooseIdentified struct {
	Id     UUID            `fory:"type=bytes"`
	Trace  ULID            `fory:"type=bytes"`
	Parent *UUID           `fory:"type=bytes"`
	Names  map[UUID]string `fory:"type=map(key=bytes)"`
}

func (ctLooseIdentified) AsterWireType() string { return "types.v1/Identified" }

// Fory Go 1.7.6 panics building a Go type for map<binary, _> from metadata
// that differs from the local TypeDef; the codec turns that into an error
// and keeps working.
func TestContractTypes_ForyPanicIsAnError(t *testing.T) {
	v := loadContractTypes(t)
	c := NewForyCodec()
	var got ctLooseIdentified
	err := c.Decode(mustHex(t, v.Payloads["identified"].Hex), &got)
	if err == nil {
		t.Skip("Fory decoded differing map<binary, _> metadata: drop the caveat in scalars.go")
	}
	var fp *foryPanic
	if !errors.As(err, &fp) {
		t.Fatalf("want a recovered Fory panic, got %v", err)
	}
	if _, err := c.Encode(&ctLooseIdentified{}); err != nil {
		t.Fatal(err)
	}
}
