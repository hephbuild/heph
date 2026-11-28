package main

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"
)

// The page data format. A fix-round agent reads these files, not the page;
// FORMAT.md beside the skill documents them.
const pageFormat = "heph-review-brief/2"

// Sums are a layer's changed lines per tier. The tiers sum to Total, which
// equals `git diff --numstat` for the layer's range (I1).
type Sums struct {
	Total          int `json:"total"`
	Scrutinize     int `json:"scrutinize"`
	Glance         int `json:"glance"`
	AICleared      int `json:"ai_cleared"`
	AIClearedDeep  int `json:"ai_cleared_read_by_decider_or_stack_review"`
	AIClearedScout int `json:"ai_cleared_read_by_scout_only"`
	Counted        int `json:"counted"`
	Untriaged      int `json:"untriaged"`
	// Settled is live lines only items the user settled in an earlier
	// round cover, with their code unchanged: out of the tiers, still counted.
	Settled int `json:"settled"`
}

type PageCommit struct {
	SHA     string `json:"sha"`
	Short   string `json:"short"`
	Subject string `json:"subject"`
	Link    string `json:"link"`
}

type PageRange struct {
	RangeRef
	Hash    string       `json:"hash"`    // content hash: review state follows it (I8)
	Context string       `json:"context"` // hash of the surrounding lines
	Anchor  string       `json:"anchor"`  // path#symbol: a card's identity across rounds
	Link    string       `json:"link"`
	Changed int          `json:"changed"`
	Commits []PageCommit `json:"commits"`
	Diff    bool         `json:"diff"` // its lines are in the layer's diffs file
}

// PageUse is a site that uses a card's code, cut from the layer's head.
type PageUse struct {
	Path   string `json:"path"`
	Start  int    `json:"start"`
	End    int    `json:"end"`
	Note   string `json:"note"`
	Symbol string `json:"symbol,omitempty"` // the function or type it is in
	Link   string `json:"link"`             // the lines on GitHub at the pinned head
	Key    string `json:"key"`              // its snippet in the diffs file
}

type PageTest struct {
	Name string `json:"name"`
	Link string `json:"link"`
}

type PageOther struct {
	PR     int    `json:"pr"`
	Symbol string `json:"symbol"`
	Link   string `json:"link"`
}

type Provenance struct {
	ProposedBy string `json:"proposed_by"`
	TieredBy   string `json:"tiered_by"`
	RaisedBy   string `json:"raised_by,omitempty"`
}

type PageCard struct {
	ID          string      `json:"id"`
	Tier        Tier        `json:"tier"`
	Reason      string      `json:"reason,omitempty"`
	ReasonTitle string      `json:"reason_title,omitempty"`
	Title       string      `json:"title,omitempty"`
	Context     string      `json:"context,omitempty"`
	Claim       string      `json:"claim"`
	Question    string      `json:"question,omitempty"`
	Options     []Option    `json:"options,omitempty"`
	Diagram     string      `json:"diagram,omitempty"`
	Examples    []Example   `json:"examples,omitempty"`
	Display     string      `json:"display"` // embed, links or file
	Changed     int         `json:"changed"`
	Ranges      []PageRange `json:"ranges"`
	UsedBy      []PageUse   `json:"used_by,omitempty"`
	StartFrom   []string    `json:"start_from,omitempty"`
	Tests       []PageTest  `json:"tests,omitempty"`
	Board       []BoardNote `json:"board,omitempty"`
	Weakenings  []string    `json:"weakenings,omitempty"`
	Provenance  Provenance  `json:"provenance"`
	OtherLayers []PageOther `json:"other_layers,omitempty"`
	Floors      []string    `json:"floors,omitempty"` // why a line under it could not go lower
	// EarlierRound is the user's answer on an earlier round's page about
	// this code, from the decisions file.
	EarlierRound *PageEarlier `json:"earlier_round,omitempty"`
}

type PageCounted struct {
	Path    string   `json:"path"`
	Class   Class    `json:"class"`
	Added   int      `json:"added"`
	Removed int      `json:"removed"`
	Link    string   `json:"link"`
	Moved   []string `json:"moved,omitempty"` // the other end of moved lines
	Note    string   `json:"note,omitempty"`
}

