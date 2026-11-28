package main

import (
	"fmt"
	"strings"
)

// The files the model stages write. Every one is data the program checks;
// none of them carries a diff, a SHA or a line count (I2).

const (
	triageFormat  = "heph-review-brief-triage/2"
	auditFormat   = "heph-review-brief-audit/1"
	surfaceFormat = "heph-review-brief-surface/1"
	scoutFormat   = "heph-review-brief-scout/1"
)

// RangeRef is a `file:line-range` on one side of a layer's diff.
type RangeRef struct {
	Path  string `json:"path"`
	Side  Side   `json:"side,omitempty"` // new (default) or old, for removed lines
	Start int    `json:"start"`
	End   int    `json:"end"`
	Note  string `json:"note,omitempty"` // what this range holds, for a linked card
}

func (r RangeRef) side() Side {
	if r.Side == "" {
		return SideNew
	}
	return r.Side
}

func (r RangeRef) String() string {
	return fmt.Sprintf("%s:%d-%d (%s)", r.Path, r.Start, r.End, r.side())
}

func (r RangeRef) covers(l Loc) bool {
	return r.Path == l.Path && r.side() == l.Side && r.Start <= l.Line && l.Line <= r.End
}

// TestRef names a test that pins a card's behavior.
type TestRef struct {
	Name string `json:"name"`
	Path string `json:"path,omitempty"`
}

// BoardNote is what a board agent said about a card's code.
type BoardNote struct {
	Agent  string `json:"agent"`
	Said   string `json:"said"`
	Status string `json:"status"` // fixed, overruled, open
}

// Item is one card or group: a tier, a reason, a claim and the ranges it is
// about (I12). Title, context, example and used_by are written for a
// reviewer who has not read the code: what changed, what it is for, what
// reaches it and how it shows up.
type Item struct {
	ID       string     `json:"id"`
	Layer    int        `json:"layer"`
	Tier     Tier       `json:"tier"`
	Reason   string     `json:"reason,omitempty"`
	Title    string     `json:"title,omitempty"`
	Context  string     `json:"context,omitempty"`
	Claim    string     `json:"claim"`
	Question string     `json:"question,omitempty"`
	Options  []Option   `json:"options,omitempty"` // what each answer to the question costs
	Diagram  string     `json:"diagram,omitempty"` // Mermaid source, from a small allowed subset
	Examples []Example  `json:"examples,omitempty"`
	Ranges   []RangeRef `json:"ranges"`
	UsedBy   []RangeRef `json:"used_by,omitempty"` // sites at the layer's head that use the changed code

	StartFrom  []string    `json:"start_from,omitempty"` // for a whole new file: the functions to read first
	Tests      []TestRef   `json:"tests,omitempty"`
	Board      []BoardNote `json:"board,omitempty"`
	Weakenings []string    `json:"weakenings,omitempty"`
	Surfaced   string      `json:"surfaced,omitempty"` // the surface item this carries
	ProposedBy string      `json:"proposed_by"`
	TieredBy   string      `json:"tiered_by,omitempty"`
	RaisedBy   string      `json:"raised_by,omitempty"`
}

// Option is one answer to a card's question and what follows from it.
type Option struct {
	Choice string `json:"choice"`
	Impact string `json:"impact"`
}

// Example is a concrete picture of how a change shows up: an error message,
// CLI output, a BUILD snippet, or one side of a before/after.
type Example struct {
	Label string `json:"label"`
	Text  string `json:"text"`
}

// Read is the ranges an agent reports it read.
type Read struct {
	Agent  string     `json:"agent"`
	Layer  int        `json:"layer"`
	Ranges []RangeRef `json:"ranges"`
}

// NotInCode is a point with no code to show (I12): not a card.
type NotInCode struct {
	Text string `json:"text"`
	Link string `json:"link"`
}

// Triage is the decider's answer.
type Triage struct {
	Format    string      `json:"format"`
	Items     []Item      `json:"items"`
	NotInCode []NotInCode `json:"not_in_code,omitempty"`
	Reads     []Read      `json:"reads"`
}

// Assessment is the stack reviewer's paragraph; every statement cites code.
type Assessment struct {
	Text string     `json:"text"`
	Refs []LayerRef `json:"refs"`
}

// LayerRef is a range in a named layer.
type LayerRef struct {
	Layer int `json:"layer"`
	RangeRef
}

