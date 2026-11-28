package main

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"time"
)

// Decisions carry the user's answers from one round's page to the next.
// The page copies a block ("Copy decisions for the next round"), the user
// pastes it into the next session, `brief decisions merge` folds it into a
// file kept across rounds, and index and render read that file. The page
// stays static: nothing but the user's paste carries the answers.

const decisionsFormat = "heph-review-brief-decisions/1"

const (
	decisionSettled = "settled" // an answer that keeps the code as it is
	decisionChange  = "change"  // an answer that asks for a code change
)

// DecisionRange is one range of a decided card, as the page had it.
type DecisionRange struct {
	Path    string `json:"path"`
	Side    Side   `json:"side"`
	Start   int    `json:"start"`
	End     int    `json:"end"`
	Hash    string `json:"hash"`    // the PageRange hash: what the decision follows
	Anchor  string `json:"anchor"`  // path#symbol, to find the code once it changed
	Changed int    `json:"changed"` // changed lines under the range, to find the same lines again
}

// UserDecision is one card or item the user answered.
type UserDecision struct {
	Layer    int             `json:"layer"`
	Card     string          `json:"card"`
	Title    string          `json:"title"`
	Question string          `json:"question,omitempty"`
	Decision string          `json:"decision"` // settled or change
	Choice   string          `json:"choice,omitempty"`
	Note     string          `json:"note,omitempty"`
	Ranges   []DecisionRange `json:"ranges"`
	// CopiedAt is when the block holding it was copied. The page leaves it
	// out; merge sets it, so newest wins across blocks.
	CopiedAt string `json:"copied_at,omitempty"`
}

// HeadRef is a layer's pinned head when the page was cut.
type HeadRef struct {
	PR   int    `json:"pr"`
	Head string `json:"head"`
}

// DecisionsFile is both the block the page copies and the file kept across
// rounds (`$ST/decisions.json`).
type DecisionsFile struct {
	Format    string         `json:"format"`
	StackKey  string         `json:"stack_key"`
	BottomPR  int            `json:"bottom_pr"`
	Heads     []HeadRef      `json:"heads"`
	CopiedAt  string         `json:"copied_at"`
	Decisions []UserDecision `json:"decisions"`
}

func stackKeyOf(p *Pin) string { return fmt.Sprintf("%s#%d", p.Repo, p.Layers[0].PR) }

// parseDecisions reads a pasted block: the JSON, or the JSON inside the
// ``` fence the page copies it in.
func parseDecisions(b []byte) (*DecisionsFile, error) {
	s := strings.TrimSpace(string(b))
	if strings.HasPrefix(s, "```") {
		if i := strings.IndexByte(s, '\n'); i >= 0 {
			s = s[i+1:]
		} else {
			s = ""
		}
		s = strings.TrimSpace(strings.TrimSuffix(strings.TrimSpace(s), "```"))
	}
	var f DecisionsFile
	dec := json.NewDecoder(strings.NewReader(s))
	dec.DisallowUnknownFields()
	if err := dec.Decode(&f); err != nil {
		return nil, fmt.Errorf("not a decisions block: %w", err)
	}
	return &f, nil
}

func loadDecisions(path string) (*DecisionsFile, error) {
	b, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	f, err := parseDecisions(b)
	if err != nil {
		return nil, fmt.Errorf("%s: %w", path, err)
	}
	return f, nil
}

func parseTime(s string) (time.Time, error) { return time.Parse(time.RFC3339Nano, s) }