type PageLayer struct {
	Format       string        `json:"format"`
	PR           int           `json:"pr"`
	Sums         Sums          `json:"sums"`
	Live         int           `json:"live"`
	Budget       Budget        `json:"budget"`
	Used         Budget        `json:"used"`
	Overflow     bool          `json:"overflow"`
	Confirmation string        `json:"confirmation,omitempty"`
	Cards        []PageCard    `json:"cards"`
	Settled      []PageCard    `json:"settled"` // settled in an earlier round, code unchanged
	Untriaged    []PageRange   `json:"untriaged"`
	Counted      []PageCounted `json:"counted"`
	Weakenings   []Weakening   `json:"weakenings"`
	Fingerprint  string        `json:"fingerprint"`
}

// DiffLine is [kind, old line, new line, text]; kind "@" marks a gap.
type DiffLine [4]any

type PageDiffs struct {
	Format   string                `json:"format"`
	PR       int                   `json:"pr"`
	Ranges   map[string][]DiffLine `json:"ranges"`
	Snippets map[string][]DiffLine `json:"snippets"` // PageUse key to its lines
}

type PageLayerHead struct {
	Layer
	File      string `json:"file"`
	DiffsFile string `json:"diffs_file"`
	Sums      Sums   `json:"sums"`
	Cards     int    `json:"scrutinize_cards"`
}

type PageDecision struct {
	Layer int    `json:"layer"`
	Card  string `json:"card"`
	Text  string `json:"text"`
}

type PageAssessment struct {
	Text string     `json:"text"`
	Refs []LayerRef `json:"refs"`
}

type PageStack struct {
	Format       string          `json:"format"`
	Repo         string          `json:"repo"`
	Trunk        string          `json:"trunk"`
	Mode         Mode            `json:"mode"`
	StackKey     string          `json:"stack_key"`
	FreshnessAPI string          `json:"freshness_api"`
	Layers       []PageLayerHead `json:"layers"`
	Assessment   *PageAssessment `json:"assessment,omitempty"`
	Decisions    []PageDecision  `json:"decisions"`
	NotInCode    []NotInCode     `json:"not_in_code"`
	OpenCards    int             `json:"open_cards"`
}

type Manifest struct {
	Format string            `json:"format"`
	Files  map[string]string `json:"files"` // name -> sha256
	Sums   map[string]Sums   `json:"sums"`  // layer PR -> sums
}

// embedLimit is the most changed lines a card embeds; a larger one lists
// links to read in order (O4's first number).
const embedLimit = 80

// RenderInput is everything render reads.
type RenderInput struct {
	Triage         *Triage
	Audit          *Audit
	Surface        *Surface
	Scouts         []Scout
	SpecTests      []string
	AllowUntriaged bool
	// Decisions are the user's answers from earlier rounds' pages.
	Decisions *DecisionsFile
	// FreshnessAPI is where the page asks for each PR's head ("" is
	// GitHub's API); tests point it at a local server.
	FreshnessAPI string
}

// ErrUntriaged is returned (after writing) when live lines had no item and
// the caller asked to publish them as Untriaged anyway.
type ErrUntriaged struct{ Lines int }

func (e ErrUntriaged) Error() string {
	return fmt.Sprintf("%d live lines are untriaged; they are on the page as raw diff", e.Lines)
}

