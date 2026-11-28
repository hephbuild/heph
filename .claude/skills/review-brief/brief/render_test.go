package main

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// basicStack: a base on origin/master and one layer of two commits, one in
// unprotected code and one in a protected path.
type basic struct {
	f         *fixture
	st        *Stack
	base      string
	c1, c2    string
	utilAdded RangeRef
	coreAdded RangeRef
}

func basicStack(t *testing.T) *basic {
	f := newFixture(t)
	f.write("src/util.rs", "pub fn a() {}\npub fn b() {}\n")
	f.write("core/engine.rs", "pub fn run() {}\n")
	base := f.commit("base")
	f.trunk(base)
	f.write("src/util.rs", "pub fn a() {}\npub fn first() {}\npub fn b() {}\n")
	c1 := f.commit("add first")
	f.write("src/util.rs", "pub fn a() {}\npub fn first() {}\npub fn b() {}\npub fn second() {}\n")
	f.write("core/engine.rs", "pub fn run() {}\npub fn step() {}\n")
	c2 := f.commit("add second and step")
	return &basic{f: f, st: f.stack(ModeWhole, c2), base: base, c1: c1, c2: c2,
		utilAdded: RangeRef{Path: "src/util.rs", Start: 2, End: 4},
		coreAdded: RangeRef{Path: "core/engine.rs", Start: 2, End: 2}}
}

func (b *basic) triage(items ...Item) (*Triage, []Scout) {
	t := &Triage{Format: triageFormat, Items: items}
	return t, []Scout{{Format: scoutFormat, Agent: "scout:1", Layer: 1, Reads: []RangeRef{b.utilAdded, b.coreAdded}}}
}

func item(id string, tier Tier, rs ...RangeRef) Item {
	it := Item{ID: id, Layer: 1, Tier: tier, Claim: "claim " + id, Ranges: rs, ProposedBy: "scout"}
	if tier == TierScrutinize {
		it.Reason, it.Question = "engine-flow", "Is this right?"
	}
	return it
}

func Test_every_changed_line_has_one_tier(t *testing.T) {
	b := basicStack(t)
	b.f.write("docs/x.md", "doc\n")
	b.f.write("x/tests/t.rs", "fn t() {}\n")
	h := b.f.commit("docs and tests")
	st := b.f.stack(ModeWhole, h)
	tr, scouts := coverAll(st)
	out := mustRender(t, st, RenderInput{Triage: tr, Scouts: scouts})
	pl := readLayer(t, out, 1)
	n, err := b.f.g.Numstat(st.Layers[0].Base, h)
	if err != nil {
		t.Fatal(err)
	}
	s := pl.Sums
	if s.Total != n || s.Scrutinize+s.Glance+s.AICleared+s.Counted+s.Untriaged != n {
		t.Fatalf("sums %+v do not add up to numstat %d", s, n)
	}
	if s.Counted != 2 || s.Glance != 1 || s.AICleared != 2 {
		t.Fatalf("sums %+v, want 2 counted, 1 glance (the protected line), 2 AI-cleared", s)
	}
	// The manifest names every data file with its checksum.
	var m Manifest
	mb, _ := os.ReadFile(filepath.Join(out, "manifest.json"))
	if err := json.Unmarshal(mb, &m); err != nil {
		t.Fatal(err)
	}
	for name, want := range m.Files {
		body, err := os.ReadFile(filepath.Join(out, name))
		if err != nil {
			t.Fatal(err)
		}
		sum := sha256.Sum256(body)
		if hex.EncodeToString(sum[:]) != want {
			t.Fatalf("%s: checksum does not match the manifest", name)
		}
	}
	if m.Sums["1"] != pl.Sums {
		t.Fatalf("manifest sums %+v, layer sums %+v", m.Sums["1"], pl.Sums)
	}
}

