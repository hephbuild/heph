package main

import (
	"crypto/sha256"
	"fmt"
	"io"
	"sort"
	"strings"
)

const indexFormat = "heph-review-brief-index/1"

// IndexRange is a run of live lines with what the program knows about it.
type IndexRange struct {
	RangeRef
	Lines  int      `json:"lines"`
	Flags  []string `json:"flags,omitempty"`
	Symbol string   `json:"symbol,omitempty"`
	Floor  Tier     `json:"floor,omitempty"`
}

type IndexLayer struct {
	PR          int           `json:"pr"`
	Title       string        `json:"title"`
	Base        string        `json:"base"`
	Head        string        `json:"head"`
	Fingerprint string        `json:"fingerprint"`
	Total       int           `json:"total"`
	Live        int           `json:"live"`
	Budget      Budget        `json:"budget"`
	LiveRanges  []IndexRange  `json:"live_ranges"`
	Counted     map[Class]int `json:"counted"`
	Weakenings  []Weakening   `json:"weakenings"`
}

// Duplicate is a pair of near-identical added blocks: seeds for the stack
// reviewer's duplication question.
type Duplicate struct {
	A LayerRef `json:"a"`
	B LayerRef `json:"b"`
}

type FileTouch struct {
	Path   string `json:"path"`
	Layers []int  `json:"layers"`
}

type Index struct {
	Format     string       `json:"format"`
	Pin        *Pin         `json:"pin"`
	Layers     []IndexLayer `json:"layers"`
	Duplicates []Duplicate  `json:"duplicates"`
	Files      []FileTouch  `json:"files_touched_by_several_layers"`
}

// BuildIndex is the classified stack as the triage agents read it.
func (st *Stack) BuildIndex(specTests []string) *Index {
	ix := &Index{Format: indexFormat, Pin: st.Pin, Duplicates: []Duplicate{}, Files: []FileTouch{}}
	touch := map[string][]int{}
	for i, d := range st.Layers {
		il := IndexLayer{PR: d.PR, Title: d.Title, Base: d.Base, Head: d.Head, Fingerprint: d.Fingerprint(),
			Total: len(d.Lines()), Live: d.LiveLines(), Counted: map[Class]int{}, Weakenings: st.Weakenings(d)}
		if i == len(st.Layers)-1 {
			il.Weakenings = append(il.Weakenings, st.MissingSpecTests(specTests)...)
		}
		if il.Weakenings == nil {
			il.Weakenings = []Weakening{}
		}
		il.Budget = BudgetFor(il.Live)
		var live []*CLine
		for _, cl := range d.Lines() {
			if cl.Class == ClassLive {
				live = append(live, cl)
			} else {
				il.Counted[cl.Class]++
			}
		}
		for _, f := range d.Files {
			touch[f.Path] = append(touch[f.Path], d.PR)
		}
		for _, r := range collapse(live) {
			ir := IndexRange{RangeRef: r, Lines: r.End - r.Start + 1}
			flags := map[string]bool{}
			for l := r.Start; l <= r.End; l++ {
				cl := d.Line(Loc{r.Path, r.Side, l})
				for _, fl := range cl.Flags {
					flags[fl] = true
				}
				ir.Floor = max(ir.Floor, cl.Floor)
			}
			for fl := range flags {
				ir.Flags = append(ir.Flags, fl)
			}
			sort.Strings(ir.Flags)
			ir.Symbol = st.Symbol(d, Loc{r.Path, r.Side, r.Start})
			il.LiveRanges = append(il.LiveRanges, ir)
		}
		ix.Layers = append(ix.Layers, il)
	}
	for p, prs := range touch {
		if len(prs) > 1 {
			ix.Files = append(ix.Files, FileTouch{Path: p, Layers: prs})
		}
	}
	sort.Slice(ix.Files, func(i, j int) bool { return ix.Files[i].Path < ix.Files[j].Path })
	ix.Duplicates = st.duplicates()
	return ix
}

// Duplicate detection: windows of dupWindow added live lines, compared
// with whitespace collapsed; windows with fewer than dupMinChars are noise
// (closing braces, blank lines).
const (
	dupWindow   = 6
	dupMinChars = 150
)