// Render checks the triage against the stack and writes the page data.
// Nothing reaches outDir unless every check passes: the files are written
// to a temp directory beside it and swapped in.
func (st *Stack) Render(in RenderInput, pagePath, outDir string) error {
	t := in.Triage
	var ref Refusals
	if t.Format != triageFormat {
		ref.add("triage format %q, want %q", t.Format, triageFormat)
	}
	for i := range t.Items {
		if t.Items[i].TieredBy == "" {
			t.Items[i].TieredBy = "decider"
		}
	}
	if in.Audit != nil {
		if in.Audit.Format != auditFormat {
			ref.add("audit format %q, want %q", in.Audit.Format, auditFormat)
		}
		if strings.TrimSpace(in.Audit.Assessment.Text) == "" || len(in.Audit.Assessment.Refs) == 0 {
			ref.add("the stack review's assessment needs text and the code it cites (I12)")
		}
		for _, r := range in.Audit.Assessment.Refs {
			if d := st.layer(r.Layer); d == nil {
				ref.add("assessment cites layer #%d, which is not in the stack", r.Layer)
			} else {
				st.checkRange(d, r.RangeRef, "assessment", &ref)
			}
		}
		for _, c := range in.Audit.Confirmations {
			if st.layer(c.Layer) == nil {
				ref.add("stack review confirms layer #%d, which is not in the stack", c.Layer)
			}
		}
	}
	if in.Surface != nil && in.Surface.Format != surfaceFormat {
		ref.add("surface format %q, want %q", in.Surface.Format, surfaceFormat)
	}
	for _, s := range in.Scouts {
		if s.Format != scoutFormat {
			ref.add("scout %s: format %q, want %q", s.Agent, s.Format, scoutFormat)
		}
	}
	if in.Decisions != nil {
		ref = append(ref, in.Decisions.check(stackKeyOf(st.Pin))...)
	}
	Merge(t, in.Audit, &ref)

	weak := map[int][]Weakening{}
	for _, d := range st.Layers {
		weak[d.PR] = st.Weakenings(d)
	}
	if missing := st.MissingSpecTests(in.SpecTests); len(missing) > 0 {
		top := st.Layers[len(st.Layers)-1].PR
		weak[top] = append(weak[top], missing...)
	}
	untriaged, more := st.Validate(t, in.Surface, in.Scouts, weak)
	ref = append(ref, more...)
	for _, d := range st.Layers {
		n := len(d.Lines())
		if n != d.Numstat {
			ref.add("layer #%d: the classifier saw %d changed lines and git --numstat reports %d", d.PR, n, d.Numstat)
		}
	}
	nUntriaged := 0
	for _, u := range untriaged {
		nUntriaged += len(u)
	}
	if len(ref) > 0 {
		return ref
	}
	if nUntriaged > 0 && !in.AllowUntriaged {
		var r Refusals
		for _, d := range st.Layers {
			for _, rr := range collapse(untriaged[d.PR]) {
				r.add("layer #%d: untriaged live lines %s", d.PR, rr)
			}
		}
		return r
	}

	page, err := os.ReadFile(pagePath)
	if err != nil {
		return fmt.Errorf("reading the page: %w", err)
	}
	files := map[string][]byte{}
	ps := &PageStack{
		Format:       pageFormat,
		Repo:         st.Pin.Repo,
		Trunk:        st.Pin.Trunk,
		Mode:         st.Pin.Mode,
		StackKey:     stackKeyOf(st.Pin),
		FreshnessAPI: "https://api.github.com/repos/" + st.Pin.Repo + "/pulls/",
		Decisions:    []PageDecision{},
		NotInCode:    append([]NotInCode{}, t.NotInCode...),
	}
	if in.FreshnessAPI != "" {
		ps.FreshnessAPI = in.FreshnessAPI
	}
	if in.Audit != nil {
		ps.Assessment = &PageAssessment{Text: in.Audit.Assessment.Text, Refs: in.Audit.Assessment.Refs}
	}
	manifest := Manifest{Format: pageFormat, Files: map[string]string{}, Sums: map[string]Sums{}}
	symbols := st.symbolIndex()
	dc := st.newDecisionContext(in.Decisions)
	for _, d := range st.Layers {
		pl, diffs, err := st.renderLayer(d, t, in.Audit, untriaged[d.PR], weak[d.PR], symbols, dc)
		if err != nil {
			return fmt.Errorf("layer #%d: %w", d.PR, err)
		}
		name := fmt.Sprintf("layer-%d.json", d.PR)
		dname := fmt.Sprintf("layer-%d-diffs.json", d.PR)
		if files[name], err = json.Marshal(pl); err != nil {
			return err
		}
		if files[dname], err = json.Marshal(diffs); err != nil {
			return err
		}
		cards := 0
		for _, c := range pl.Cards {
			if c.Tier == TierScrutinize {
				cards++
				ps.OpenCards++
			}
			open := false
			for _, b := range c.Board {
				open = open || b.Status == "open" || b.Status == "overruled"
			}
			if c.Reason == "decision" || open {
				text := c.Title
				if text == "" {
					text = c.Claim
				}
				ps.Decisions = append(ps.Decisions, PageDecision{Layer: d.PR, Card: c.ID, Text: text})
			}
		}
		ps.Layers = append(ps.Layers, PageLayerHead{Layer: d.Layer, File: name, DiffsFile: dname, Sums: pl.Sums, Cards: cards})
		manifest.Sums[fmt.Sprint(d.PR)] = pl.Sums
	}
	if files["stack.json"], err = json.Marshal(ps); err != nil {
		return err
	}
	for name, b := range files {
		sum := sha256.Sum256(b)
		manifest.Files[name] = hex.EncodeToString(sum[:])
	}
	if files["manifest.json"], err = json.MarshalIndent(manifest, "", "  "); err != nil {
		return err
	}
	// The title names the artifact in the gallery: one per stack, not one
	// "Review brief" for all of them.
	title := fmt.Sprintf("<title>Review brief #%d</title>", st.Layers[0].PR)
	files["index.html"] = []byte(strings.Replace(string(page), "<title>Review brief</title>", title, 1))
	if err := writeAtomically(outDir, files); err != nil {
		return err
	}
	if nUntriaged > 0 {
		return ErrUntriaged{Lines: nUntriaged}
	}
	return nil
}

