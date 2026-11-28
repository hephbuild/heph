package main

import (
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"path"
	"regexp"
	"sort"
	"strings"
)

// Class is a changed line's mechanical class. Every class but live is
// Counted; live lines are what the triage tiers.
type Class string

const (
	ClassLive       Class = "live"
	ClassTest       Class = "test"
	ClassMoved      Class = "moved"
	ClassDocs       Class = "docs"
	ClassLockfile   Class = "lockfile"
	ClassGenerated  Class = "generated"
	ClassSuperseded Class = "superseded"
	ClassBinary     Class = "binary"
	ClassSymlink    Class = "symlink"
	ClassSubmodule  Class = "submodule"
	ClassMode       Class = "mode" // a file change with no line: mode flip, pure rename
	// ClassHarness is a rules-file class only: test support, read as live
	// code with a Glance floor, because a harness change can weaken every
	// test with no weakening signal.
	ClassHarness Class = "harness"
)

// Side is the side of the diff a line exists on.
type Side string

const (
	SideNew Side = "new"
	SideOld Side = "old"
)

// Loc is a line on one side of a layer's diff.
type Loc struct {
	Path string `json:"path"`
	Side Side   `json:"side"`
	Line int    `json:"line"`
}

// CLine is one changed line of a layer.
type CLine struct {
	Loc
	Text  string
	Class Class
	Flags []string
	Floor Tier
	Other *Loc // a moved line's other end
	file  *LFile
}

func (l *CLine) flag(f string) {
	for _, x := range l.Flags {
		if x == f {
			return
		}
	}
	l.Flags = append(l.Flags, f)
}

func (l *CLine) Kind() byte {
	if l.Side == SideNew {
		return '+'
	}
	return '-'
}

// LFile is one file of a layer's diff.
type LFile struct {
	FC        *FileChange
	Path      string // new path, or old path when deleted
	Lines     []*CLine
	PinsMoved int // Cargo.lock: existing pins whose version changed
	FileClass Class
	byLoc     map[Loc]*CLine
}

// LayerData is one layer's classified diff.
type LayerData struct {
	Layer
	Files   []*LFile
	Numstat int
	byLoc   map[Loc]*CLine
}

// Lines iterates every changed line of the layer in file order.
func (d *LayerData) Lines() []*CLine {
	var out []*CLine
	for _, f := range d.Files {
		out = append(out, f.Lines...)
	}
	return out
}

func (d *LayerData) Line(l Loc) *CLine { return d.byLoc[l] }

// File returns the file a path names on a side.
func (d *LayerData) File(p string, side Side) *LFile {
	for _, f := range d.Files {
		if (side == SideNew && f.FC.NewPath == p) || (side == SideOld && f.FC.OldPath == p) {
			return f
		}
	}
	return nil
}

// Stack is the whole analysed stack.
type Stack struct {
	Pin    *Pin
	Layers []*LayerData
	git    *Git
	rules  *Rules
}

var generatedRe = regexp.MustCompile(`@generated|Code generated .* DO NOT EDIT`)

// Analyze classifies every changed line of every layer of a pinned stack.
func Analyze(g *Git, pin *Pin, rules *Rules) (*Stack, error) {
	st := &Stack{Pin: pin, git: g, rules: rules}
	for _, l := range pin.Layers {
		d, err := classifyLayer(g, l, rules)
		if err != nil {
			return nil, fmt.Errorf("layer #%d: %w", l.PR, err)
		}
		st.Layers = append(st.Layers, d)
	}
	if pin.Mode == ModeWhole && len(st.Layers) > 1 {
		if err := st.markSuperseded(); err != nil {
			return nil, err
		}
	}
	for _, d := range st.Layers {
		markMoved(d, rules)
		markFloors(d, rules)
	}
	return st, nil
}

func classifyLayer(g *Git, l Layer, rules *Rules) (*LayerData, error) {
	fcs, err := g.Diff(l.Base, l.Head, 3)
	if err != nil {
		return nil, err
	}
	n, err := g.Numstat(l.Base, l.Head)
	if err != nil {
		return nil, err
	}
	d := &LayerData{Layer: l, Numstat: n, byLoc: map[Loc]*CLine{}}
	rtNew := NewRustTests(g, l.Head)
	rtOld := NewRustTests(g, l.Base)
	for _, fc := range fcs {
		f := &LFile{FC: fc, Path: fc.Path(), byLoc: map[Loc]*CLine{}}
		switch {
		case fc.Submodule():
			f.FileClass = ClassSubmodule
		case fc.Binary || fc.NonUTF8:
			f.FileClass = ClassBinary
		case fc.Symlink():
			f.FileClass = ClassSymlink
		case fc.ModeOnly():
			f.FileClass = ClassMode
		}
		for _, h := range fc.Hunks {
			for _, hl := range h.Lines {
				var cl *CLine
				switch hl.Kind {
				case '+':
					cl = &CLine{Loc: Loc{fc.NewPath, SideNew, hl.New}, Text: hl.Text}
				case '-':
					cl = &CLine{Loc: Loc{fc.OldPath, SideOld, hl.Old}, Text: hl.Text}
				default:
					continue
				}
				cl.file = f
				cl.Class = classOf(g, l, f, cl, rules, rtNew, rtOld)
				f.Lines = append(f.Lines, cl)
				f.byLoc[cl.Loc] = cl
				d.byLoc[cl.Loc] = cl
				if path.Base(f.Path) == "Cargo.lock" && hl.Kind == '-' && strings.HasPrefix(hl.Text, "version = ") {
					f.PinsMoved++
				}
			}
		}
		d.Files = append(d.Files, f)
	}
	sort.SliceStable(d.Files, func(i, j int) bool { return d.Files[i].Path < d.Files[j].Path })
	return d, nil
}