// Promotion raises an item's tier. A promotion can never lower one (I4).
// One that raises an item to Scrutinize supplies what the card needs and the
// item lacks: reason, title, context, question and options.
type Promotion struct {
	Item     string   `json:"item"`
	Tier     Tier     `json:"tier"`
	Note     string   `json:"note"`
	Reason   string   `json:"reason,omitempty"`
	Title    string   `json:"title,omitempty"`
	Context  string   `json:"context,omitempty"`
	Question string   `json:"question,omitempty"`
	Options  []Option `json:"options,omitempty"`
}

// Confirmation is the stack reviewer approving a layer's AI-cleared tier.
type Confirmation struct {
	Layer int    `json:"layer"`
	Note  string `json:"note"`
}

// Audit is the stack reviewer's answer.
type Audit struct {
	Format        string         `json:"format"`
	Assessment    Assessment     `json:"assessment"`
	Promotions    []Promotion    `json:"promotions,omitempty"`
	Items         []Item         `json:"items,omitempty"`
	Confirmations []Confirmation `json:"confirmations,omitempty"`
	Reads         []Read         `json:"reads"`
}

// SurfaceItem is what a board agent asked the user to see.
type SurfaceItem struct {
	ID       string     `json:"id"`
	Agent    string     `json:"agent"`
	Layer    int        `json:"layer"`
	Tier     Tier       `json:"tier"` // the board's own call: the card's floor
	Reason   string     `json:"reason"`
	Claim    string     `json:"claim"`
	Question string     `json:"question,omitempty"`
	Status   string     `json:"status,omitempty"`
	Ranges   []RangeRef `json:"ranges"`
}

// Surface is what `/board review` wrote on the pushed SHA.
type Surface struct {
	Format string        `json:"format"`
	Items  []SurfaceItem `json:"items"`
	Vouch  []Read        `json:"vouched,omitempty"`
}

// ScoutCandidate is a change a scout proposes the decider look at.
// Its context, examples, diagram and used_by are proposals for the decider's
// card; render checks them only once the decider has written them there.
type ScoutCandidate struct {
	Claim    string     `json:"claim"`
	Reason   string     `json:"reason,omitempty"`
	Tier     Tier       `json:"tier"`
	Why      string     `json:"why,omitempty"`
	Context  string     `json:"context,omitempty"`
	Examples []Example  `json:"examples,omitempty"`
	Diagram  string     `json:"diagram,omitempty"`
	Ranges   []RangeRef `json:"ranges"`
	UsedBy   []RangeRef `json:"used_by,omitempty"`
}

// Scout is one scout's answer for (part of) one layer.
type Scout struct {
	Format     string           `json:"format"`
	Agent      string           `json:"agent"`
	Layer      int              `json:"layer"`
	Candidates []ScoutCandidate `json:"candidates"`
	Reads      []RangeRef       `json:"reads"`
}

// Reason is one entry of the closed list: every card says which rule put
// it in front of the user.
type Reason struct {
	Slug, Title string
}

var Reasons = []Reason{
	{"cache-key", "Cache key"},
	{"stored-format", "Stored format"},
	{"concurrency", "Concurrency and lifetimes"},
	{"engine-flow", "Engine flow"},
	{"compatibility", "Compatibility"},
	{"security", "Security and trust"},
	{"hermeticity", "Hermeticity"},
	{"unsafe-seam", "Unsafe and the plugin seam"},
	{"platform", "Platform divergence"},
	{"decision", "A decision made for you"},
	{"low-confidence", "Low confidence"},
	{"weakened-test", "Weakened test"},
	{"stack-design", "Stack design"},
}

func reasonTitle(slug string) (string, bool) {
	for _, r := range Reasons {
		if r.Slug == slug {
			return r.Title, true
		}
	}
	return "", false
}

// Refusals collects every reason render will not finish, so the triage
// agent fixes them in one round.
type Refusals []string

func (r *Refusals) add(format string, args ...any) { *r = append(*r, fmt.Sprintf(format, args...)) }

func (r Refusals) Error() string {
	return fmt.Sprintf("render refused (%d):\n  - %s", len(r), strings.Join(r, "\n  - "))
}