// writeAtomically writes every file into a temp directory beside outDir,
// then swaps it in, so a failed render leaves the previous brief whole.
func writeAtomically(outDir string, files map[string][]byte) error {
	parent := filepath.Dir(outDir)
	if err := os.MkdirAll(parent, 0o755); err != nil {
		return err
	}
	tmp, err := os.MkdirTemp(parent, ".brief-tmp-*")
	if err != nil {
		return err
	}
	ok := false
	defer func() {
		if !ok {
			os.RemoveAll(tmp)
		}
	}()
	for name, b := range files {
		if err := os.WriteFile(filepath.Join(tmp, name), b, 0o644); err != nil {
			return err
		}
	}
	if err := os.Chmod(tmp, 0o755); err != nil {
		return err
	}
	old := ""
	if _, err := os.Stat(outDir); err == nil {
		old = outDir + ".old"
		os.RemoveAll(old)
		if err := os.Rename(outDir, old); err != nil {
			return err
		}
	}
	if err := os.Rename(tmp, outDir); err != nil {
		if old != "" {
			os.Rename(old, outDir)
		}
		return err
	}
	ok = true
	if old != "" {
		os.RemoveAll(old)
	}
	return nil
}

// symbolIndex maps, per layer, each file to the symbols its live lines
// change, so a card can name the other layers that touch the same code.
func (st *Stack) symbolIndex() map[int]map[string]map[string]bool {
	out := map[int]map[string]map[string]bool{}
	for _, d := range st.Layers {
		m := map[string]map[string]bool{}
		for _, cl := range d.Lines() {
			if cl.Class != ClassLive || cl.Side != SideNew {
				continue
			}
			s := st.Symbol(d, cl.Loc)
			if s == "" {
				continue
			}
			if m[cl.Path] == nil {
				m[cl.Path] = map[string]bool{}
			}
			m[cl.Path][s] = true
		}
		out[d.PR] = m
	}
	return out
}

func (st *Stack) filesLink(d *LayerData, p string) string {
	sum := sha256.Sum256([]byte(p))
	return fmt.Sprintf("https://github.com/%s/pull/%d/files/%s..%s#diff-%s", st.Pin.Repo, d.PR, d.Base, d.Head, hex.EncodeToString(sum[:]))
}

func (st *Stack) rangeLink(d *LayerData, r RangeRef) string {
	c := "R"
	if r.side() == SideOld {
		c = "L"
	}
	return fmt.Sprintf("%s%s%d-%s%d", st.filesLink(d, r.Path), c, r.Start, c, r.End)
}