func Test_uncovered_live_line_is_shown_and_fails(t *testing.T) {
	b := basicStack(t)
	tr, scouts := b.triage(item("core", TierGlance, b.coreAdded))
	out, err := render(t, b.st, RenderInput{Triage: tr, Scouts: scouts})
	wantRefusal(t, err, "untriaged live lines src/util.rs:2-2 (new)")
	wantRefusal(t, err, "untriaged live lines src/util.rs:4-4 (new)")
	if _, statErr := os.Stat(out); !os.IsNotExist(statErr) {
		t.Fatal("a refused render wrote its output")
	}
	tr, scouts = b.triage(item("core", TierGlance, b.coreAdded))
	out, err = render(t, b.st, RenderInput{Triage: tr, Scouts: scouts, AllowUntriaged: true})
	var u ErrUntriaged
	if !errors.As(err, &u) || u.Lines != 2 {
		t.Fatalf("want ErrUntriaged for 2 lines, got %v", err)
	}
	pl := readLayer(t, out, 1)
	if pl.Sums.Untriaged != 2 || len(pl.Untriaged) != 2 {
		t.Fatalf("untriaged sums %d ranges %+v, want 2 lines in 2 ranges", pl.Sums.Untriaged, pl.Untriaged)
	}
	if !readDiffs(t, out, 1).has(pl.Untriaged[0].Hash) {
		t.Fatal("untriaged lines have no raw diff on the page")
	}
}

func (d *PageDiffs) has(h string) bool { _, ok := d.Ranges[h]; return ok }

func Test_card_diff_is_bytes_from_git(t *testing.T) {
	b := basicStack(t)
	tr, scouts := b.triage(item("util", TierScrutinize, b.utilAdded), item("core", TierGlance, b.coreAdded))
	out := mustRender(t, b.st, RenderInput{Triage: tr, Scouts: scouts})
	pl := readLayer(t, out, 1)
	var card PageCard
	for _, c := range pl.Cards {
		if c.ID == "util" {
			card = c
		}
	}
	got := []string{}
	for _, l := range readDiffs(t, out, 1).Ranges[card.Ranges[0].Hash] {
		if k := l[0].(string); k == "+" || k == "-" {
			got = append(got, k+l[3].(string))
		}
	}
	raw, err := b.f.g.Run("diff", "-U0", b.base, b.c2, "--", "src/util.rs")
	if err != nil {
		t.Fatal(err)
	}
	want := []string{}
	for _, l := range splitLines(string(raw)) {
		if (strings.HasPrefix(l, "+") && !strings.HasPrefix(l, "+++")) || (strings.HasPrefix(l, "-") && !strings.HasPrefix(l, "---")) {
			want = append(want, l)
		}
	}
	if strings.Join(got, "\n") != strings.Join(want, "\n") {
		t.Fatalf("card diff:\n%s\ngit diff:\n%s", strings.Join(got, "\n"), strings.Join(want, "\n"))
	}
}

func Test_card_commits_come_from_blame(t *testing.T) {
	b := basicStack(t)
	tr, scouts := b.triage(item("util", TierScrutinize, b.utilAdded), item("core", TierGlance, b.coreAdded))
	out := mustRender(t, b.st, RenderInput{Triage: tr, Scouts: scouts})
	for _, c := range readLayer(t, out, 1).Cards {
		if c.ID != "util" {
			continue
		}
		got := map[string]string{}
		for _, m := range c.Ranges[0].Commits {
			got[m.SHA] = m.Subject
		}
		if got[b.c1] != "add first" || got[b.c2] != "add second and step" || len(got) != 2 {
			t.Fatalf("commits %v, want exactly the layer's two (and not the base %s)", got, short(b.base))
		}
		return
	}
	t.Fatal("no util card")
}