// checkRange refuses a range outside the layer's pinned diff: a file the
// layer does not change on that side, an inverted range, a line past the
// file's end, or a range holding none of the layer's changed lines.
func (st *Stack) checkRange(d *LayerData, r RangeRef, where string, ref *Refusals) {
	side := r.side()
	if side != SideNew && side != SideOld {
		ref.add("%s: range %s: side %q is neither new nor old", where, r, r.Side)
		return
	}
	if r.Start < 1 || r.End < r.Start {
		ref.add("%s: range %s is empty or inverted", where, r)
		return
	}
	f := d.File(r.Path, side)
	if f == nil {
		ref.add("%s: range %s is outside layer #%d's diff: the layer does not change %s on the %s side", where, r, d.PR, r.Path, side)
		return
	}
	rev := d.Head
	if side == SideOld {
		rev = d.Base
	}
	if n := len(st.git.Lines(rev, r.Path)); r.End > n {
		ref.add("%s: range %s runs past the end of %s (%d lines on the %s side)", where, r, r.Path, n, side)
		return
	}
	for _, cl := range f.Lines {
		if r.covers(cl.Loc) {
			return
		}
	}
	ref.add("%s: range %s is outside layer #%d's diff: it holds none of the layer's changed lines", where, r, d.PR)
}

func (st *Stack) layer(pr int) *LayerData {
	for _, d := range st.Layers {
		if d.PR == pr {
			return d
		}
	}
	return nil
}

// checkItem applies every per-item refusal.
func (st *Stack) checkItem(it *Item, ref *Refusals) {
	where := fmt.Sprintf("item %q", it.ID)
	if it.ID == "" {
		ref.add("an item has no id (claim %q)", it.Claim)
	}
	d := st.layer(it.Layer)
	if d == nil {
		ref.add("%s: layer #%d is not in the stack", where, it.Layer)
		return
	}
	if it.Tier == TierNone {
		ref.add("%s: no tier", where)
	}
	if strings.TrimSpace(it.Claim) == "" {
		ref.add("%s: no claim", where)
	}
	if it.Reason != "" {
		if _, ok := reasonTitle(it.Reason); !ok {
			ref.add("%s: reason %q is not in the closed list", where, it.Reason)
		}
	}
	if it.Tier == TierScrutinize {
		if it.Reason == "" {
			ref.add("%s: a Scrutinize card needs a reason", where)
		}
		if strings.TrimSpace(it.Question) == "" {
			ref.add("%s: a Scrutinize card needs a question", where)
		}
	}
	st.checkCard(d, it, where, ref)
	if len(it.Ranges) == 0 {
		ref.add("%s: no code range: every card is rooted in code (I12); a point with no code goes in not_in_code", where)
	}
	for _, r := range it.Ranges {
		st.checkRange(d, r, where, ref)
	}
	for _, t := range it.Tests {
		found, ignored := st.testExists(d.Head, t)
		switch {
		case !found:
			ref.add("%s: test %q does not exist at #%d's head %s", where, t.Name, d.PR, short(d.Head))
		case ignored:
			ref.add("%s: test %q is ignored at #%d's head, so it pins nothing", where, t.Name, d.PR)
		}
	}
	for _, b := range it.Board {
		switch b.Status {
		case "fixed", "overruled", "open":
		default:
			ref.add("%s: board note from %s has status %q (want fixed, overruled or open)", where, b.Agent, b.Status)
		}
	}
}

// Merge applies the stack reviewer's answer to the triage: new items are
// added, promotions raise a tier, and nothing lowers one (I4).
func Merge(t *Triage, a *Audit, ref *Refusals) {
	if a == nil {
		return
	}
	byID := map[string]*Item{}
	for i := range t.Items {
		byID[t.Items[i].ID] = &t.Items[i]
	}
	for _, p := range a.Promotions {
		it, ok := byID[p.Item]
		if !ok {
			ref.add("stack review promotes item %q, which the triage does not have", p.Item)
			continue
		}
		if p.Tier < it.Tier {
			ref.add("stack review would lower item %q from %s to %s: it can only promote (I4)", p.Item, it.Tier, p.Tier)
			continue
		}
		if p.Tier > it.Tier {
			it.Tier = p.Tier
			it.RaisedBy = "stack-review: " + p.Note
			if it.Reason == "" {
				it.Reason = p.Reason
			}
			if it.Title == "" {
				it.Title = p.Title
			}
			if it.Context == "" {
				it.Context = p.Context
			}
			if it.Question == "" {
				it.Question = p.Question
			}
			if len(it.Options) == 0 {
				it.Options = p.Options
			}
		}
	}
	for _, it := range a.Items {
		if it.ProposedBy == "" {
			it.ProposedBy = "stack-review"
		}
		if it.TieredBy == "" {
			it.TieredBy = "stack-review"
		}
		t.Items = append(t.Items, it)
	}
	t.Reads = append(t.Reads, a.Reads...)
}