// slice cuts a range's lines out of the layer's diff: the changed lines
// in it, their context, and a gap marker where the range spans lines the
// diff did not print.
func slice(d *LayerData, r RangeRef) []DiffLine {
	f := d.File(r.Path, r.side())
	if f == nil {
		return nil
	}
	var out []DiffLine
	last := -1
	for _, h := range f.FC.Hunks {
		for _, hl := range h.Lines {
			pos, in := 0, false
			switch {
			case r.side() == SideNew && hl.Kind != '-':
				pos = hl.New
			case r.side() == SideNew:
				pos = hl.At
			case hl.Kind != '+':
				pos = hl.Old
			default:
				pos = hl.At
			}
			in = r.Start <= pos && pos <= r.End
			if hl.Kind == '-' && r.side() == SideNew && pos == r.End+1 {
				in = false
			}
			if !in {
				continue
			}
			if last >= 0 && pos > last+1 {
				out = append(out, DiffLine{"@", 0, 0, ""})
			}
			if hl.Kind != '-' || r.side() == SideOld {
				last = pos
			}
			out = append(out, DiffLine{string(hl.Kind), hl.Old, hl.New, hl.Text})
		}
	}
	return out
}

func changedIn(lines []DiffLine) int {
	n := 0
	for _, l := range lines {
		if k := l[0].(string); k == "+" || k == "-" {
			n++
		}
	}
	return n
}

// rangeHash is the content hash review state follows (I8): the path, the
// added and removed lines with trailing whitespace and CR trimmed. SHAs,
// line numbers, context and commit order are left out.
func rangeHash(path string, lines []DiffLine) string {
	h := sha256.New()
	h.Write([]byte(path))
	h.Write([]byte{0})
	for _, l := range lines {
		k := l[0].(string)
		if k != "+" && k != "-" {
			continue
		}
		h.Write([]byte(k))
		h.Write([]byte(strings.TrimRight(l[3].(string), " \t\r")))
		h.Write([]byte{'\n'})
	}
	return hex.EncodeToString(h.Sum(nil))[:20]
}

func contextHash(lines []string, start, end int) string {
	h := sha256.New()
	for _, i := range []int{start - 3, start - 2, start - 1, end + 1, end + 2, end + 3} {
		if i >= 1 && i <= len(lines) {
			h.Write([]byte(strings.TrimSpace(lines[i-1])))
		}
		h.Write([]byte{'\n'})
	}
	return hex.EncodeToString(h.Sum(nil))[:12]
}