func Test_deleted_only_hunk_is_covered_and_has_commit(t *testing.T) {
	f := newFixture(t)
	f.write("src/del.rs", "keep\ngone one\ngone two\nkeep too\n")
	f.write("src/r.rs", block("r", 12)+"doomed line\n")
	f.write("core/protected.rs", "pub fn p() {}\n")
	f.trunk(f.commit("base"))
	f.write("src/other.rs", "fn unrelated() {}\n")
	c1 := f.commit("unrelated")
	f.write("src/del.rs", "keep\nkeep too\n")
	c2 := f.commit("delete two lines")
	f.git("mv", "src/r.rs", "src/r2.rs")
	c3 := f.commit("rename")
	f.write("src/r2.rs", block("r", 12))
	c4 := f.commit("delete after the rename")
	f.rm("core/protected.rs")
	h := f.commit("delete a protected file")
	_ = c1
	_ = c3
	st := f.stack(ModeWhole, h)
	d := st.Layers[0]
	del := RangeRef{Path: "src/del.rs", Side: SideOld, Start: 2, End: 3}
	ren := RangeRef{Path: "src/r.rs", Side: SideOld, Start: 13, End: 13}
	prot := RangeRef{Path: "core/protected.rs", Side: SideOld, Start: 1, End: 1}
	other := RangeRef{Path: "src/other.rs", Start: 1, End: 1}
	scouts := []Scout{{Format: scoutFormat, Agent: "scout:1", Layer: 1, Reads: []RangeRef{del, ren, prot, other}}}

	// The protected deletion cannot be AI-cleared.
	tr := &Triage{Format: triageFormat, Items: []Item{item("del", TierAICleared, del), item("ren", TierAICleared, ren),
		item("prot", TierAICleared, prot), item("other", TierAICleared, other)}}
	_, err := render(t, st, RenderInput{Triage: tr, Scouts: scouts})
	wantRefusal(t, err, "core/protected.rs:1 (old) is ai-cleared, below its floor of glance")

	tr = &Triage{Format: triageFormat, Items: []Item{item("del", TierScrutinize, del), item("ren", TierAICleared, ren),
		item("prot", TierGlance, prot), item("other", TierAICleared, other)}}
	out := mustRender(t, st, RenderInput{Triage: tr, Scouts: scouts})
	pl := readLayer(t, out, 1)
	if pl.Sums.Untriaged != 0 || pl.Sums.Total != len(d.Lines()) {
		t.Fatalf("sums %+v: a deleted line was not counted", pl.Sums)
	}
	commits := map[string][]string{}
	for _, c := range pl.Cards {
		for _, m := range c.Ranges[0].Commits {
			commits[c.ID] = append(commits[c.ID], m.SHA)
		}
	}
	if fmt.Sprint(commits["del"]) != fmt.Sprint([]string{c2}) {
		t.Errorf("deleting commit %v, want [%s]", commits["del"], short(c2))
	}
	if fmt.Sprint(commits["ren"]) != fmt.Sprint([]string{c4}) {
		t.Errorf("deletion after a rename: commit %v, want [%s]", commits["ren"], short(c4))
	}
	if len(commits["prot"]) != 1 || commits["prot"][0] != h {
		t.Errorf("deleted file: commit %v, want [%s]", commits["prot"], short(h))
	}
}

func Test_unknown_test_name_is_refused(t *testing.T) {
	b := basicStack(t)
	it := item("util", TierScrutinize, b.utilAdded)
	it.Tests = []TestRef{{Name: "no_such_test_anywhere"}}
	tr, scouts := b.triage(it, item("core", TierGlance, b.coreAdded))
	_, err := render(t, b.st, RenderInput{Triage: tr, Scouts: scouts})
	wantRefusal(t, err, `test "no_such_test_anywhere" does not exist`)
}

func Test_ignored_named_test_is_refused(t *testing.T) {
	b := basicStack(t)
	b.f.write("src/util.rs", "pub fn a() {}\npub fn first() {}\npub fn b() {}\npub fn second() {}\n#[test]\n#[ignore]\nfn pins_second() {}\n")
	st := b.f.stack(ModeWhole, b.f.commit("ignored test"))
	it := item("util", TierScrutinize, RangeRef{Path: "src/util.rs", Start: 2, End: 4})
	it.Tests = []TestRef{{Name: "pins_second"}}
	tr, scouts := coverAll(st)
	tr.Items = append([]Item{it}, tr.Items...)
	_, err := render(t, st, RenderInput{Triage: tr, Scouts: scouts})
	wantRefusal(t, err, `test "pins_second" is ignored`)
}