// check lists every reason a decisions file is refused for the stack key.
func (f *DecisionsFile) check(stackKey string) Refusals {
	var ref Refusals
	if f.Format != decisionsFormat {
		ref.add("decisions format %q, want %q", f.Format, decisionsFormat)
		return ref
	}
	if f.StackKey != stackKey {
		ref.add("the decisions are for stack %q and this brief is %q: a block from another stack is refused", f.StackKey, stackKey)
	}
	if _, err := parseTime(f.CopiedAt); err != nil {
		ref.add("decisions: copied_at %q is not an RFC 3339 time", f.CopiedAt)
	}
	for i, d := range f.Decisions {
		where := fmt.Sprintf("decision %d (card %q)", i+1, d.Card)
		if d.Card == "" || d.Layer <= 0 {
			ref.add("%s: needs a card id and a layer", where)
		}
		if d.Decision != decisionSettled && d.Decision != decisionChange {
			ref.add("%s: decision %q is neither settled nor change", where, d.Decision)
		}
		if d.CopiedAt != "" {
			if _, err := parseTime(d.CopiedAt); err != nil {
				ref.add("%s: copied_at %q is not an RFC 3339 time", where, d.CopiedAt)
			}
		}
		if len(d.Ranges) == 0 {
			ref.add("%s: no ranges", where)
		}
		for _, r := range d.Ranges {
			if r.Path == "" || r.Hash == "" || (r.Side != SideNew && r.Side != SideOld) || r.Changed < 1 {
				ref.add("%s: range %s:%d-%d needs a path, a side, a hash and its changed lines", where, r.Path, r.Start, r.End)
			}
		}
	}
	return ref
}

// mergeDecisions folds a block into the kept file. A range's hash belongs to
// the newest decision naming it; an older decision keeps only the ranges no
// newer one names, and goes once it has none. On a tie the block wins.
func mergeDecisions(cur, in *DecisionsFile) *DecisionsFile {
	type dated struct {
		d  UserDecision
		at time.Time
	}
	var all []dated
	stamp := func(f *DecisionsFile) {
		for _, d := range f.Decisions {
			if d.CopiedAt == "" {
				d.CopiedAt = f.CopiedAt
			}
			at, _ := parseTime(d.CopiedAt)
			all = append(all, dated{d, at})
		}
	}
	stamp(in)
	out := &DecisionsFile{Format: decisionsFormat, StackKey: in.StackKey, BottomPR: in.BottomPR, Heads: in.Heads, CopiedAt: in.CopiedAt}
	if cur != nil {
		stamp(cur)
		curAt, _ := parseTime(cur.CopiedAt)
		inAt, _ := parseTime(in.CopiedAt)
		if curAt.After(inAt) {
			out.Heads, out.CopiedAt = cur.Heads, cur.CopiedAt
		}
	}
	sort.SliceStable(all, func(i, j int) bool { return all[i].at.After(all[j].at) })
	claimed := map[string]bool{}
	out.Decisions = []UserDecision{}
	for _, x := range all {
		var kept []DecisionRange
		for _, r := range x.d.Ranges {
			if !claimed[r.Hash] {
				kept = append(kept, r)
			}
		}
		for _, r := range kept {
			claimed[r.Hash] = true
		}
		if len(kept) == 0 {
			continue
		}
		x.d.Ranges = kept
		out.Decisions = append(out.Decisions, x.d)
	}
	sort.SliceStable(out.Decisions, func(i, j int) bool {
		a, b := out.Decisions[i], out.Decisions[j]
		if a.Layer != b.Layer {
			return a.Layer < b.Layer
		}
		return a.Card < b.Card
	})
	return out
}

// MergeDecisionsFile validates a pasted block against the pin and folds it
// into the kept file at statePath, written whole through a rename.
func MergeDecisionsFile(inPath, statePath string, pin *Pin) (*DecisionsFile, error) {
	key := stackKeyOf(pin)
	in, err := loadDecisions(inPath)
	if err != nil {
		return nil, err
	}
	if ref := in.check(key); len(ref) > 0 {
		return nil, fmt.Errorf("%s: %w", inPath, decisionsRefused(ref))
	}
	if in.BottomPR != pin.Layers[0].PR {
		return nil, fmt.Errorf("%s: bottom_pr #%d, and this stack's bottom is #%d", inPath, in.BottomPR, pin.Layers[0].PR)
	}
	var cur *DecisionsFile
	if _, err := os.Stat(statePath); err == nil {
		if cur, err = loadDecisions(statePath); err != nil {
			return nil, err
		}
		if ref := cur.check(key); len(ref) > 0 {
			return nil, fmt.Errorf("%s: %w", statePath, decisionsRefused(ref))
		}
	}
	out := mergeDecisions(cur, in)
	if err := os.MkdirAll(filepath.Dir(statePath), 0o755); err != nil {
		return nil, err
	}
	tmp := statePath + ".tmp"
	if err := writeJSON(tmp, out); err != nil {
		return nil, err
	}
	return out, os.Rename(tmp, statePath)
}