func (st *Stack) renderLayer(d *LayerData, t *Triage, a *Audit, untriaged []*CLine, weak []Weakening, symbols map[int]map[string]map[string]bool, dc *decisionContext) (*PageLayer, *PageDiffs, error) {
	pl := &PageLayer{Format: pageFormat, PR: d.PR, Live: d.LiveLines(), Weakenings: weak, Fingerprint: d.Fingerprint(),
		Cards: []PageCard{}, Settled: []PageCard{}, Untriaged: []PageRange{}, Counted: []PageCounted{}}
	if pl.Weakenings == nil {
		pl.Weakenings = []Weakening{}
	}
	diffs := &PageDiffs{Format: pageFormat, PR: d.PR, Ranges: map[string][]DiffLine{}, Snippets: map[string][]DiffLine{}}
	pl.Budget = BudgetFor(pl.Live)

	// Ordinals for identical blocks, in path/line order so a rebase keeps them.
	type rkey struct {
		path       string
		side       Side
		start, end int
	}
	var all []rkey
	seen := map[rkey]bool{}
	for _, it := range t.Items {
		if it.Layer != d.PR {
			continue
		}
		for _, r := range it.Ranges {
			k := rkey{r.Path, r.side(), r.Start, r.End}
			if !seen[k] {
				seen[k] = true
				all = append(all, k)
			}
		}
	}
	sort.Slice(all, func(i, j int) bool {
		if all[i].path != all[j].path {
			return all[i].path < all[j].path
		}
		if all[i].side != all[j].side {
			return all[i].side < all[j].side
		}
		return all[i].start < all[j].start
	})
	hashOf := map[rkey]string{}
	count := map[string]int{}
	for _, k := range all {
		h := rangeHash(k.path, slice(d, RangeRef{Path: k.path, Side: k.side, Start: k.start, End: k.end}))
		if n := count[h]; n > 0 {
			hashOf[k] = fmt.Sprintf("%s-%d", h, n)
		} else {
			hashOf[k] = h
		}
		count[h]++
	}

	allSHAs := map[string]bool{}
	pageRange := func(r RangeRef, embed bool) (PageRange, error) {
		lines := slice(d, r)
		rev := d.Head
		if r.side() == SideOld {
			rev = d.Base
		}
		src := st.git.Lines(rev, r.Path)
		k := rkey{r.Path, r.side(), r.Start, r.End}
		h := hashOf[k]
		if h == "" {
			h = rangeHash(r.Path, lines)
		}
		pr := PageRange{RangeRef: r, Hash: h, Context: contextHash(src, r.Start, r.End),
			Link: st.rangeLink(d, r), Changed: changedIn(lines)}
		pr.Side = r.side()
		var add, rem []int
		for _, l := range lines {
			switch l[0].(string) {
			case "+":
				add = append(add, l[2].(int))
			case "-":
				rem = append(rem, l[1].(int))
			}
			if pr.Anchor == "" && (l[0] == "+" || l[0] == "-") {
				line := l[2].(int)
				if l[0] == "-" {
					line = l[1].(int)
				}
				pr.Anchor = r.Path + "#" + enclosingSymbol(src, line)
			}
		}
		var shas []string
		if r.side() == SideNew {
			s, err := st.git.CommitsAdding(d.Base, d.Head, r.Path, add)
			if err != nil {
				return pr, err
			}
			shas = s
		} else {
			s, err := st.git.CommitsRemoving(d.Base, d.Head, r.Path, rem)
			if err != nil {
				return pr, err
			}
			shas = s
		}
		for _, s := range shas {
			allSHAs[s] = true
			pr.Commits = append(pr.Commits, PageCommit{SHA: s, Short: short(s),
				Link: fmt.Sprintf("https://github.com/%s/pull/%d/commits/%s", st.Pin.Repo, d.PR, s)})
		}
		if embed {
			pr.Diff = true
			diffs.Ranges[h] = lines
		}
		return pr, nil
	}

	tiers := lineTiers(d, t.Items)
	var live, settled []Item // the items still in the tiers, and those the user settled earlier
	for _, it := range t.Items {
		if it.Layer != d.PR {
			continue
		}
		changed := 0
		for _, r := range it.Ranges {
			changed += changedIn(slice(d, r))
		}
		display := "embed"
		if changed > embedLimit {
			display = "links"
			if st.wholeFile(d, it.Ranges) {
				display = "file"
			}
		}
		title, _ := reasonTitle(it.Reason)
		c := PageCard{ID: it.ID, Tier: it.Tier, Reason: it.Reason, ReasonTitle: title, Title: it.Title, Context: it.Context,
			Claim: it.Claim, Question: it.Question, Options: it.Options, Diagram: it.Diagram, Examples: it.Examples,
			Display: display, Changed: changed, StartFrom: it.StartFrom,
			Board: it.Board, Weakenings: it.Weakenings,
			Provenance: Provenance{ProposedBy: it.ProposedBy, TieredBy: it.TieredBy, RaisedBy: it.RaisedBy}}
		for _, u := range it.UsedBy {
			src := st.git.Lines(d.Head, u.Path)
			lines := snippet(src, u)
			key := snippetKey(u.Path, lines)
			diffs.Snippets[key] = lines
			c.UsedBy = append(c.UsedBy, PageUse{Path: u.Path, Start: u.Start, End: u.End, Note: u.Note,
				Symbol: enclosingSymbol(src, u.Start), Key: key,
				Link: fmt.Sprintf("https://github.com/%s/blob/%s/%s#L%d-L%d", st.Pin.Repo, d.Head, u.Path, u.Start, u.End)})
		}
		floors := map[string]bool{}
		others := map[PageOther]bool{}
		for _, r := range it.Ranges {
			pr, err := pageRange(r, display == "embed")
			if err != nil {
				return nil, nil, err
			}
			c.Ranges = append(c.Ranges, pr)
			f := d.File(r.Path, r.side())
			for _, cl := range f.Lines {
				if r.covers(cl.Loc) && cl.Floor > TierNone && tiers[cl] == it.Tier && cl.Floor == it.Tier {
					for _, fl := range cl.Flags {
						floors[fl] = true
					}
				}
			}
			sym := strings.TrimPrefix(pr.Anchor, r.Path+"#")
			if sym == "" {
				continue
			}
			for _, od := range st.Layers {
				if od != d && symbols[od.PR][r.Path][sym] {
					others[PageOther{PR: od.PR, Symbol: sym, Link: st.filesLink(od, r.Path)}] = true
				}
			}
		}
		for fl := range floors {
			c.Floors = append(c.Floors, fl)
		}
		sort.Strings(c.Floors)
		for o := range others {
			c.OtherLayers = append(c.OtherLayers, o)
		}
		sort.Slice(c.OtherLayers, func(i, j int) bool { return c.OtherLayers[i].PR < c.OtherLayers[j].PR })
		for _, tr := range it.Tests {
			c.Tests = append(c.Tests, PageTest{Name: tr.Name, Link: st.testLink(d, tr)})
		}
		var hashes, anchors []string
		for _, r := range c.Ranges {
			hashes, anchors = append(hashes, r.Hash), append(anchors, r.Anchor)
		}
		moved, earlier := dc.forItem(hashes, anchors)
		c.EarlierRound = earlier
		if moved {
			// Settled earlier, code unchanged: not asked again, and off the
			// card budget, but its lines still count (I1).
			settled = append(settled, it)
			pl.Settled = append(pl.Settled, c)
			continue
		}
		live = append(live, it)
		if c.Tier == TierScrutinize {
			pl.Used.Cards++
			pl.Used.Lines += changed
		}
		pl.Cards = append(pl.Cards, c)
	}
	sort.SliceStable(pl.Cards, func(i, j int) bool { return pl.Cards[i].Tier > pl.Cards[j].Tier })
	// A line counts at the highest tier of the items still in the tiers;
	// one only settled items cover counts as settled.
	tiers = lineTiers(d, live)
	settledLines := lineTiers(d, settled)
	pl.Overflow = pl.Used.Cards > pl.Budget.Cards || pl.Used.Lines > pl.Budget.Lines

	for _, rr := range collapse(untriaged) {
		pr, err := pageRange(rr, true)
		if err != nil {
			return nil, nil, err
		}
		pl.Untriaged = append(pl.Untriaged, pr)
	}

	// Sums: every changed line once, at its highest tier.
	deep := readersCover(d, t.Reads, "decider", "stack-review")
	for _, cl := range d.Lines() {
		pl.Sums.Total++
		if cl.Class != ClassLive {
			pl.Sums.Counted++
			continue
		}
		tier, ok := tiers[cl]
		if _, s := settledLines[cl]; !ok && s {
			pl.Sums.Settled++
			continue
		}
		switch tier {
		case TierScrutinize:
			pl.Sums.Scrutinize++
		case TierGlance:
			pl.Sums.Glance++
		case TierAICleared:
			pl.Sums.AICleared++
			if deep[cl] {
				pl.Sums.AIClearedDeep++
			} else {
				pl.Sums.AIClearedScout++
			}
		default:
			pl.Sums.Untriaged++
		}
	}
	if a != nil {
		for _, c := range a.Confirmations {
			if c.Layer == d.PR {
				pl.Confirmation = fmt.Sprintf("AI-approved: %d lines, by triage and stack review. %s", pl.Sums.AICleared, c.Note)
			}
		}
	}
	if pl.Counted = st.counted(d); pl.Counted == nil {
		pl.Counted = []PageCounted{} // the format has lists, never null
	}

	subj, err := st.git.Subjects(keys(allSHAs))
	if err != nil {
		return nil, nil, err
	}
	fill := func(rs []PageRange) {
		for i := range rs {
			for j := range rs[i].Commits {
				rs[i].Commits[j].Subject = subj[rs[i].Commits[j].SHA]
			}
		}
	}
	for i := range pl.Cards {
		fill(pl.Cards[i].Ranges)
	}
	for i := range pl.Settled {
		fill(pl.Settled[i].Ranges)
	}
	fill(pl.Untriaged)
	return pl, diffs, nil
}