func Test_protected_line_below_glance_is_refused(t *testing.T) {
	b := basicStack(t)
	tr, scouts := b.triage(item("util", TierAICleared, b.utilAdded), item("core", TierAICleared, b.coreAdded))
	_, err := render(t, b.st, RenderInput{Triage: tr, Scouts: scouts})
	wantRefusal(t, err, "core/engine.rs:2 (new) is ai-cleared, below its floor of glance (protected)")
}

func Test_audit_cannot_demote(t *testing.T) {
	b := basicStack(t)
	tr, scouts := b.triage(item("util", TierScrutinize, b.utilAdded), item("core", TierGlance, b.coreAdded))
	audit := &Audit{Format: auditFormat, Assessment: Assessment{Text: "fine", Refs: []LayerRef{{Layer: 1, RangeRef: b.utilAdded}}},
		Promotions: []Promotion{{Item: "util", Tier: TierGlance, Note: "talk it down"}}}
	_, err := render(t, b.st, RenderInput{Triage: tr, Scouts: scouts, Audit: audit})
	wantRefusal(t, err, "would lower item \"util\" from scrutinize to glance")

	tr, scouts = b.triage(item("util", TierAICleared, b.utilAdded), item("core", TierGlance, b.coreAdded))
	audit.Promotions = []Promotion{{Item: "util", Tier: TierScrutinize, Note: "not willing to approve",
		Reason: "low-confidence", Question: "Does first() need a caller?"}}
	out := mustRender(t, b.st, RenderInput{Triage: tr, Scouts: scouts, Audit: audit})
	for _, c := range readLayer(t, out, 1).Cards {
		if c.ID == "util" && (c.Tier != TierScrutinize || !strings.Contains(c.Provenance.RaisedBy, "stack-review")) {
			t.Fatalf("promoted card: tier %s raised by %q", c.Tier, c.Provenance.RaisedBy)
		}
	}
}

func Test_board_item_not_demotable(t *testing.T) {
	b := basicStack(t)
	surface := &Surface{Format: surfaceFormat, Items: []SurfaceItem{{ID: "h1", Agent: "hermeticity", Layer: 1, Tier: TierScrutinize,
		Reason: "cache-key", Claim: "a key", Ranges: []RangeRef{b.utilAdded}}}}
	it := item("util", TierGlance, b.utilAdded)
	it.Surfaced = "h1"
	tr, scouts := b.triage(it, item("core", TierGlance, b.coreAdded))
	_, err := render(t, b.st, RenderInput{Triage: tr, Scouts: scouts, Surface: surface})
	wantRefusal(t, err, `carries board item "h1" at glance, below the board's call of scrutinize`)

	tr, scouts = b.triage(item("core", TierGlance, b.coreAdded), item("util", TierAICleared, b.utilAdded))
	_, err = render(t, b.st, RenderInput{Triage: tr, Scouts: scouts, Surface: surface})
	wantRefusal(t, err, `board item "h1" (hermeticity: "a key") has no card`)
}

func manyLines(t *testing.T, n int) (*fixture, *Stack) {
	return oneLayer(t, map[string]string{"src/many.rs": "\n"}, map[string]string{"src/many.rs": "\n" + lines(n, func(i int) string {
		return fmt.Sprintf("pub fn f%d() {}", i)
	})})
}

func Test_over_budget_shows_every_card(t *testing.T) {
	_, st := manyLines(t, 16)
	tr := &Triage{Format: triageFormat}
	var reads []RangeRef
	for i := 0; i < 16; i++ {
		r := RangeRef{Path: "src/many.rs", Start: i + 2, End: i + 2}
		tr.Items = append(tr.Items, item(itoa(i), TierScrutinize, r))
		reads = append(reads, r)
	}
	out := mustRender(t, st, RenderInput{Triage: tr, Scouts: []Scout{{Format: scoutFormat, Agent: "scout:1", Layer: 1, Reads: reads}}})
	pl := readLayer(t, out, 1)
	if len(pl.Cards) != 16 || !pl.Overflow || pl.Used.Cards != 16 || pl.Budget.Cards != 5 {
		t.Fatalf("%d cards, overflow %v, used %+v, budget %+v; want 16 cards over a budget of 5", len(pl.Cards), pl.Overflow, pl.Used, pl.Budget)
	}
}

