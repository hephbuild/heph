package main

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func Test_card_text_refusals(t *testing.T) {
	b := basicStack(t)
	long := func(n int) string { return strings.Repeat("x", n) }
	opts := func(n int) []Option {
		var out []Option
		for i := 0; i < n; i++ {
			out = append(out, Option{Choice: "choice " + itoa(i), Impact: "impact " + itoa(i), Change: i%2 == 1})
		}
		return out
	}
	ex := Example{Label: "Output", Text: "ok"}
	use := RangeRef{Path: "src/util.rs", Start: 1, End: 1, Note: "calls it"}
	cases := []struct {
		name string
		edit func(*Item)
		want string
	}{
		{"no title", func(it *Item) { it.Title = " " }, "a Scrutinize card needs a title"},
		{"no context", func(it *Item) { it.Context = "" }, "a Scrutinize card needs context"},
		{"no options", func(it *Item) { it.Options = nil }, "a Scrutinize card needs options"},
		{"one option", func(it *Item) { it.Options = opts(1) }, "1 options: a question offers 2 to 4"},
		{"five options", func(it *Item) { it.Options = opts(5) }, "5 options: a question offers 2 to 4"},
		{"option without impact", func(it *Item) { it.Options[1].Impact = "" }, "option 2 needs a choice and its impact"},
		{"no answer keeps the code", func(it *Item) { it.Options[0].Change = true }, `no option has change: false: a Scrutinize card offers at least one answer that keeps the code as it is`},
		{"no answer asks for a change", func(it *Item) { it.Options[1].Change = false }, "no option has change: true: a Scrutinize card offers at least one answer that asks for a code change"},
		{"long title", func(it *Item) { it.Title = long(titleCap + 1) }, "title is 91 characters, over the cap of 90"},
		{"long title in runes", func(it *Item) { it.Title = strings.Repeat("é", titleCap+1) }, "title is 91 characters"},
		{"long context", func(it *Item) { it.Context = long(contextCap + 1) }, "context is 601 characters, over the cap of 600"},
		{"long question", func(it *Item) { it.Question = long(questionCap + 1) }, "question is 301 characters, over the cap of 300"},
		{"long choice", func(it *Item) { it.Options[0].Choice = long(choiceCap + 1) }, "option 1's choice is 81 characters, over the cap of 80"},
		{"long impact", func(it *Item) { it.Options[0].Impact = long(impactCap + 1) }, "option 1's impact is 301 characters, over the cap of 300"},
		{"id in title", func(it *Item) { it.Title = "Apply U28 in this layer" }, "title names U28: the reviewer cannot look up"},
		{"id in question", func(it *Item) { it.Question = "Does this hold (P20, D4)?" }, "question names P20, D4"},
		{"id in a choice", func(it *Item) { it.Options[0].Choice = "Follow I11" }, "option 1's choice names I11"},
		{"id in an impact", func(it *Item) { it.Options[1].Impact = "Reopens O3." }, "option 2's impact names O3"},
		{"four examples", func(it *Item) { it.Examples = []Example{ex, ex, ex, ex} }, "4 examples, over the cap of 3"},
		{"long example", func(it *Item) {
			it.Examples = []Example{{Label: "Log", Text: lines(exampleLines+1, func(int) string { return "line" })}}
		}, "example 1 is 31 lines, over the cap of 30"},
		{"unlabelled example", func(it *Item) { it.Examples = []Example{{Text: "ok"}} }, "example 1 needs a label and text"},
		{"long label", func(it *Item) { it.Examples = []Example{{Label: long(labelCap + 1), Text: "ok"}} }, "example 1's label is 41 characters"},
		{"diagram kind", func(it *Item) { it.Diagram = "pie\n  \"a\": 1" }, `the diagram starts with "pie"`},
		{"diagram after a comment", func(it *Item) { it.Diagram = "%% a note\nerDiagram\n  A ||--o{ B : has" }, `the diagram starts with "erDiagram"`},
		{"diagram init directive", func(it *Item) { it.Diagram = "%%{init: {\"securityLevel\": \"loose\"}}%%\nflowchart LR\n  a --> b" }, "the diagram has a %%{ directive"},
		{"diagram click", func(it *Item) { it.Diagram = "flowchart LR\n  a --> b\n  click a call alert()" }, `the diagram has a "click" line`},
		{"diagram link", func(it *Item) { it.Diagram = "sequenceDiagram\n  link A: Docs @ https://example.com" }, `the diagram has a "link" line`},
		{"diagram markup", func(it *Item) { it.Diagram = "flowchart LR\n  a[\"<b>x</b>\"]" }, "the diagram has a '<'"},
		{"diagram javascript", func(it *Item) { it.Diagram = "flowchart LR\n  a[\"JavaScript:alert(1)\"]" }, "the diagram has a javascript: URL"},
		{"long diagram", func(it *Item) {
			it.Diagram = "flowchart LR\n" + lines(diagramLines, func(i int) string { return fmt.Sprintf("  n%d --> n%d", i, i+1) })
		}, "the diagram is 41 lines, over the cap of 40"},
		{"four used_by", func(it *Item) { it.UsedBy = []RangeRef{use, use, use, use} }, "4 used_by sites, over the cap of 3"},
		{"used_by without a note", func(it *Item) { u := use; u.Note = ""; it.UsedBy = []RangeRef{u} }, "used_by src/util.rs:1-1 has no note"},
		{"used_by on the old side", func(it *Item) { u := use; u.Side = SideOld; it.UsedBy = []RangeRef{u} }, "is on the old side"},
		{"used_by inverted", func(it *Item) { u := use; u.Start, u.End = 3, 2; it.UsedBy = []RangeRef{u} }, "used_by src/util.rs:3-2 is empty or inverted"},
		{"used_by missing file", func(it *Item) { u := use; u.Path = "src/nowhere.rs"; it.UsedBy = []RangeRef{u} }, "src/nowhere.rs does not exist at #1's head"},
		{"used_by past the end", func(it *Item) { u := use; u.End = 5; it.UsedBy = []RangeRef{u} }, "runs past the end of src/util.rs (4 lines"},
		{"used_by too long", func(it *Item) { u := use; u.End = usedByLines + 1; it.UsedBy = []RangeRef{u} }, "is 41 lines, over the cap of 40"},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			it := item("util", TierScrutinize, b.utilAdded)
			c.edit(&it)
			tr, scouts := b.triage(it, item("core", TierGlance, b.coreAdded))
			_, err := render(t, b.st, RenderInput{Triage: tr, Scouts: scouts})
			wantRefusal(t, err, c.want)
		})
	}
}

