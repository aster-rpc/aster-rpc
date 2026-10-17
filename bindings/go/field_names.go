package aster

import (
	"fmt"
	"strings"
	"unicode"

	forygo "github.com/apache/fory/go/fory"
)

// Field names (ffi_spec/Aster-FieldNaming.md). A field's contract name is the
// Go field name, verbatim, or the `aster:"name=..."` override when the
// contract is defined in another language:
//
//	type Book struct {
//		Title     string    `aster:"name=title"`
//		WrittenBy *Author   `aster:"name=written_by"`
//	}
//
// Fory Go matches fields by its snake_case form of the Go field name
// (forygo.SnakeCase), so contract definition refuses a name that is not
// plain, a field whose Fory form differs from the reference snake_case form
// of its name (`ID` is `i_d` to Fory Go: name it `Id`), and two fields with
// the same snake_case form.

// fieldName is one field's (Fory Go snake_case form of the member, contract name).
type fieldName struct {
	memberSnake string
	name        string
}

// referenceSnakeCase is the snake_case form of a field name as Rust fory-core
// and Fory JavaScript write it, the form every binding compares against.
func referenceSnakeCase(name string) string {
	rs := []rune(name)
	var b strings.Builder
	for i, c := range rs {
		if c >= 'A' && c <= 'Z' {
			if i > 0 {
				prevUpper := rs[i-1] >= 'A' && rs[i-1] <= 'Z'
				nextUpperOrEnd := i+1 >= len(rs) || (rs[i+1] >= 'A' && rs[i+1] <= 'Z')
				if !prevUpper || !nextUpperOrEnd {
					b.WriteByte('_')
				}
			}
			b.WriteRune(c + 'a' - 'A')
		} else {
			b.WriteRune(c)
		}
	}
	return b.String()
}

func isASCIIUpper(c rune) bool { return c >= 'A' && c <= 'Z' }

// notPlain says why name is not plain, or "" when it is: every ASCII capital
// starts the name or follows an ASCII lowercase letter or a digit, no
// non-ASCII letter is uppercase, and the name does not end in `_`.
func notPlain(name string) string {
	rs := []rune(name)
	for i, c := range rs {
		if isASCIIUpper(c) && i > 0 && !((rs[i-1] >= 'a' && rs[i-1] <= 'z') || (rs[i-1] >= '0' && rs[i-1] <= '9')) {
			return "an uppercase letter follows another uppercase letter, `_` or a non-ASCII character"
		}
		if c > unicode.MaxASCII && unicode.IsUpper(c) {
			return "it has an uppercase letter outside ASCII"
		}
	}
	if strings.HasSuffix(name, "_") {
		return "it ends in `_`"
	}
	return ""
}

// plainSuggestion is a plain spelling of name: acronyms as words (`userID` ->
// `userId`, `HTTPServer` -> `HttpServer`), a capital after `_` or a non-ASCII
// character lowered, non-ASCII capitals lowered, a trailing `_` dropped.
func plainSuggestion(name string) string {
	rs := []rune(name)
	for i := 0; i < len(rs); {
		if !isASCIIUpper(rs[i]) {
			if rs[i] > unicode.MaxASCII && unicode.IsUpper(rs[i]) {
				rs[i] = unicode.ToLower(rs[i])
			}
			i++
			continue
		}
		end := i
		for end < len(rs) && isASCIIUpper(rs[end]) {
			end++
		}
		nextLower := end < len(rs) && rs[end] >= 'a' && rs[end] <= 'z'
		keepLast := nextLower && end-i >= 3
		firstOK := i == 0 || (rs[i-1] >= 'a' && rs[i-1] <= 'z') || (rs[i-1] >= '0' && rs[i-1] <= '9')
		for k := i; k < end; k++ {
			if !((k == i && firstOK) || (k == end-1 && keepLast)) {
				rs[k] = unicode.ToLower(rs[k])
			}
		}
		i = end
	}
	return strings.TrimRight(string(rs), "_")
}

// checkFieldNames refuses a type's fields per Aster-FieldNaming.md §4 and §5.
func checkFieldNames(fields []fieldName) error {
	for _, f := range fields {
		if why := notPlain(f.name); why != "" {
			return fmt.Errorf("%q is not a plain field name: %s, so the Fory implementations disagree "+
				"on its snake_case form. Use a plain spelling such as %q (ffi_spec/Aster-FieldNaming.md §5)",
				f.name, why, plainSuggestion(f.name))
		}
		if s := referenceSnakeCase(f.name); s != f.memberSnake {
			return fmt.Errorf("the field name %q does not match its member: Fory writes the member as %q "+
				"but %q is %q. Name the member so its snake_case form is %q, with `aster:\"name=%s\"` "+
				"(ffi_spec/Aster-FieldNaming.md §4)", f.name, f.memberSnake, f.name, s, s, f.name)
		}
	}
	for i, b := range fields {
		for _, a := range fields[:i] {
			if referenceSnakeCase(a.name) == referenceSnakeCase(b.name) {
				return fmt.Errorf("the fields %q and %q collide: Fory writes both as %q and could not "+
					"tell them apart (ffi_spec/Aster-FieldNaming.md §4)", a.name, b.name, referenceSnakeCase(b.name))
			}
		}
	}
	return nil
}

// memberFieldName is a struct field's (Fory Go form, contract name).
func memberFieldName(memberName, override string) fieldName {
	name := memberName
	if override != "" {
		name = override
	}
	return fieldName{memberSnake: forygo.SnakeCase(memberName), name: name}
}