func cardHash(t *testing.T, st *Stack, r RangeRef) string {
	tr, scouts := coverAll(st)
	tr.Items = append([]Item{item("x", TierScrutinize, r)}, tr.Items...)
	out := mustRender(t, st, RenderInput{Triage: tr, Scouts: scouts})
	for _, c := range readLayer(t, out, 1).Cards {
		if c.ID == "x" {
			return c.Ranges[0].Hash
		}
	}
	t.Fatal("no card x")
	return ""
}

func Test_rebase_keeps_card_hash(t *testing.T) {
	f := newFixture(t)
	f.write("src/a.rs", "fn a() {}\n")
	m1 := f.commit("m1")
	f.trunk(m1)
	f.write("src/a.rs", "fn a() {}\nfn change() {}\n")
	before := cardHash(t, f.stack(ModeWhole, f.commit("layer")), RangeRef{Path: "src/a.rs", Start: 2, End: 2})

	// The same change on a master that grew lines above it: SHAs and line
	// numbers move, the content does not.
	f.git("checkout", "-q", "-b", "m2", m1)
	f.write("src/a.rs", "// header\n// more\nfn a() {}\n")
	f.trunk(f.commit("m2"))
	f.write("src/a.rs", "// header\n// more\nfn a() {}\nfn change() {}\n")
	after := cardHash(t, f.stack(ModeWhole, f.commit("layer, rebased")), RangeRef{Path: "src/a.rs", Start: 4, End: 4})
	if before != after {
		t.Fatalf("card hash moved on a rebase: %s -> %s", before, after)
	}
}

func Test_edit_changes_card_hash(t *testing.T) {
	f := newFixture(t)
	f.write("src/a.rs", "fn a() {}\n")
	f.trunk(f.commit("m1"))
	f.write("src/a.rs", "fn a() {}\nfn change() {}\n")
	before := cardHash(t, f.stack(ModeWhole, f.commit("layer")), RangeRef{Path: "src/a.rs", Start: 2, End: 2})
	f.write("src/a.rs", "fn a() {}\nfn change_fixed() {}\n")
	after := cardHash(t, f.stack(ModeWhole, f.commit("fix")), RangeRef{Path: "src/a.rs", Start: 2, End: 2})
	if before == after {
		t.Fatal("card hash did not move when its code changed")
	}
}

func Test_duplicate_block_hash_distinct(t *testing.T) {
	_, st := oneLayer(t, map[string]string{"src/a.rs": "\n"}, map[string]string{"src/a.rs": "\nfn same() {}\n\nfn same() {}\n"})
	tr, scouts := coverAll(st)
	tr.Items = []Item{item("one", TierAICleared, RangeRef{Path: "src/a.rs", Start: 2, End: 2}),
		item("two", TierAICleared, RangeRef{Path: "src/a.rs", Start: 4, End: 4}),
		item("rest", TierAICleared, RangeRef{Path: "src/a.rs", Start: 1, End: 4})}
	out := mustRender(t, st, RenderInput{Triage: tr, Scouts: scouts})
	h := map[string]string{}
	for _, c := range readLayer(t, out, 1).Cards {
		h[c.ID] = c.Ranges[0].Hash
	}
	if h["one"] == h["two"] {
		t.Fatalf("two identical blocks share hash %s", h["one"])
	}
}

