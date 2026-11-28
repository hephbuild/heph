package main

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func testPin() *Pin { return &Pin{Format: pinFormat, Repo: "o/r", Layers: []Layer{{PR: 1}}} }

func decBlock(at string, ds ...UserDecision) *DecisionsFile {
	return &DecisionsFile{Format: decisionsFormat, StackKey: "o/r#1", BottomPR: 1,
		Heads: []HeadRef{{PR: 1, Head: "abc"}}, CopiedAt: at, Decisions: ds}
}

func dec(card, decision string, hashes ...string) UserDecision {
	d := UserDecision{Layer: 1, Card: card, Title: "t " + card, Decision: decision}
	for _, h := range hashes {
		d.Ranges = append(d.Ranges, DecisionRange{Path: "src/a.rs", Side: SideNew, Start: 1, End: 1, Hash: h, Anchor: "src/a.rs#", Changed: 1})
	}
	return d
}

// writeBlock writes a block as the page copies it: inside a ``` fence.
func writeBlock(t *testing.T, f *DecisionsFile) string {
	t.Helper()
	b, err := json.MarshalIndent(f, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	p := filepath.Join(t.TempDir(), "decisions-in.json")
	if err := os.WriteFile(p, []byte("```json\n"+string(b)+"\n```\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	return p
}

func Test_decisions_merge_refuses_another_format_or_stack(t *testing.T) {
	state := filepath.Join(t.TempDir(), "decisions.json")
	wrong := decBlock("2026-11-28T10:00:00.000Z", dec("a", decisionSettled, "h1"))
	wrong.Format = "heph-review-brief-decisions/0"
	if _, err := MergeDecisionsFile(writeBlock(t, wrong), state, testPin()); err == nil || !strings.Contains(err.Error(), `decisions format "heph-review-brief-decisions/0"`) {
		t.Fatalf("a block of another format: %v", err)
	}
	other := decBlock("2026-11-28T10:00:00.000Z", dec("a", decisionSettled, "h1"))
	other.StackKey = "o/r#9"
	_, err := MergeDecisionsFile(writeBlock(t, other), state, testPin())
	if err == nil || !strings.Contains(err.Error(), `the decisions are for stack "o/r#9" and this brief is "o/r#1": a block from another stack is refused`) {
		t.Fatalf("a block from another stack: %v", err)
	}
	bad := decBlock("2026-11-28T10:00:00.000Z", dec("a", "maybe", "h1"))
	if _, err := MergeDecisionsFile(writeBlock(t, bad), state, testPin()); err == nil || !strings.Contains(err.Error(), `decision "maybe" is neither settled nor change`) {
		t.Fatalf("an unknown decision: %v", err)
	}
	if _, err := os.Stat(state); !os.IsNotExist(err) {
		t.Fatal("a refused block wrote the state file")
	}
}

func Test_decisions_merge_newest_wins_by_hash(t *testing.T) {
	state := filepath.Join(t.TempDir(), "decisions.json")
	merge := func(f *DecisionsFile) *DecisionsFile {
		t.Helper()
		out, err := MergeDecisionsFile(writeBlock(t, f), state, testPin())
		if err != nil {
			t.Fatal(err)
		}
		return out
	}
	merge(decBlock("2026-11-28T10:00:00.000Z", dec("a", decisionSettled, "h1", "h2"), dec("b", decisionSettled, "h3")))
	// A newer round asks for a change on h1: it takes h1, and the older
	// decision keeps only h2.
	merge(decBlock("2026-11-28T12:00:00.000Z", dec("a2", decisionChange, "h1")))
	// A block copied before both loses wherever they name the same hash.
	out := merge(decBlock("2026-11-28T09:00:00.000Z", dec("old", decisionSettled, "h1", "h3", "h4")))
	got := map[string]string{}
	for _, d := range out.Decisions {
		for _, r := range d.Ranges {
			got[r.Hash] = d.Card + ":" + d.Decision
		}
	}
	want := map[string]string{"h1": "a2:change", "h2": "a:settled", "h3": "b:settled", "h4": "old:settled"}
	for h, w := range want {
		if got[h] != w {
			t.Errorf("%s: %q, want %q (all: %v)", h, got[h], w, got)
		}
	}
	if out.CopiedAt != "2026-11-28T12:00:00.000Z" {
		t.Errorf("copied_at %q: the kept file names the newest block", out.CopiedAt)
	}
	kept, err := loadDecisions(state)
	if err != nil || len(kept.Decisions) != len(out.Decisions) {
		t.Fatalf("the kept file does not read back: %v", err)
	}
}

// decisionsOn renders the basic stack and answers its cards as the page
// would copy them.
func decisionsOn(t *testing.T, b *basic, answers map[string]string) *DecisionsFile {
	t.Helper()
	tr, scouts := b.triage(item("util", TierScrutinize, b.utilAdded), item("core", TierGlance, b.coreAdded))
	out := mustRender(t, b.st, RenderInput{Triage: tr, Scouts: scouts})
	f := decBlock("2026-11-28T10:00:00.000Z")
	for _, c := range readLayer(t, out, 1).Cards {
		a, ok := answers[c.ID]
		if !ok {
			continue
		}
		d := UserDecision{Layer: 1, Card: c.ID, Title: c.Title, Question: c.Question, Decision: a, Choice: "Yes", Note: "note on " + c.ID}
		for _, r := range c.Ranges {
			d.Ranges = append(d.Ranges, DecisionRange{Path: r.Path, Side: r.Side, Start: r.Start, End: r.End, Hash: r.Hash, Anchor: r.Anchor, Changed: r.Changed})
		}
		f.Decisions = append(f.Decisions, d)
	}
	return f
}

// moved is the basic stack one round later: second() changed, so util's
// hash moved while its anchor stayed, and core/engine.rs no longer changes.
func moved(b *basic) *Stack {
	b.f.write("src/util.rs", "pub fn a() {}\npub fn first() {}\npub fn b() {}\npub fn second(x: u8) {}\n")
	b.f.write("core/engine.rs", "pub fn run() {}\n")
	return b.f.stack(ModeWhole, b.f.commit("fix round"))
}

func Test_index_classifies_earlier_decisions(t *testing.T) {
	b := basicStack(t)
	f := decisionsOn(t, b, map[string]string{"util": decisionSettled, "core": decisionChange})
	status := func(st *Stack) map[string]EarlierDecision {
		ix, err := st.BuildIndex(nil, f)
		if err != nil {
			t.Fatal(err)
		}
		m := map[string]EarlierDecision{}
		for _, e := range ix.EarlierDecisions {
			m[e.Card] = e
		}
		return m
	}
	now := status(b.st)
	if now["util"].Status != codeUnchanged || now["util"].Instruction != "settled, do not raise again" {
		t.Errorf("util on the same code: %+v", now["util"])
	}
	if now["core"].Status != codeUnchanged || !strings.HasPrefix(now["core"].Instruction, `you asked for "Yes": check whether the change was made`) {
		t.Errorf("core on the same code: %+v", now["core"])
	}
	st := moved(b)
	later := status(st)
	if later["util"].Status != codeChanged || later["util"].Instruction != "the code changed after it was settled; re-check" || len(later["util"].Now) == 0 {
		t.Errorf("util after its code changed: %+v", later["util"])
	}
	if later["core"].Status != codeGone {
		t.Errorf("core after its code went: %+v", later["core"])
	}
	ix, _ := st.BuildIndex(nil, f)
	var md strings.Builder
	ix.WriteText(&md)
	for _, want := range []string{"## Earlier decisions", "`util` \"Title of util\": the code changed after it was settled; re-check", "  > note on util"} {
		if !strings.Contains(md.String(), want) {
			t.Errorf("index.md lacks %q:\n%s", want, md.String())
		}
	}
	other := *f
	other.StackKey = "o/r#2"
	if _, err := st.BuildIndex(nil, &other); err == nil || !strings.Contains(err.Error(), "a block from another stack is refused") {
		t.Errorf("index took decisions from another stack: %v", err)
	}
}

func Test_render_moves_a_settled_unchanged_item_out_of_the_tiers(t *testing.T) {
	b := basicStack(t)
	f := decisionsOn(t, b, map[string]string{"util": decisionSettled})
	tr, scouts := b.triage(item("util", TierScrutinize, b.utilAdded), item("core", TierGlance, b.coreAdded))
	out := mustRender(t, b.st, RenderInput{Triage: tr, Scouts: scouts, Decisions: f})
	pl := readLayer(t, out, 1)
	if len(pl.Settled) != 1 || pl.Settled[0].ID != "util" || pl.Settled[0].EarlierRound == nil || pl.Settled[0].EarlierRound.Note != "note on util" {
		t.Fatalf("settled %+v, want util with its earlier answer", pl.Settled)
	}
	for _, c := range pl.Cards {
		if c.ID == "util" {
			t.Fatal("a settled item is still a card")
		}
	}
	s := pl.Sums
	if s.Settled != 2 || s.Scrutinize != 0 || s.Glance != 1 || s.Scrutinize+s.Glance+s.AICleared+s.Counted+s.Untriaged+s.Settled != s.Total {
		t.Fatalf("sums %+v, want util's 2 lines settled and the total kept", s)
	}
	if pl.Used.Cards != 0 || !readDiffs(t, out, 1).has(pl.Settled[0].Ranges[0].Hash) {
		t.Fatalf("used %+v; the settled item's diff must stay one click away", pl.Used)
	}
	var ps PageStack
	sb, _ := os.ReadFile(filepath.Join(out, "stack.json"))
	if err := json.Unmarshal(sb, &ps); err != nil || ps.OpenCards != 0 {
		t.Fatalf("open cards %d (%v), want 0", ps.OpenCards, err)
	}

	other := *f
	other.StackKey = "o/r#2"
	tr, scouts = b.triage(item("util", TierScrutinize, b.utilAdded), item("core", TierGlance, b.coreAdded))
	_, err := render(t, b.st, RenderInput{Triage: tr, Scouts: scouts, Decisions: &other})
	wantRefusal(t, err, "a block from another stack is refused")
}

func Test_render_keeps_a_change_request_as_a_card(t *testing.T) {
	b := basicStack(t)
	f := decisionsOn(t, b, map[string]string{"util": decisionChange})
	tr, scouts := b.triage(item("util", TierScrutinize, b.utilAdded), item("core", TierGlance, b.coreAdded))
	pl := readLayer(t, mustRender(t, b.st, RenderInput{Triage: tr, Scouts: scouts, Decisions: f}), 1)
	if len(pl.Settled) != 0 {
		t.Fatalf("a change request was settled: %+v", pl.Settled)
	}
	for _, c := range pl.Cards {
		if c.ID == "util" {
			if e := c.EarlierRound; e == nil || e.Decision != decisionChange || e.Code != codeUnchanged || e.Choice != "Yes" {
				t.Fatalf("earlier round %+v, want the unchanged change request", e)
			}
			return
		}
	}
	t.Fatal("no util card")
}

func Test_render_shows_the_earlier_answer_when_the_code_changed(t *testing.T) {
	b := basicStack(t)
	f := decisionsOn(t, b, map[string]string{"util": decisionSettled})
	st := moved(b)
	r := RangeRef{Path: "src/util.rs", Start: 2, End: 4}
	tr := &Triage{Format: triageFormat, Items: []Item{item("util", TierScrutinize, r)}}
	scouts := []Scout{{Format: scoutFormat, Agent: "scout:1", Layer: 1, Reads: []RangeRef{r}}}
	pl := readLayer(t, mustRender(t, st, RenderInput{Triage: tr, Scouts: scouts, Decisions: f}), 1)
	if len(pl.Settled) != 0 || len(pl.Cards) != 1 {
		t.Fatalf("settled %d, cards %d: changed code is asked again", len(pl.Settled), len(pl.Cards))
	}
	if e := pl.Cards[0].EarlierRound; e == nil || e.Code != codeChanged || e.Decision != decisionSettled || e.Note != "note on util" {
		t.Fatalf("earlier round %+v, want the settled answer on changed code", e)
	}
}