// A card that says everything renders, and the caps and the id rule hold on
// every tier while title, context and options stay optional below
// Scrutinize.
func Test_card_text_accepted(t *testing.T) {
	b := basicStack(t)
	it := item("util", TierScrutinize, b.utilAdded)
	it.Title = "S3 and R2 caches get a first() hook"
	it.Diagram = "%% the call path\nsequenceDiagram\n  engine->>cache: first()\n  cache-->>engine: hit"
	it.Examples = []Example{{Label: "Before", Text: "error: no hook"}, {Label: "After", Text: "hook ran"}}
	it.UsedBy = []RangeRef{{Path: "core/engine.rs", Start: 1, End: 2, Note: "run() reaches it"}}
	glance := item("core", TierGlance, b.coreAdded)
	glance.Diagram = "stateDiagram-v2\n  [*] --> Run"
	tr, scouts := b.triage(it, glance)
	mustRender(t, b.st, RenderInput{Triage: tr, Scouts: scouts})

	glance.Title = "Step names D4"
	tr, scouts = b.triage(item("util", TierScrutinize, b.utilAdded), glance)
	_, err := render(t, b.st, RenderInput{Triage: tr, Scouts: scouts})
	wantRefusal(t, err, `item "core": title names D4`)
}

func Test_bare_ids(t *testing.T) {
	for in, want := range map[string]string{
		"U28 and P20":             "U28 P20",
		"(D4), I11.":              "D4 I11",
		"U28 twice: U28":          "U28",
		"S3, R2, V8 and L1 cache": "",
		"E2E tests on x86_64":     "",
		"IPv4 and sha256":         "",
		"U1234 is not an id":      "",
		"u28 lower case":          "",
		"A1-style cells":          "A1",
	} {
		if got := strings.Join(bareIDs(in), " "); got != want {
			t.Errorf("bareIDs(%q) = %q, want %q", in, got, want)
		}
	}
}

// twoLayers: layer 1 changes src/a.rs; layer 2 changes it again and adds
// src/caller.rs, so the two heads hold different files.
func twoLayers(t *testing.T) (*fixture, *Stack) {
	f := newFixture(t)
	f.write("src/a.rs", "fn a() {}\n")
	f.trunk(f.commit("base"))
	f.write("src/a.rs", "fn a() {}\nfn b() {}\n")
	h1 := f.commit("layer 1")
	f.write("src/a.rs", "fn a() {}\nfn b() {}\nfn c() {}\n")
	f.write("src/caller.rs", lines(10, func(i int) string { return fmt.Sprintf("fn caller%d() { a(); }", i) }))
	h2 := f.commit("layer 2")
	return f, f.stack(ModeWhole, h1, h2)
}