func Test_failed_render_writes_nothing(t *testing.T) {
	b := basicStack(t)
	out := filepath.Join(t.TempDir(), "brief")
	tr, scouts := b.triage(item("util", TierScrutinize, b.utilAdded), item("core", TierGlance, b.coreAdded))
	if err := b.st.Render(RenderInput{Triage: tr, Scouts: scouts}, pagePath(t), out); err != nil {
		t.Fatal(err)
	}
	before, _ := os.ReadFile(filepath.Join(out, "manifest.json"))
	tr, scouts = b.triage(item("util", TierScrutinize, b.utilAdded)) // the protected line is now untriaged
	tr.Items[0].Reason = "not-a-reason"
	if err := b.st.Render(RenderInput{Triage: tr, Scouts: scouts}, pagePath(t), out); err == nil {
		t.Fatal("render did not refuse")
	}
	after, _ := os.ReadFile(filepath.Join(out, "manifest.json"))
	if string(before) != string(after) {
		t.Fatal("a refused render changed the previous brief")
	}
	entries, _ := os.ReadDir(filepath.Dir(out))
	if len(entries) != 1 {
		t.Fatalf("a refused render left files beside the brief: %v", entries)
	}
}

func Test_scout_incomplete_read_is_refused(t *testing.T) {
	b := basicStack(t)
	tr, _ := b.triage(item("util", TierScrutinize, b.utilAdded), item("core", TierGlance, b.coreAdded))
	scouts := []Scout{{Format: scoutFormat, Agent: "scout:1", Layer: 1, Reads: []RangeRef{b.utilAdded}}}
	_, err := render(t, b.st, RenderInput{Triage: tr, Scouts: scouts})
	wantRefusal(t, err, "the scouts' reported reads miss 1 live lines, first core/engine.rs:2")
	_, err = render(t, b.st, RenderInput{Triage: tr})
	wantRefusal(t, err, "layer #1: no scout answer")
}

func Test_reported_read_split_in_ledger(t *testing.T) {
	b := basicStack(t)
	tr, scouts := b.triage(item("util", TierAICleared, b.utilAdded), item("core", TierGlance, b.coreAdded))
	tr.Reads = []Read{{Agent: "decider", Layer: 1, Ranges: []RangeRef{{Path: "src/util.rs", Start: 2, End: 2}}}}
	s := readLayer(t, mustRender(t, b.st, RenderInput{Triage: tr, Scouts: scouts}), 1).Sums
	if s.AICleared != 2 || s.AIClearedDeep != 1 || s.AIClearedScout != 1 {
		t.Fatalf("sums %+v, want 2 AI-cleared: 1 read by the decider, 1 by a scout only", s)
	}
}

func Test_range_outside_layer_diff_refused(t *testing.T) {
	b := basicStack(t)
	for _, r := range []RangeRef{
		{Path: "src/untouched.rs", Start: 1, End: 1},
		{Path: "src/util.rs", Start: 1, End: 1},   // a line the layer did not change
		{Path: "src/util.rs", Start: 3, End: 400}, // past the end of the file
		{Path: "src/util.rs", Side: SideOld, Start: 1, End: 1},
	} {
		tr, scouts := b.triage(item("util", TierScrutinize, b.utilAdded), item("core", TierGlance, b.coreAdded), item("bad", TierGlance, r))
		_, err := render(t, b.st, RenderInput{Triage: tr, Scouts: scouts})
		if err == nil || !strings.Contains(err.Error(), `item "bad"`) {
			t.Errorf("range %s: want a refusal of item bad, got %v", r, err)
		}
	}
}

func Test_unknown_reason_refused(t *testing.T) {
	b := basicStack(t)
	it := item("util", TierScrutinize, b.utilAdded)
	it.Reason = "vibes"
	tr, scouts := b.triage(it, item("core", TierGlance, b.coreAdded))
	_, err := render(t, b.st, RenderInput{Triage: tr, Scouts: scouts})
	wantRefusal(t, err, `reason "vibes" is not in the closed list`)
}

func Test_card_without_code_range_is_refused(t *testing.T) {
	b := basicStack(t)
	tr, scouts := b.triage(item("util", TierScrutinize, b.utilAdded), item("core", TierGlance, b.coreAdded), item("scope", TierScrutinize))
	_, err := render(t, b.st, RenderInput{Triage: tr, Scouts: scouts})
	wantRefusal(t, err, `item "scope": no code range`)
}