type decisionsRefused Refusals

func (r decisionsRefused) Error() string {
	return fmt.Sprintf("decisions refused (%d):\n  - %s", len(r), strings.Join(r, "\n  - "))
}

// Where a decision's code is at the new pins.
const (
	codeUnchanged = "unchanged" // every range's lines are still changed, as they were
	codeChanged   = "changed"   // the code is still there, but not as it was decided
	codeGone      = "gone"      // nothing at its anchors is changed any more
)

// codeFinder answers, for a decision taken on an earlier round's page,
// whether its code is still in the stack's diff as it was.
type codeFinder struct {
	st   *Stack
	seqs map[seqKey][]DiffLine
}

type seqKey struct {
	pr   int
	path string
	side Side
}

func newCodeFinder(st *Stack) *codeFinder { return &codeFinder{st: st, seqs: map[seqKey][]DiffLine{}} }

// changedSeq is a file's added and removed lines in diff order: a range's
// hash covers a run of them, whichever side it is on.
func (cf *codeFinder) changedSeq(d *LayerData, path string, side Side) []DiffLine {
	k := seqKey{d.PR, path, side}
	if s, ok := cf.seqs[k]; ok {
		return s
	}
	var s []DiffLine
	if f := d.File(path, side); f != nil {
		for _, h := range f.FC.Hunks {
			for _, hl := range h.Lines {
				if hl.Kind == '+' || hl.Kind == '-' {
					s = append(s, DiffLine{string(hl.Kind), hl.Old, hl.New, hl.Text})
				}
			}
		}
	}
	cf.seqs[k] = s
	return s
}

// found reports whether some layer still changes exactly the lines the
// range's hash was taken over: the same run, by rangeHash.
func (cf *codeFinder) found(r DecisionRange) bool {
	want, _, _ := strings.Cut(r.Hash, "-") // a repeated block's ordinal is not content
	for _, d := range cf.st.Layers {
		s := cf.changedSeq(d, r.Path, r.Side)
		for i := 0; i+r.Changed <= len(s); i++ {
			if rangeHash(r.Path, s[i:i+r.Changed]) == want {
				return true
			}
		}
	}
	return false
}

// atAnchor lists the live lines, as ranges, that the stack changes at the
// range's anchor (its path and enclosing symbol).
func (cf *codeFinder) atAnchor(r DecisionRange) []LayerRef {
	var out []LayerRef
	for _, d := range cf.st.Layers {
		f := d.File(r.Path, r.Side)
		if f == nil {
			continue
		}
		var at []*CLine
		for _, cl := range f.Lines {
			if cl.Class != ClassLive || cl.Path != r.Path {
				continue
			}
			if r.Path+"#"+cf.st.Symbol(d, cl.Loc) == r.Anchor {
				at = append(at, cl)
			}
		}
		for _, rr := range collapse(at) {
			out = append(out, LayerRef{Layer: d.PR, RangeRef: rr})
		}
	}
	return out
}

// status says where a decision's code is now, and where its anchors' live
// lines are when it changed.
func (cf *codeFinder) status(dec UserDecision) (string, []LayerRef) {
	all, some := true, false
	var now []LayerRef
	for _, r := range dec.Ranges {
		if cf.found(r) {
			some = true
			continue
		}
		all = false
		if at := cf.atAnchor(r); len(at) > 0 {
			some = true
			now = append(now, at...)
		}
	}
	switch {
	case all:
		return codeUnchanged, nil
	case some:
		return codeChanged, now
	default:
		return codeGone, nil
	}
}