func Test_used_by_is_read_at_the_layers_head(t *testing.T) {
	_, st := twoLayers(t)
	itemOf := func(tr *Triage, pr int) *Item {
		for i := range tr.Items {
			if tr.Items[i].Layer == pr {
				return &tr.Items[i]
			}
		}
		t.Fatalf("no item in layer #%d", pr)
		return nil
	}
	// At layer 1's head src/caller.rs does not exist yet and src/a.rs has
	// two lines; the top head has both.
	for _, c := range []struct {
		use  RangeRef
		want string
	}{
		{RangeRef{Path: "src/caller.rs", Start: 5, End: 6, Note: "calls a()"}, "src/caller.rs does not exist at #1's head"},
		{RangeRef{Path: "src/a.rs", Start: 3, End: 3, Note: "c()"}, "runs past the end of src/a.rs (2 lines at #1's head"},
	} {
		tr, scouts := coverAll(st)
		itemOf(tr, 1).UsedBy = []RangeRef{c.use}
		_, err := render(t, st, RenderInput{Triage: tr, Scouts: scouts})
		wantRefusal(t, err, c.want)
	}

	// The same sites at the top head, and an unchanged line of layer 1's
	// head: a use site need not be in the diff.
	tr, scouts := coverAll(st)
	itemOf(tr, 1).UsedBy = []RangeRef{{Path: "src/a.rs", Start: 1, End: 1, Note: "a() itself"}}
	itemOf(tr, 2).UsedBy = []RangeRef{{Path: "src/caller.rs", Start: 5, End: 6, Note: "calls a()"}}
	out := mustRender(t, st, RenderInput{Triage: tr, Scouts: scouts})

	top := st.Layers[1]
	var u PageUse
	for _, c := range readLayer(t, out, 2).Cards {
		if len(c.UsedBy) > 0 {
			u = c.UsedBy[0]
		}
	}
	if want := fmt.Sprintf("https://github.com/o/r/blob/%s/src/caller.rs#L5-L6", top.Head); u.Link != want {
		t.Fatalf("used_by link %q, want %q", u.Link, want)
	}
	if u.Symbol == "" || u.Note != "calls a()" {
		t.Fatalf("used_by %+v: want its symbol and note", u)
	}
	got := readDiffs(t, out, 2).Snippets[u.Key]
	src := st.git.Lines(top.Head, "src/caller.rs")
	if len(got) != 8 { // lines 2..9: three either side of 5-6
		t.Fatalf("snippet has %d lines, want 8 (2-9): %v", len(got), got)
	}
	for _, l := range got {
		n := int(l[2].(float64))
		kind := " "
		if n == 5 || n == 6 {
			kind = ">"
		}
		if l[0] != kind || l[3] != src[n-1] {
			t.Fatalf("snippet line %v, want [%q %d %q]", l, kind, n, src[n-1])
		}
	}
	if int(got[0][2].(float64)) != 2 || int(got[len(got)-1][2].(float64)) != 9 {
		t.Fatalf("snippet spans %v to %v, want 2 to 9", got[0][2], got[len(got)-1][2])
	}
	// Layer 1's snippet is cut from layer 1's head, which ends at line 2.
	var low PageUse
	for _, c := range readLayer(t, out, 1).Cards {
		if len(c.UsedBy) > 0 {
			low = c.UsedBy[0]
		}
	}
	if s := readDiffs(t, out, 1).Snippets[low.Key]; len(s) != 2 || s[0][0] != ">" || s[1][3] != "fn b() {}" {
		t.Fatalf("layer 1 snippet %v, want a() marked and b() as context", s)
	}
	// The snippets are in the diffs file the manifest checks.
	var m Manifest
	mb, _ := os.ReadFile(filepath.Join(out, "manifest.json"))
	if err := json.Unmarshal(mb, &m); err != nil {
		t.Fatal(err)
	}
	body, _ := os.ReadFile(filepath.Join(out, "layer-2-diffs.json"))
	sum := sha256.Sum256(body)
	if m.Files["layer-2-diffs.json"] != hex.EncodeToString(sum[:]) || !strings.Contains(string(body), u.Key) {
		t.Fatal("the snippet is not in a diffs file the manifest checksums")
	}
}
