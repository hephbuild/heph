package main

import (
	"fmt"
	"sort"
	"strings"
)

// Weakening is a way a layer made its tests weaker (I5). Each must be the
// subject of a Scrutinize card, or render refuses.
type Weakening struct {
	ID     string    `json:"id"`
	Kind   string    `json:"kind"` // deleted-test, new-ignore, lost-assertions, missing-spec-test
	Test   string    `json:"test"`
	Range  *RangeRef `json:"range,omitempty"`
	Detail string    `json:"detail"`
}

func testFns(g *Git, rev, p string) []TestFn {
	lines := g.Lines(rev, p)
	if lines == nil {
		return nil
	}
	switch {
	case strings.HasSuffix(p, "_test.go"):
		return goTestFns(p, lines)
	case strings.HasSuffix(p, ".rs"):
		return rustTestFns(p, strings.Join(lines, "\n")+"\n")
	}
	return nil
}

type testKey struct{ path, name string }

// Weakenings compares the tests of every file a layer changed at its base
// and at its head. A test that moved to another changed file, or kept its
// body under a new name, is not deleted.
func (st *Stack) Weakenings(d *LayerData) []Weakening {
	g := st.git
	base := map[testKey]TestFn{}
	head := map[testKey]TestFn{}
	newPathOf := map[string]string{}
	oldPathOf := map[string]string{}
	for _, f := range d.Files {
		fc := f.FC
		if fc.OldPath != "" {
			newPathOf[fc.OldPath] = fc.NewPath
			for _, t := range testFns(g, d.Base, fc.OldPath) {
				base[testKey{fc.OldPath, t.Name}] = t
			}
		}
		if fc.NewPath != "" {
			oldPathOf[fc.NewPath] = fc.OldPath
			for _, t := range testFns(g, d.Head, fc.NewPath) {
				head[testKey{fc.NewPath, t.Name}] = t
			}
		}
	}
	// Tests a file gained: where a moved or renamed test went.
	gainedByName := map[string]TestFn{}
	gainedByBody := map[string]TestFn{}
	for k, t := range head {
		if _, ok := base[testKey{oldPathOf[k.path], k.name}]; !ok {
			gainedByName[k.name] = t
			gainedByBody[t.BodyHash] = t
		}
	}
	// counterpart is the test at head that a base test became.
	counterpart := func(k testKey, t TestFn) (TestFn, bool) {
		if h, ok := head[testKey{newPathOf[k.path], k.name}]; ok {
			return h, true
		}
		if h, ok := gainedByName[k.name]; ok {
			return h, true
		}
		h, ok := gainedByBody[t.BodyHash]
		return h, ok
	}
	var out []Weakening
	matched := map[testKey]bool{}
	for k, t := range base {
		h, ok := counterpart(k, t)
		if !ok {
			out = append(out, Weakening{
				ID: "deleted-test:" + k.path + ":" + k.name, Kind: "deleted-test", Test: k.name,
				Range:  &RangeRef{Path: k.path, Side: SideOld, Start: t.Start, End: t.End},
				Detail: fmt.Sprintf("%s was deleted and no test with its name or body was added", k.name),
			})
			continue
		}
		matched[testKey{h.Path, h.Name}] = true
		if h.Ignored && !t.Ignored {
			out = append(out, newIgnore(h))
		}
		if h.Asserts < t.Asserts {
			out = append(out, Weakening{
				ID: "lost-assertions:" + h.Path + ":" + h.Name, Kind: "lost-assertions", Test: h.Name,
				Range:  &RangeRef{Path: k.path, Side: SideOld, Start: t.Start, End: t.End},
				Detail: fmt.Sprintf("%s had %d assertions and now has %d", h.Name, t.Asserts, h.Asserts),
			})
		}
	}
	for k, h := range head {
		if !matched[k] && h.Ignored {
			out = append(out, newIgnore(h)) // a new test that never ran
		}
	}
	sort.Slice(out, func(i, j int) bool { return out[i].ID < out[j].ID })
	return out
}

func newIgnore(h TestFn) Weakening {
	return Weakening{
		ID: "new-ignore:" + h.Path + ":" + h.Name, Kind: "new-ignore", Test: h.Name,
		Range:  &RangeRef{Path: h.Path, Side: SideNew, Start: h.Start, End: h.End},
		Detail: fmt.Sprintf("%s is ignored or skipped at the head and was not at the base", h.Name),
	}
}

// MissingSpecTests returns the spec's named tests that do not exist at the
// top of the stack. They belong to the top layer.
func (st *Stack) MissingSpecTests(names []string) []Weakening {
	top := st.Layers[len(st.Layers)-1]
	var out []Weakening
	for _, n := range names {
		if found, _ := st.testExists(top.Head, TestRef{Name: n}); !found {
			out = append(out, Weakening{
				ID: "missing-spec-test:" + n, Kind: "missing-spec-test", Test: n,
				Detail: fmt.Sprintf("the spec names %s and no Rust or Go source at the head mentions it", n),
			})
		}
	}
	return out
}

// testExists looks a named test up at a revision. A test is found when its
// name occurs as a word in a Rust or Go source (so a macro-generated test
// counts); it is ignored when a test function of that name carries
// #[ignore] or calls t.Skip.
func (st *Stack) testExists(rev string, t TestRef) (found, ignored bool) {
	args := []string{"grep", "-n", "-w", "-F", "-e", t.Name, rev, "--"}
	if t.Path != "" {
		args = append(args, t.Path)
	} else {
		args = append(args, "*.rs", "*.go")
	}
	out, err := st.git.Run(args...)
	if err != nil || len(out) == 0 {
		return false, false
	}
	files := map[string]bool{}
	for _, l := range splitLines(string(out)) {
		// <rev>:<path>:<line>:<text>
		rest := strings.TrimPrefix(l, rev+":")
		if p, _, ok := strings.Cut(rest, ":"); ok {
			files[p] = true
		}
	}
	for p := range files {
		for _, fn := range testFns(st.git, rev, p) {
			if fn.Name == t.Name && fn.Ignored {
				return true, true
			}
		}
	}
	return true, false
}