// classOf applies the precedence: file-level classes, generated, lockfile,
// docs, test, then live. Supersession and moves come later.
func classOf(g *Git, l Layer, f *LFile, cl *CLine, rules *Rules, rtNew, rtOld *RustTests) Class {
	if f.FileClass != "" {
		return f.FileClass
	}
	rev := l.Head
	rt := rtNew
	if cl.Side == SideOld {
		rev, rt = l.Base, rtOld
	}
	pc := rules.PathClass(cl.Path)
	if pc == ClassHarness {
		cl.flag("harness")
		return ClassLive
	}
	if isGenerated(g, rev, cl.Path) {
		return ClassGenerated
	}
	if pc == ClassGenerated || pc == ClassLockfile || pc == ClassDocs {
		return pc
	}
	if pc == ClassTest || strings.HasSuffix(cl.Path, "_test.go") {
		return ClassTest
	}
	if strings.HasSuffix(cl.Path, ".rs") {
		test, untrusted := rt.IsTestLine(cl.Path, cl.Line)
		if untrusted {
			cl.flag("untrusted-test-region")
			return ClassLive
		}
		if test {
			return ClassTest
		}
	}
	return ClassLive
}

func isGenerated(g *Git, rev, p string) bool {
	lines := g.Lines(rev, p)
	for i := 0; i < len(lines) && i < 5; i++ {
		if generatedRe.MatchString(lines[i]) {
			return true
		}
	}
	return false
}

// markSuperseded applies I9: in a whole-merge stack, a line one layer adds
// and a higher layer rewrites or deletes is Counted, and a line two layers
// both leave in the final form belongs to the higher one.
func (st *Stack) markSuperseded() error {
	g := st.git
	top := st.Layers[len(st.Layers)-1]
	type final struct {
		path string
		line int
	}
	claims := map[final][]*CLine{}
	for k, d := range st.Layers {
		var toTop map[string]*FileChange
		if d != top {
			fcs, err := g.Diff(d.Head, top.Head, 0)
			if err != nil {
				return err
			}
			toTop = byOldPath(fcs)
		}
		var fromBottom map[string]map[int]bool
		if k > 0 {
			fcs, err := g.Diff(st.Layers[0].Base, d.Base, 0)
			if err != nil {
				return err
			}
			fromBottom = addedLines(fcs)
		}
		for _, cl := range d.Lines() {
			if cl.Class != ClassLive {
				continue
			}
			switch cl.Side {
			case SideNew:
				fp, fl := cl.Path, cl.Line
				if fc, ok := toTop[cl.Path]; ok {
					m, ok := NewLineMap(fc).Map(cl.Line)
					if !ok {
						cl.Class = ClassSuperseded
						continue
					}
					fp, fl = fc.NewPath, m
				}
				claims[final{fp, fl}] = append(claims[final{fp, fl}], cl)
			case SideOld:
				// A removal of a line only a lower layer added never
				// reaches master: that line is not on master to remove.
				if fromBottom[cl.Path][cl.Line] {
					cl.Class = ClassSuperseded
				}
			}
		}
	}
	for _, cls := range claims {
		// Layers were walked bottom to top, so the last claimer is the owner.
		for _, cl := range cls[:len(cls)-1] {
			cl.Class = ClassSuperseded
		}
	}
	return nil
}

func byOldPath(fcs []*FileChange) map[string]*FileChange {
	m := map[string]*FileChange{}
	for _, fc := range fcs {
		if fc.OldPath != "" {
			m[fc.OldPath] = fc
		}
	}
	return m
}

func addedLines(fcs []*FileChange) map[string]map[int]bool {
	m := map[string]map[int]bool{}
	for _, fc := range fcs {
		if fc.NewPath == "" {
			continue
		}
		set := map[int]bool{}
		for _, h := range fc.Hunks {
			for _, l := range h.Lines {
				if l.Kind == '+' {
					set[l.New] = true
				}
			}
		}
		m[fc.NewPath] = set
	}
	return m
}