func keys(m map[string]bool) []string {
	out := make([]string, 0, len(m))
	for k := range m {
		out = append(out, k)
	}
	sort.Strings(out)
	return out
}

// wholeFile reports a card that covers a file the layer adds or rewrites
// from its first line to its last.
func (st *Stack) wholeFile(d *LayerData, rs []RangeRef) bool {
	for _, r := range rs {
		f := d.File(r.Path, r.side())
		if f == nil || r.side() != SideNew {
			continue
		}
		n := len(st.git.Lines(d.Head, r.Path))
		added := 0
		for _, cl := range f.Lines {
			if cl.Side == SideNew {
				added++
			}
		}
		if r.Start <= 1 && r.End >= n && added == n {
			return true
		}
	}
	return false
}

func (st *Stack) testLink(d *LayerData, t TestRef) string {
	p := t.Path
	if p == "" {
		return fmt.Sprintf("https://github.com/%s/search?q=%s&type=code", st.Pin.Repo, t.Name)
	}
	return fmt.Sprintf("https://github.com/%s/blob/%s/%s", st.Pin.Repo, d.Head, p)
}

// readersCover marks the live lines the named agents report reading.
func readersCover(d *LayerData, reads []Read, agents ...string) map[*CLine]bool {
	out := map[*CLine]bool{}
	for _, r := range reads {
		ok := false
		for _, a := range agents {
			ok = ok || r.Agent == a
		}
		if !ok || (r.Layer != 0 && r.Layer != d.PR) {
			continue
		}
		for _, rr := range r.Ranges {
			f := d.File(rr.Path, rr.side())
			if f == nil {
				continue
			}
			for _, cl := range f.Lines {
				if rr.covers(cl.Loc) {
					out[cl] = true
				}
			}
		}
	}
	return out
}

