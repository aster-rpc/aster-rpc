//go:build cgo

package aster

import (
	"fmt"
	"reflect"
	"slices"
)

// Type-graph resolution for contract identity, including recursive types
// (Aster-ContractIdentity §11.3.4 Step 3, the Rust and Python algorithm):
// Tarjan's strongly connected components, leaves first; inside a component
// a DFS from the codepoint-smallest member, successors in codepoint order,
// makes every edge to an already visited member a SELF_REF (type_kind 2,
// self_ref_name = the target's package.name) and every other edge an
// ordinary REF to the target's hash; members are hashed in that DFS's
// post-order, so each REF's target is hashed first.

type typeResolver struct {
	hashes map[reflect.Type]string
	jsons  map[reflect.Type]string // each resolved type's TypeDef JSON
	// fqns maps each resolved type's package.name to its Go type, so two Go
	// types under one wire name are refused.
	fqns map[string]reflect.Type
	// selfRefs holds the SELF_REF targets (package.name) of the TypeDef
	// being built.
	selfRefs map[string]bool
}

func newTypeResolver() *typeResolver {
	return &typeResolver{hashes: map[reflect.Type]string{}, jsons: map[reflect.Type]string{}, fqns: map[string]reflect.Type{}}
}

// fqnOf is the package.name of a WireTyped struct type.
func fqnOf(t reflect.Type) (string, error) {
	tag, ok := wireTypeOf(t)
	if !ok {
		return "", fmt.Errorf("%v does not implement AsterWireType() (aster.WireTyped)", t)
	}
	pkg, name := splitWireType(tag)
	if pkg == "" {
		return name, nil
	}
	return pkg + "." + name, nil
}

// hash returns the type hash of t, resolving the unresolved part of t's type
// graph first.
func (r *typeResolver) hash(t reflect.Type) (string, error) {
	for t.Kind() == reflect.Pointer {
		t = t.Elem()
	}
	if h, ok := r.hashes[t]; ok {
		return h, nil
	}
	if err := r.resolveGraph(t); err != nil {
		return "", err
	}
	return r.hashes[t], nil
}

// structEdges lists the WireTyped struct types t's fields reference through
// pointers, lists, sets and maps, as the field builder resolves them.
func structEdges(t reflect.Type) []reflect.Type {
	var out []reflect.Type
	add := func(x reflect.Type) {
		if x.Kind() == reflect.Pointer {
			x = x.Elem()
		}
		if x.Kind() == reflect.Struct && scalarPrimitive(x, scalarTag{}) == "" {
			if _, ok := wireTypeOf(x); ok {
				out = append(out, x)
			}
		}
	}
	for i := 0; i < t.NumField(); i++ {
		sf := t.Field(i)
		if !sf.IsExported() || foryIgnored(sf) {
			continue
		}
		ft := sf.Type
		if ft.Kind() == reflect.Pointer {
			ft = ft.Elem()
		}
		if isBytes(ft) || is16Bytes(ft) {
			continue
		}
		switch ft.Kind() {
		case reflect.Slice, reflect.Array:
			add(ft.Elem())
		case reflect.Map:
			add(ft.Key())
			add(ft.Elem())
		default:
			add(ft)
		}
	}
	return out
}

func (r *typeResolver) resolveGraph(root reflect.Type) error {
	// The unresolved part of the graph reachable from root, by package.name.
	nodes := map[string]reflect.Type{}
	edges := map[string][]string{}
	var collect func(t reflect.Type) error
	collect = func(t reflect.Type) error {
		fqn, err := fqnOf(t)
		if err != nil {
			return err
		}
		prev, ok := nodes[fqn]
		if !ok {
			prev, ok = r.fqns[fqn]
		}
		if ok {
			if prev != t {
				return fmt.Errorf("wire type %s names both %v and %v", fqn, prev, t)
			}
			return nil
		}
		nodes[fqn] = t
		var succ []string
		for _, e := range structEdges(t) {
			if err := collect(e); err != nil {
				return err
			}
			if _, done := r.hashes[e]; done {
				continue
			}
			efqn, _ := fqnOf(e)
			if !slices.Contains(succ, efqn) {
				succ = append(succ, efqn)
			}
		}
		slices.Sort(succ)
		edges[fqn] = succ
		return nil
	}
	if err := collect(root); err != nil {
		return err
	}
	saved := r.selfRefs
	defer func() { r.selfRefs = saved }()
	for _, scc := range tarjanSCC(nodes, edges) {
		order, back := sccOrder(scc, edges)
		for _, fqn := range order {
			t := nodes[fqn]
			r.selfRefs = back[fqn]
			js, err := r.typeDefJSON(t)
			if err != nil {
				return err
			}
			canon, err := ComputeCanonicalBytes("TypeDef", js)
			if err != nil {
				return err
			}
			h, err := Blake3Hex(canon)
			if err != nil {
				return err
			}
			r.hashes[t], r.jsons[t] = h, js
			r.fqns[fqn] = t
		}
	}
	return nil
}

// tarjanSCC returns the graph's strongly connected components, leaves
// (dependencies) first, visiting nodes and successors in sorted order.
func tarjanSCC(nodes map[string]reflect.Type, edges map[string][]string) [][]string {
	index := map[string]int{}
	low := map[string]int{}
	onStack := map[string]bool{}
	var stack []string
	var sccs [][]string
	next := 0
	var connect func(v string)
	connect = func(v string) {
		index[v], low[v] = next, next
		next++
		stack = append(stack, v)
		onStack[v] = true
		for _, w := range edges[v] {
			if _, seen := index[w]; !seen {
				connect(w)
				low[v] = min(low[v], low[w])
			} else if onStack[w] {
				low[v] = min(low[v], index[w])
			}
		}
		if low[v] == index[v] {
			var scc []string
			for {
				w := stack[len(stack)-1]
				stack = stack[:len(stack)-1]
				onStack[w] = false
				scc = append(scc, w)
				if w == v {
					break
				}
			}
			sccs = append(sccs, scc)
		}
	}
	names := make([]string, 0, len(nodes))
	for n := range nodes {
		names = append(names, n)
	}
	slices.Sort(names)
	for _, n := range names {
		if _, seen := index[n]; !seen {
			connect(n)
		}
	}
	return sccs
}

// sccOrder returns the component's members in hashing order (post-order of
// the DFS from the smallest member) and each member's SELF_REF targets. A
// self-loop is a SELF_REF too.
func sccOrder(scc []string, edges map[string][]string) ([]string, map[string]map[string]bool) {
	member := map[string]bool{}
	for _, m := range scc {
		member[m] = true
	}
	back := map[string]map[string]bool{}
	visited := map[string]bool{}
	var order []string
	var dfs func(v string)
	dfs = func(v string) {
		visited[v] = true
		for _, w := range edges[v] {
			if !member[w] {
				continue
			}
			if visited[w] {
				if back[v] == nil {
					back[v] = map[string]bool{}
				}
				back[v][w] = true
				continue
			}
			dfs(w)
		}
		order = append(order, v)
	}
	dfs(slices.Min(scc))
	return order, back
}