// lineTiers is the tier of each live line: the highest tier of any item
// covering it, so a line counts once (I1).
func lineTiers(d *LayerData, items []Item) map[*CLine]Tier {
	tiers := map[*CLine]Tier{}
	for _, it := range items {
		if it.Layer != d.PR {
			continue
		}
		for _, r := range it.Ranges {
			f := d.File(r.Path, r.side())
			if f == nil {
				continue
			}
			for _, cl := range f.Lines {
				if cl.Class == ClassLive && r.covers(cl.Loc) && it.Tier > tiers[cl] {
					tiers[cl] = it.Tier
				}
			}
		}
	}
	return tiers
}

// Validate runs every cross-item check. It returns the untriaged live
// lines separately: they are shown, not just refused (I1).
func (st *Stack) Validate(t *Triage, surface *Surface, scouts []Scout, weak map[int][]Weakening) (untriaged map[int][]*CLine, ref Refusals) {
	ids := map[string]bool{}
	for i := range t.Items {
		it := &t.Items[i]
		if ids[it.ID] {
			ref.add("item id %q is used twice", it.ID)
		}
		ids[it.ID] = true
		st.checkItem(it, &ref)
	}
	for _, n := range t.NotInCode {
		if strings.TrimSpace(n.Text) == "" || n.Link == "" {
			ref.add("a not_in_code point needs text and a link to where it was raised")
		}
	}
	// Board items: carried, and never below the board's own call.
	if surface != nil {
		for _, s := range surface.Items {
			var carried *Item
			for i := range t.Items {
				if t.Items[i].Surfaced == s.ID {
					carried = &t.Items[i]
				}
			}
			switch {
			case carried == nil:
				ref.add("board item %q (%s: %q) has no card", s.ID, s.Agent, s.Claim)
			case carried.Tier < s.Tier:
				ref.add("item %q carries board item %q at %s, below the board's call of %s", carried.ID, s.ID, carried.Tier, s.Tier)
			}
		}
	}
	// Weakenings: every one the program found has a Scrutinize card.
	for pr, ws := range weak {
		for _, w := range ws {
			ok := false
			for _, it := range t.Items {
				if it.Layer != pr {
					continue
				}
				for _, id := range it.Weakenings {
					if id == w.ID && it.Tier == TierScrutinize {
						ok = true
					}
				}
			}
			if !ok {
				ref.add("layer #%d: weakening %s (%s) has no Scrutinize card", pr, w.ID, w.Detail)
			}
		}
	}
	known := map[string]bool{}
	for _, ws := range weak {
		for _, w := range ws {
			known[w.ID] = true
		}
	}
	for _, it := range t.Items {
		for _, id := range it.Weakenings {
			if !known[id] {
				ref.add("item %q cites weakening %q, which the program did not find", it.ID, id)
			}
		}
	}
	untriaged = map[int][]*CLine{}
	for _, d := range st.Layers {
		tiers := lineTiers(d, t.Items)
		for _, cl := range d.Lines() {
			if cl.Class != ClassLive {
				continue
			}
			tier, ok := tiers[cl]
			if !ok {
				untriaged[d.PR] = append(untriaged[d.PR], cl)
				continue
			}
			if tier < cl.Floor {
				ref.add("layer #%d: %s:%d (%s) is %s, below its floor of %s (%s)", d.PR, cl.Path, cl.Line, cl.Side, tier, cl.Floor, strings.Join(cl.Flags, ", "))
			}
		}
		// Scouts: their reported reads cover every live line of the layer.
		var reads []RangeRef
		scouted := false
		for _, s := range scouts {
			if s.Layer == d.PR {
				scouted = true
				reads = append(reads, s.Reads...)
			}
		}
		if d.LiveLines() > 0 {
			if !scouted {
				ref.add("layer #%d: no scout answer", d.PR)
			} else if miss := uncovered(d, reads); len(miss) > 0 {
				ref.add("layer #%d: the scouts' reported reads miss %d live lines, first %s:%d (%s)", d.PR, len(miss), miss[0].Path, miss[0].Line, miss[0].Side)
			}
		}
	}
	return untriaged, ref
}

func uncovered(d *LayerData, reads []RangeRef) []*CLine {
	var out []*CLine
	for _, cl := range d.Lines() {
		if cl.Class != ClassLive {
			continue
		}
		hit := false
		for _, r := range reads {
			if r.covers(cl.Loc) {
				hit = true
				break
			}
		}
		if !hit {
			out = append(out, cl)
		}
	}
	return out
}