func (st *Stack) duplicates() []Duplicate {
	type at struct {
		layer int
		path  string
		line  int
	}
	seen := map[[32]byte][]at{}
	for _, d := range st.Layers {
		for _, run := range runs(d, SideNew) {
			for i := 0; i+dupWindow <= len(run); i++ {
				var b strings.Builder
				for _, cl := range run[i : i+dupWindow] {
					b.WriteString(strings.Join(strings.Fields(cl.Text), " "))
					b.WriteByte('\n')
				}
				if b.Len() < dupMinChars {
					continue
				}
				k := sha256.Sum256([]byte(b.String()))
				seen[k] = append(seen[k], at{d.PR, run[i].Path, run[i].Line})
			}
		}
	}
	// Coalesce overlapping windows of the same pair into one range.
	type pairKey struct {
		la, lb int
		pa, pb string
		delta  int
	}
	starts := map[pairKey][]int{}
	for _, locs := range seen {
		for i := 0; i < len(locs); i++ {
			for j := i + 1; j < len(locs); j++ {
				a, b := locs[i], locs[j]
				if a.layer == b.layer && a.path == b.path && abs(a.line-b.line) < dupWindow {
					continue // a window overlapping itself
				}
				if a.layer > b.layer || (a.layer == b.layer && (a.path > b.path || (a.path == b.path && a.line > b.line))) {
					a, b = b, a
				}
				k := pairKey{a.layer, b.layer, a.path, b.path, b.line - a.line}
				starts[k] = append(starts[k], a.line)
			}
		}
	}
	var out []Duplicate
	for k, ls := range starts {
		sort.Ints(ls)
		for i := 0; i < len(ls); {
			j := i
			for j+1 < len(ls) && ls[j+1] <= ls[j]+1 {
				j++
			}
			s, e := ls[i], ls[j]+dupWindow-1
			out = append(out, Duplicate{
				A: LayerRef{Layer: k.la, RangeRef: RangeRef{Path: k.pa, Side: SideNew, Start: s, End: e}},
				B: LayerRef{Layer: k.lb, RangeRef: RangeRef{Path: k.pb, Side: SideNew, Start: s + k.delta, End: e + k.delta}},
			})
			i = j + 1
		}
	}
	sort.Slice(out, func(i, j int) bool {
		a, b := out[i].A, out[j].A
		if a.Layer != b.Layer {
			return a.Layer < b.Layer
		}
		if a.Path != b.Path {
			return a.Path < b.Path
		}
		return a.Start < b.Start
	})
	return out
}

func abs(x int) int {
	if x < 0 {
		return -x
	}
	return x
}

// WriteText prints the index for an agent: one line per live range.
func (ix *Index) WriteText(w io.Writer) {
	fmt.Fprintf(w, "# Review brief index (%s mode, %s)\n\n", ix.Pin.Mode, ix.Pin.Repo)
	for i, l := range ix.Layers {
		pos := ""
		switch {
		case len(ix.Layers) == 1:
		case i == 0:
			pos = " (bottom)"
		case i == len(ix.Layers)-1:
			pos = " (top)"
		}
		fmt.Fprintf(w, "## Layer #%d%s: %s\n\n", l.PR, pos, l.Title)
		fmt.Fprintf(w, "range %s..%s · %d changed lines, %d live · budget %d Scrutinize lines / %d cards · fingerprint %s\n\n",
			short(l.Base), short(l.Head), l.Total, l.Live, l.Budget.Lines, l.Budget.Cards, l.Fingerprint)
		var cs []string
		for c, n := range l.Counted {
			cs = append(cs, fmt.Sprintf("%s %d", c, n))
		}
		sort.Strings(cs)
		if len(cs) > 0 {
			fmt.Fprintf(w, "Counted: %s\n\n", strings.Join(cs, ", "))
		}
		if len(l.Weakenings) > 0 {
			fmt.Fprintf(w, "Weakenings (each needs a Scrutinize card citing its id):\n")
			for _, wk := range l.Weakenings {
				loc := ""
				if wk.Range != nil {
					loc = " at " + wk.Range.String()
				}
				fmt.Fprintf(w, "- %s%s: %s\n", wk.ID, loc, wk.Detail)
			}
			fmt.Fprintln(w)
		}
		fmt.Fprintf(w, "Live ranges (path:start-end side, lines, floor, flags, symbol):\n")
		for _, r := range l.LiveRanges {
			floor := ""
			if r.Floor > TierNone {
				floor = " floor=" + r.Floor.String()
			}
			flags := ""
			if len(r.Flags) > 0 {
				flags = " [" + strings.Join(r.Flags, ",") + "]"
			}
			fmt.Fprintf(w, "- %s:%d-%d %s, %d%s%s  %s\n", r.Path, r.Start, r.End, r.Side, r.Lines, floor, flags, r.Symbol)
		}
		fmt.Fprintln(w)
	}
	if len(ix.Files) > 0 {
		fmt.Fprintf(w, "## Files several layers touch\n\n")
		for _, f := range ix.Files {
			fmt.Fprintf(w, "- %s: %v\n", f.Path, f.Layers)
		}
		fmt.Fprintln(w)
	}
	if len(ix.Duplicates) > 0 {
		fmt.Fprintf(w, "## Near-identical added blocks\n\n")
		for _, d := range ix.Duplicates {
			fmt.Fprintf(w, "- #%d %s ~ #%d %s\n", d.A.Layer, d.A.RangeRef, d.B.Layer, d.B.RangeRef)
		}
	}
}