func Test_weakening_without_card_fails(t *testing.T) {
	_, st := oneLayer(t, map[string]string{"src/lib.rs": testsBase}, map[string]string{"src/lib.rs": strings.Replace(testsBase, "        assert!(f() > 0);\n", "", 1)})
	tr, scouts := coverAll(st)
	_, err := render(t, st, RenderInput{Triage: tr, Scouts: scouts})
	wantRefusal(t, err, "weakening lost-assertions:src/lib.rs:keeps")

	ws := st.Weakenings(st.Layers[0])
	it := item("weak", TierScrutinize, *ws[0].Range)
	it.Reason, it.Weakenings = "weakened-test", []string{ws[0].ID}
	tr.Items = append(tr.Items, it)
	mustRender(t, st, RenderInput{Triage: tr, Scouts: scouts})
}

func Test_missing_spec_test_needs_a_card(t *testing.T) {
	b := basicStack(t)
	tr, scouts := b.triage(item("util", TierScrutinize, b.utilAdded), item("core", TierGlance, b.coreAdded))
	_, err := render(t, b.st, RenderInput{Triage: tr, Scouts: scouts, SpecTests: []string{"spec_names_this"}})
	wantRefusal(t, err, "weakening missing-spec-test:spec_names_this")
}

func Test_assessment_must_cite_code(t *testing.T) {
	b := basicStack(t)
	tr, scouts := b.triage(item("util", TierScrutinize, b.utilAdded), item("core", TierGlance, b.coreAdded))
	_, err := render(t, b.st, RenderInput{Triage: tr, Scouts: scouts, Audit: &Audit{Format: auditFormat, Assessment: Assessment{Text: "Looks coherent."}}})
	wantRefusal(t, err, "assessment needs text and the code it cites")
}

func Test_large_card_links_instead_of_embedding(t *testing.T) {
	_, st := manyLines(t, embedLimit+1)
	tr, scouts := coverAll(st)
	tr.Items[0].Tier, tr.Items[0].Reason, tr.Items[0].Question = TierScrutinize, "engine-flow", "?"
	out := mustRender(t, st, RenderInput{Triage: tr, Scouts: scouts})
	c := readLayer(t, out, 1).Cards[0]
	if c.Display != "links" || c.Ranges[0].Diff || readDiffs(t, out, 1).has(c.Ranges[0].Hash) {
		t.Fatalf("an %d-line card: display %s, embedded %v", embedLimit+1, c.Display, c.Ranges[0].Diff)
	}
	if !strings.Contains(c.Ranges[0].Link, "/pull/1/files/") || !strings.Contains(c.Ranges[0].Link, "R2-R") {
		t.Fatalf("link %s does not open the PR's Files view at the lines", c.Ranges[0].Link)
	}
}

func Test_new_whole_file_card_is_one_link(t *testing.T) {
	_, st := oneLayer(t, map[string]string{"x.txt": "\n"}, map[string]string{"src/new.rs": lines(embedLimit+5, func(i int) string { return fmt.Sprintf("fn n%d() {}", i) })})
	tr, scouts := coverAll(st)
	out := mustRender(t, st, RenderInput{Triage: tr, Scouts: scouts})
	if c := readLayer(t, out, 1).Cards[0]; c.Display != "file" {
		t.Fatalf("a whole new file: display %s, want file", c.Display)
	}
}

func Test_other_layers_touching_the_same_function(t *testing.T) {
	f := newFixture(t)
	f.write("src/a.rs", "fn shared() {\n    one();\n}\n")
	f.trunk(f.commit("base"))
	f.write("src/a.rs", "fn shared() {\n    one();\n    two();\n}\n")
	h1 := f.commit("layer 1")
	f.write("src/a.rs", "fn shared() {\n    one();\n    two();\n    three();\n}\n")
	h2 := f.commit("layer 2")
	st := f.stack(ModeWhole, h1, h2)
	tr, scouts := coverAll(st)
	out := mustRender(t, st, RenderInput{Triage: tr, Scouts: scouts})
	c := readLayer(t, out, 2).Cards[0]
	if len(c.OtherLayers) != 1 || c.OtherLayers[0].PR != 1 || c.OtherLayers[0].Symbol != "fn shared" {
		t.Fatalf("other layers %+v, want layer 1's fn shared", c.OtherLayers)
	}
}