// instruction is what the decider is told to do with an earlier decision.
func instruction(dec UserDecision, status string) string {
	if dec.Decision == decisionSettled {
		switch status {
		case codeUnchanged:
			return "settled, do not raise again"
		case codeChanged:
			return "the code changed after it was settled; re-check"
		default:
			return "settled, and its code is gone: nothing to raise"
		}
	}
	asked := "a change"
	if dec.Choice != "" {
		asked = fmt.Sprintf("%q", dec.Choice)
	}
	s := "you asked for " + asked + ": check whether the change was made"
	switch status {
	case codeUnchanged:
		return s + " (the code is unchanged, so it looks not made)"
	case codeChanged:
		return s + " (the code changed)"
	default:
		return s + " (the code is gone)"
	}
}

// EarlierDecision is one decision as the index reports it.
type EarlierDecision struct {
	UserDecision
	Status      string     `json:"status"` // unchanged, changed or gone
	Instruction string     `json:"instruction"`
	Now         []LayerRef `json:"now,omitempty"` // the live lines at its anchors, when it changed
}

func (st *Stack) earlierDecisions(f *DecisionsFile) []EarlierDecision {
	if f == nil {
		return nil
	}
	cf := newCodeFinder(st)
	out := []EarlierDecision{}
	for _, d := range mergeDecisions(nil, f).Decisions {
		status, now := cf.status(d)
		out = append(out, EarlierDecision{UserDecision: d, Status: status, Instruction: instruction(d, status), Now: now})
	}
	return out
}

// decisionContext is what render knows of the earlier decisions.
type decisionContext struct {
	byHash   map[string]*UserDecision
	byAnchor map[string][]*UserDecision
	status   map[*UserDecision]string
	cf       *codeFinder
}

func (st *Stack) newDecisionContext(f *DecisionsFile) *decisionContext {
	dc := &decisionContext{byHash: map[string]*UserDecision{}, byAnchor: map[string][]*UserDecision{},
		status: map[*UserDecision]string{}, cf: newCodeFinder(st)}
	if f == nil {
		return dc
	}
	m := mergeDecisions(nil, f) // newest first wins each hash
	for i := range m.Decisions {
		d := &m.Decisions[i]
		for _, r := range d.Ranges {
			dc.byHash[r.Hash] = d
			dc.byAnchor[r.Anchor] = append(dc.byAnchor[r.Anchor], d)
		}
	}
	return dc
}

func (dc *decisionContext) statusOf(d *UserDecision) string {
	s, ok := dc.status[d]
	if !ok {
		s, _ = dc.cf.status(*d)
		dc.status[d] = s
	}
	return s
}

// PageEarlier is what a card shows of an earlier round's answer.
type PageEarlier struct {
	Decision string `json:"decision"` // settled or change
	Choice   string `json:"choice,omitempty"`
	Note     string `json:"note,omitempty"`
	Code     string `json:"code"` // unchanged or changed
}

func earlierOf(d *UserDecision, code string) *PageEarlier {
	return &PageEarlier{Decision: d.Decision, Choice: d.Choice, Note: d.Note, Code: code}
}

// forItem says what an item's ranges' hashes and anchors carry from an
// earlier round: settled is true when every range is code the user settled
// and it is unchanged, which moves the item out of the tiers.
func (dc *decisionContext) forItem(hashes, anchors []string) (settled bool, e *PageEarlier) {
	if len(hashes) == 0 {
		return false, nil
	}
	all := true
	var change, partly *UserDecision
	for _, h := range hashes {
		d := dc.byHash[h]
		switch {
		case d == nil:
			all = false
		case d.Decision == decisionChange:
			all = false
			if change == nil {
				change = d
			}
		case partly == nil:
			partly = d
		}
	}
	switch {
	case all:
		return true, earlierOf(dc.byHash[hashes[0]], codeUnchanged)
	case change != nil:
		return false, earlierOf(change, codeUnchanged)
	case partly != nil:
		return false, earlierOf(partly, codeChanged)
	}
	var newest *UserDecision
	var newestAt time.Time
	for _, a := range anchors {
		for _, d := range dc.byAnchor[a] {
			if dc.statusOf(d) != codeChanged {
				continue
			}
			at, _ := parseTime(d.CopiedAt)
			if newest == nil || at.After(newestAt) {
				newest, newestAt = d, at
			}
		}
	}
	if newest != nil {
		return false, earlierOf(newest, codeChanged)
	}
	return false, nil
}