// collapse turns lines into ranges of consecutive lines per file and side.
func collapse(lines []*CLine) []RangeRef {
	sorted := append([]*CLine{}, lines...)
	sort.Slice(sorted, func(i, j int) bool {
		a, b := sorted[i], sorted[j]
		if a.Path != b.Path {
			return a.Path < b.Path
		}
		if a.Side != b.Side {
			return a.Side < b.Side
		}
		return a.Line < b.Line
	})
	var out []RangeRef
	for _, cl := range sorted {
		if n := len(out); n > 0 && out[n-1].Path == cl.Path && out[n-1].Side == cl.Side && out[n-1].End+1 == cl.Line {
			out[n-1].End = cl.Line
			continue
		}
		out = append(out, RangeRef{Path: cl.Path, Side: cl.Side, Start: cl.Line, End: cl.Line})
	}
	return out
}

// counted lists every file's Counted lines by class, and every file with
// no line at all (a mode flip, a binary, a pure rename), so nothing is
// dropped.
func (st *Stack) counted(d *LayerData) []PageCounted {
	var out []PageCounted
	for _, f := range d.Files {
		by := map[Class]*PageCounted{}
		var order []Class
		for _, cl := range f.Lines {
			if cl.Class == ClassLive {
				continue
			}
			pc := by[cl.Class]
			if pc == nil {
				pc = &PageCounted{Path: f.Path, Class: cl.Class, Link: st.filesLink(d, f.Path)}
				by[cl.Class] = pc
				order = append(order, cl.Class)
			}
			if cl.Side == SideNew {
				pc.Added++
			} else {
				pc.Removed++
			}
			if cl.Other != nil {
				o := fmt.Sprintf("%s:%d", cl.Other.Path, cl.Other.Line)
				if len(pc.Moved) == 0 || !strings.HasPrefix(pc.Moved[len(pc.Moved)-1], cl.Other.Path+":") {
					pc.Moved = append(pc.Moved, o)
				}
			}
		}
		if len(f.Lines) == 0 {
			class := f.FileClass
			if class == "" {
				class = ClassMode
			}
			pc := &PageCounted{Path: f.Path, Class: class, Link: st.filesLink(d, f.Path)}
			switch {
			case f.FC.OldPath != "" && f.FC.NewPath != "" && f.FC.OldPath != f.FC.NewPath:
				pc.Note = "renamed from " + f.FC.OldPath
			case f.FC.OldMode != f.FC.NewMode && f.FC.OldMode != "000000" && f.FC.NewMode != "000000":
				pc.Note = fmt.Sprintf("mode %s -> %s", f.FC.OldMode, f.FC.NewMode)
			}
			by[class] = pc
			order = append(order, class)
		}
		for _, c := range order {
			pc := by[c]
			if f.PinsMoved > 0 && c == ClassLockfile {
				pc.Note = fmt.Sprintf("%d existing pins moved", f.PinsMoved)
			}
			out = append(out, *pc)
		}
	}
	return out
}