// D6: a moved block is 8 or more consecutive lines, 200 or more characters,
// identical after trimming.
const (
	moveMinLines = 8
	moveMinChars = 200
	moveMaxCands = 256
)

// markMoved finds blocks of live lines removed in one place and added in
// another. A block matches one removal only, so a block removed once and
// added twice is moved once and live once.
func markMoved(d *LayerData, rules *Rules) {
	removed := runs(d, SideOld)
	added := runs(d, SideNew)
	type pos struct{ run, i int }
	idx := map[string][]pos{}
	for r, run := range removed {
		for i, cl := range run {
			k := strings.TrimSpace(cl.Text)
			if len(idx[k]) < moveMaxCands {
				idx[k] = append(idx[k], pos{r, i})
			}
		}
	}
	used := map[*CLine]bool{}
	for _, run := range added {
		for i := 0; i < len(run); {
			best, bestLen, bestChars := pos{}, 0, 0
			for _, p := range idx[strings.TrimSpace(run[i].Text)] {
				src := removed[p.run]
				n, chars := 0, 0
				for i+n < len(run) && p.i+n < len(src) && !used[src[p.i+n]] &&
					strings.TrimSpace(run[i+n].Text) == strings.TrimSpace(src[p.i+n].Text) {
					chars += len(strings.TrimSpace(run[i+n].Text))
					n++
				}
				if n > bestLen {
					best, bestLen, bestChars = p, n, chars
				}
			}
			if bestLen < moveMinLines || bestChars < moveMinChars {
				i++
				continue
			}
			src := removed[best.run]
			for k := 0; k < bestLen; k++ {
				to, from := run[i+k], src[best.i+k]
				used[from] = true
				to.Other, from.Other = &from.Loc, &to.Loc
				switch {
				case to.file == from.file:
					// Match-arm and hash-write order carry meaning.
					to.flag("reorder")
					from.flag("reorder")
				case rules.IsProtected(to.Path):
					to.flag("moved-into-protected")
					from.Class = ClassMoved
				default:
					to.Class, from.Class = ClassMoved, ClassMoved
				}
			}
			i += bestLen
		}
	}
}

// runs groups a side's live lines into runs of consecutive line numbers
// within one file.
func runs(d *LayerData, side Side) [][]*CLine {
	var out [][]*CLine
	for _, f := range d.Files {
		var cur []*CLine
		var lines []*CLine
		for _, cl := range f.Lines {
			if cl.Side == side && cl.Class == ClassLive {
				lines = append(lines, cl)
			}
		}
		sort.Slice(lines, func(i, j int) bool { return lines[i].Line < lines[j].Line })
		for _, cl := range lines {
			if len(cur) > 0 && cur[len(cur)-1].Line+1 != cl.Line {
				out = append(out, cur)
				cur = nil
			}
			cur = append(cur, cl)
		}
		if len(cur) > 0 {
			out = append(out, cur)
		}
	}
	return out
}

// markFloors sets the floor of every live line: protected paths (I3),
// harness files, content tripwires (D10), reorders and moves into protected
// code.
func markFloors(d *LayerData, rules *Rules) {
	for _, cl := range d.Lines() {
		if cl.Class != ClassLive {
			continue
		}
		if rules.IsProtected(cl.Path) {
			cl.flag("protected")
		}
		for _, t := range rules.Tripped(cl.Path, cl.Text) {
			cl.flag("tripwire:" + t)
		}
		for _, f := range cl.Flags {
			switch {
			case f == "protected", f == "harness", f == "reorder", f == "moved-into-protected",
				strings.HasPrefix(f, "tripwire:"):
				cl.Floor = TierGlance
			}
		}
	}
}

// LiveLines counts a layer's live lines.
func (d *LayerData) LiveLines() int {
	n := 0
	for _, cl := range d.Lines() {
		if cl.Class == ClassLive {
			n++
		}
	}
	return n
}

// Fingerprint is a layer's content with every SHA and line number left
// out, so a rebase that moves nothing keeps it and a layer is scouted again
// only when its diff or its set of live lines changed.
func (d *LayerData) Fingerprint() string {
	h := sha256.New()
	for _, f := range d.Files {
		fmt.Fprintf(h, "F %s %s\n", f.FC.OldPath, f.FC.NewPath)
		for _, cl := range f.Lines {
			fmt.Fprintf(h, "%c%s %s\n", cl.Kind(), cl.Class, strings.TrimRight(cl.Text, " \t\r"))
		}
	}
	return hex.EncodeToString(h.Sum(nil))[:16]
}

// Symbol names the declaration around a changed line, for cross-layer links.
func (st *Stack) Symbol(d *LayerData, l Loc) string {
	rev := d.Head
	if l.Side == SideOld {
		rev = d.Base
	}
	return enclosingSymbol(st.git.Lines(rev, l.Path), l.Line)
}
