package main

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"html"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strings"
	"testing"
	"time"
)

// The page carries only the JavaScript it needs to draw and to verify the
// manifest (D12). These tests load it in headless Chrome against real
// render output and read back the DOM it built, to cover its refusal and
// freshness branches.
//
// Chrome is found through $BRIEF_CHROME, then PATH, then the macOS app. With
// none the tests skip, unless BRIEF_REQUIRE_BROWSER=1 (CI sets it), so a
// suite that never loaded the page cannot read as one that passed.

func chromePath(t *testing.T) string {
	t.Helper()
	if p := os.Getenv("BRIEF_CHROME"); p != "" {
		return p
	}
	for _, n := range []string{"google-chrome", "google-chrome-stable", "chromium", "chromium-browser", "chrome"} {
		if p, err := exec.LookPath(n); err == nil {
			return p
		}
	}
	if p := "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"; fileExists(p) {
		return p
	}
	if os.Getenv("BRIEF_REQUIRE_BROWSER") == "1" {
		t.Fatal("no Chrome found and BRIEF_REQUIRE_BROWSER=1: set BRIEF_CHROME")
	}
	t.Skip("no Chrome found; set BRIEF_CHROME to run the page tests")
	return ""
}

func fileExists(p string) bool { _, err := os.Stat(p); return err == nil }

// dumpDOM loads a URL in headless Chrome and returns the DOM after the
// page's scripts ran.
func dumpDOM(t *testing.T, url string) string { return dumpDOMNet(t, url, false) }

// dumpDOMNet is dumpDOM with the network reachable when online is set.
func dumpDOMNet(t *testing.T, url string, online bool) string {
	t.Helper()
	chrome := chromePath(t)
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	args := []string{"--headless=new", "--disable-gpu", "--no-sandbox",
		"--no-first-run", "--no-default-browser-check", "--disable-extensions",
		"--user-data-dir=" + t.TempDir(), "--virtual-time-budget=10000", "--dump-dom", url}
	if !online {
		// No host name resolves, so no test reaches the network: the fonts and
		// mermaid fail at once, and the page must draw without them. The fake
		// GitHub API is served at 127.0.0.1, which needs no resolving.
		args = append(args, "--host-resolver-rules=MAP * ~NOTFOUND, EXCLUDE 127.0.0.1")
	}
	cmd := exec.CommandContext(ctx, chrome, args...)
	var stderr strings.Builder
	cmd.Stderr = &stderr
	out, err := cmd.Output()
	if err != nil {
		t.Fatalf("chrome: %v: %s", err, tail(stderr.String()))
	}
	if p := os.Getenv("BRIEF_DUMP_DIR"); p != "" {
		os.WriteFile(filepath.Join(p, t.Name()+".html"), out, 0o644)
	}
	return string(out)
}

const hostile = `<img src=x onerror="document.title='pwned'"></script><script>document.title='pwned'</script>`

// servedBrief renders a one-layer brief whose claim and code carry hostile
// text, and serves it with a fake GitHub API that answers `api`.
func servedBrief(t *testing.T, api func(w http.ResponseWriter, head string)) (dir, url string) {
	t.Helper()
	s := serveBrief(t, api, briefOpts{})
	return s.dir, s.url
}

// briefOpts vary the served brief.
type briefOpts struct {
	glance   bool // the second item is Glance, not AI-cleared
	twoCards bool // the second item is a second Scrutinize card
	// earlier are answers from an earlier round's page, by item id: the
	// brief is rendered with them as its decisions file.
	earlier map[string]UserDecision
}

type served struct {
	dir, url string
	pin      *Pin
}

// The served brief has two items at one anchor: c1-0, the Scrutinize card,
// and c1-1, an AI-cleared (or Glance) item.
func serveBrief(t *testing.T, api func(w http.ResponseWriter, head string), o briefOpts) served {
	t.Helper()
	f := newFixture(t)
	f.write("src/a.rs", "fn a() {}\n// keep\n")
	f.write("src/use.rs", "fn caller() {\n    a();\n}\n\n#[test]\nfn pins_a() {}\n")
	f.trunk(f.commit("base"))
	// Two separate live ranges at one anchor (src/a.rs, top level): the
	// second becomes an AI-cleared item beside the card.
	f.write("src/a.rs", "fn a() {}\nconst S: &str = \"</script><b>bold</b>\";\n// keep\nconst T: u8 = 1;\n")
	st := f.stack(ModeWhole, f.commit("layer"))
	head := st.Layers[0].Head
	// The handler is complete before the server starts: setting it after
	// is a race the detector reports, as it cannot see Chrome connect later.
	dir := filepath.Join(t.TempDir(), "brief")
	mux := http.NewServeMux()
	mux.Handle("/brief/", http.StripPrefix("/brief/", http.FileServer(http.Dir(dir))))
	mux.HandleFunc("/api/pulls/", func(w http.ResponseWriter, r *http.Request) { api(w, head) })
	srv := httptest.NewServer(mux)
	t.Cleanup(srv.Close)
	input := func() RenderInput {
		tr, scouts := coverAll(st)
		it := &tr.Items[0]
		it.Tier, it.Reason = TierScrutinize, "security"
		cardText(it)
		it.Title = "A string constant joins a()"
		it.Claim, it.Question = hostile, hostile
		it.Context = "Context for the constant. " + hostile
		it.Options = []Option{{Choice: "Keep the constant", Impact: "Callers of a() see it."}, {Choice: "Drop it", Impact: hostile, Change: true}}
		it.Examples = []Example{{Label: "What a user sees", Text: "error: the constant is shown\n  at src/a.rs:2"}, {Label: "Before", Text: hostile}}
		it.Diagram = "flowchart LR\n  caller --> a"
		it.UsedBy = []RangeRef{{Path: "src/use.rs", Start: 2, End: 2, Note: "caller() calls a()"}}
		it.Tests = []TestRef{{Name: "pins_a"}}
		if o.glance {
			tr.Items[1].Tier = TierGlance
		}
		if o.twoCards {
			tr.Items[1].Tier, tr.Items[1].Reason = TierScrutinize, "security"
			cardText(&tr.Items[1])
		}
		cited := LayerRef{Layer: 1, RangeRef: RangeRef{Path: "src/a.rs", Start: 2, End: 2}}
		audit := &Audit{Format: auditFormat, Assessment: Assessment{Text: "Hangs together.", Refs: []LayerRef{cited, cited}}}
		return RenderInput{Triage: tr, Scouts: scouts, Audit: audit, FreshnessAPI: srv.URL + "/api/pulls/"}
	}
	in := input()
	if len(o.earlier) > 0 {
		// The earlier round's page is this one: its decisions name its ranges.
		in.Decisions = earlierDecisions(t, st, readLayer(t, mustRender(t, st, input()), 1), o.earlier)
	}
	if err := st.Render(in, pagePath(t), dir); err != nil {
		t.Fatal(err)
	}
	return served{dir: dir, url: srv.URL + "/brief/index.html", pin: st.Pin}
}

// earlierDecisions is the decisions file an earlier round's page would have
// copied, answering the items in `answers` on the page `pl`.
func earlierDecisions(t *testing.T, st *Stack, pl *PageLayer, answers map[string]UserDecision) *DecisionsFile {
	t.Helper()
	f := &DecisionsFile{Format: decisionsFormat, StackKey: stackKeyOf(st.Pin), BottomPR: st.Pin.Layers[0].PR,
		Heads: []HeadRef{{PR: 1, Head: st.Layers[0].Head}}, CopiedAt: "2026-11-28T10:00:00Z"}
	for _, c := range pl.Cards {
		d, ok := answers[c.ID]
		if !ok {
			continue
		}
		d.Layer, d.Card, d.Title, d.Question = 1, c.ID, c.Title, c.Question
		for _, r := range c.Ranges {
			d.Ranges = append(d.Ranges, DecisionRange{Path: r.Path, Side: r.side(), Start: r.Start, End: r.End, Hash: r.Hash, Anchor: r.Anchor, Changed: r.Changed})
		}
		f.Decisions = append(f.Decisions, d)
	}
	if len(f.Decisions) != len(answers) {
		t.Fatalf("answers name %d items and the page has %d of them", len(answers), len(f.Decisions))
	}
	return f
}

func apiDown(w http.ResponseWriter, _ string) { http.Error(w, "down", http.StatusInternalServerError) }

// A Scrutinize card reads top to bottom, and its small print is links that
// name what they open.
func Test_card_reads_top_to_bottom(t *testing.T) {
	_, url := servedBrief(t, apiDown)
	dom := dumpDOM(t, url)
	start := strings.Index(dom, `<article class="card"`)
	end := strings.Index(dom, `</article>`)
	if start < 0 || end < start {
		t.Fatalf("no card on the page:\n%s", tail(dom))
	}
	card := dom[start:end]
	at := 0
	for _, want := range []string{
		`class="card-title">A string constant joins a()</h4>`,
		`class="question">&lt;img src=x`,
		`<span class="answer"><span class="choice">Keep the constant</span><span class="kind">keeps the code</span></span><span class="impact">Callers of a() see it.</span>`,
		`<span class="choice">Drop it</span><span class="kind change">asks for a change</span>`,
		`<span class="choice">Something else</span><span class="kind change">asks for a change</span>`,
		`class="review-note"`,
		`class="context prose">Context for the constant.`,
		`class="diagram"`,
		`<figcaption class="label">What a user sees</figcaption><pre class="text">error: the constant is shown` + "\n  at src/a.rs:2</pre>",
		`class="diff"`,
		`<details class="uses">`,
		`<tr class="hit"><td class="n">2</td><td>    a();</td></tr>`,
		`Why it's flagged`,
		`class="card-foot"`,
	} {
		i := strings.Index(card[at:], want)
		if i < 0 {
			t.Fatalf("after offset %d the card lacks %q:\n%s", at, want, card)
		}
		at += i + len(want)
	}
	if !regexp.MustCompile(`class="diagram"><pre class="text">flowchart LR\n  caller --&gt; a</pre>|class="diagram"><svg`).MatchString(card) {
		t.Errorf("the diagram is neither its source as text nor an svg:\n%s", card)
	}
	for _, re := range []string{
		`<a href="https://github.com/o/r/pull/1/commits/[0-9a-f]{40}"[^>]*>[0-9a-f]{9}</a>`,
		`<a href="https://github.com/o/r/search\?q=pins_a&amp;type=code"[^>]*>pins_a</a>`,
		`<a href="https://github.com/o/r/blob/[0-9a-f]{40}/src/use.rs#L2-L2"[^>]*>src/use.rs:2</a>`,
	} {
		if !regexp.MustCompile(re).MatchString(card) {
			t.Errorf("the card has no link matching %s", re)
		}
	}
	text := regexp.MustCompile(`(?s)<script\b.*?</script>|<style\b.*?</style>|<[^>]*>`).ReplaceAllString(dom, " ")
	if strings.Contains(text, "https://") || strings.Contains(text, "[object ") {
		t.Errorf("the page shows a URL or a stringified object as text")
	}
}

// driverPrelude is served beside the brief, same origin, and drives it in an
// iframe the way a reviewer would. --dump-dom cannot click, so the driver
// writes what it saw into its own DOM, one <pre> per field. A test appends
// its own script, which calls start(seed, drive).
const driverPrelude = `<!doctype html><meta charset="utf-8"><title>driver</title>
<pre id="err"></pre>
<iframe id="f"></iframe>
<script>
const f = document.getElementById("f");
const put = (id, v) => { let e = document.getElementById(id); if (!e) { e = document.createElement("pre"); e.id = id; document.body.prepend(e); } e.textContent = v; };
const until = (cond, then, n = 0) => { const v = cond(); if (v) then(v); else if (n < 200) setTimeout(() => until(cond, then, n + 1), 25); else put("err", "timed out"); };
const doc = () => f.contentDocument;
const card = () => doc() && doc().querySelector("article.card");
// item is the served brief's second item, c1-1: Glance or AI-cleared.
const item = () => doc().querySelector('details.item[id^="card-"]');
const button = (text, root) => [...(root || doc()).querySelectorAll("button")].find((b) => b.textContent === text);
const answer = (root, choice) => root.querySelector('button[data-choice="' + choice + '"]');
const copied = (text) => { button(text).click(); return doc().getElementById("review-text").value; };
const typeNote = (root, text) => { const n = root.querySelector(".review-note textarea"); n.value = text; n.dispatchEvent(new Event("input")); };
const progress = () => doc().querySelector(".layer summary").textContent;
const openCount = () => doc().getElementById("open-count").textContent;
const key = (stack, hash) => "brief:" + stack.stack_key + ":" + hash;
// start lets seed(stack, layer) write this browser's storage, as an earlier
// visit would have, then loads the brief and calls drive with its card.
function start(seed, drive) {
  (async () => {
    const stack = await (await fetch("stack.json")).json();
    const layer = await (await fetch(stack.layers[0].file)).json();
    seed(stack, layer);
    f.src = "index.html";
    until(card, (c) => { try { drive(c, stack, layer); } catch (e) { put("err", e.message + "\n" + e.stack); } });
  })().catch((e) => put("err", e.message));
}
</script>
<script>
`

// driveBrief serves script after driverPrelude beside the brief, loads it,
// and returns the fields the driver wrote. A driver error fails the test.
func driveBrief(t *testing.T, dir, url, script string) func(id string) string {
	t.Helper()
	if err := os.WriteFile(filepath.Join(dir, "driver.html"), []byte(driverPrelude+script+"\n</script>"), 0o644); err != nil {
		t.Fatal(err)
	}
	dom := dumpDOM(t, strings.TrimSuffix(url, "index.html")+"driver.html")
	field := func(id string) string {
		t.Helper()
		m := regexp.MustCompile(`(?s)<pre id="` + id + `">(.*?)</pre>`).FindStringSubmatch(dom)
		if m == nil {
			t.Fatalf("driver output lacks #%s:\n%s", id, tail(dom))
		}
		return html.UnescapeString(m[1])
	}
	if e := field("err"); e != "" {
		t.Fatalf("driver: %s", e)
	}
	return field
}

const noReview = "No card or item asks for a change or has a note yet."

// Seeds a note on code that is gone, at the anchor both ranges share, as an
// earlier round would have left it; then answers the card with a change and
// writes a note.
const reviewDriver = `
start((stack, layer) => {
  const anchors = layer.cards.map((c) => c.ranges[0].anchor);
  put("anchors", anchors.join("|"));
  localStorage.setItem("brief:" + stack.stack_key + ":anchor:" + anchors[0], JSON.stringify({ hashes: { stale0: "old note" } }));
}, (c) => {
  put("before", copied("Copy GitHub review"));
  put("earlier", (c.querySelector(".earlier-notes") || { textContent: "" }).textContent);
  answer(c, "Drop it").click();
  typeNote(c, "Drop it: nobody reads S.\n\nAnd rename a().");
  put("progress", progress());
  put("state", c.dataset.state + "|" + c.querySelector(".chip.state").textContent + "|" + openCount());
  put("gh", copied("Copy GitHub review"));
  put("prompt", copied("Copy fix prompt"));
  f.src = "index.html?again";
  setTimeout(() => until(() => f.contentWindow.location.search === "?again" && card() && card().querySelector(".review-note textarea") && card(), (c2) => {
    put("persist", [c2.querySelector(".review-note textarea").value, c2.querySelector('button.option[aria-pressed="true"] .choice').textContent, c2.dataset.state].join("|"));
    put("after", copied("Copy GitHub review"));
  }), 50);
});`

// A reviewer picks an answer that asks for a change and writes a note; both
// copied reviews carry them, the card reads as a change request, the layer
// and the top line count it closed, and a reload keeps the answer and note.
func Test_review_answers_and_notes_reach_the_copied_review(t *testing.T) {
	dir, url := servedBrief(t, apiDown)
	field := driveBrief(t, dir, url, reviewDriver)
	if p := field("progress"); !strings.Contains(p, "All 1 cards closed") {
		t.Errorf("progress after answering: %q", p)
	}
	if s := field("state"); s != "change|change requested|0 cards open" {
		t.Errorf("after a change answer the card reads %q", s)
	}
	item := []string{
		"### #1 · A string constant joins a()",
		"[`src/a.rs:2`](https://github.com/o/r/",
		"**Change requested.**",
		"Answer: **Drop it**: " + hostile,
		"Note:\n> Drop it: nobody reads S.\n> \n> And rename a().",
	}
	gh := field("gh")
	if !strings.HasPrefix(gh, "Review of #1, from its review brief: 1 change request, 1 note.") {
		t.Errorf("GitHub review starts %q", gh[:min(80, len(gh))])
	}
	prompt := field("prompt")
	if !strings.HasPrefix(prompt, "/goal apply my review of the stack #1.") {
		t.Errorf("fix prompt starts %q", prompt[:min(80, len(prompt))])
	}
	for _, want := range item {
		if !strings.Contains(gh, want) {
			t.Errorf("GitHub review lacks %q:\n%s", want, gh)
		}
		if !strings.Contains(prompt, want) {
			t.Errorf("fix prompt lacks %q", want)
		}
	}
	if p := field("persist"); p != "Drop it: nobody reads S.\n\nAnd rename a().|Drop it|change" {
		t.Errorf("after a reload the card holds %q", p)
	}

	// The two ranges share an anchor, which is what made one note repeat.
	if a := strings.Split(field("anchors"), "|"); len(a) != 2 || a[0] != a[1] {
		t.Fatalf("anchors %q: the fixture needs two cards at one anchor", a)
	}
	// A note on code that is gone shows as an earlier note, and is nobody's
	// note: nothing is copied for it.
	if b := field("before"); b != noReview {
		t.Errorf("before any review, the copy holds %q", b)
	}
	if e := field("earlier"); !strings.Contains(e, "old note") {
		t.Errorf("the card does not show the earlier note: %q", e)
	}
	// One note on one card is copied once, before and after a reload, and
	// the earlier note is dropped once a range there is reviewed.
	for _, id := range []string{"gh", "after"} {
		got := field(id)
		if n := strings.Count(got, "nobody reads S."); n != 1 || strings.Count(got, "### ") != 1 || strings.Contains(got, "old note") {
			t.Errorf("%s: the note appears %d times in %d items:\n%s", id, n, strings.Count(got, "### "), got)
		}
	}
}

// An answer that keeps the code settles the card and closes it; a settled
// card with no note is left out of both copies; picking the answer again
// reopens the card.
func Test_a_keep_answer_settles_the_card(t *testing.T) {
	dir, url := servedBrief(t, apiDown)
	field := driveBrief(t, dir, url, `
start(() => {}, (c) => {
  const keep = answer(c, "Keep the constant");
  keep.click();
  put("state", [c.dataset.state, c.querySelector(".chip.state").textContent, keep.getAttribute("aria-pressed"), openCount()].join("|"));
  put("progress", progress());
  put("gh", copied("Copy GitHub review"));
  put("prompt", copied("Copy fix prompt"));
  keep.click();
  put("cleared", [c.dataset.state, keep.getAttribute("aria-pressed"), openCount()].join("|"));
  put("progress2", progress());
});`)
	if s := field("state"); s != "settled|settled|true|0 cards open" {
		t.Errorf("after a keep answer the card reads %q", s)
	}
	if p := field("progress"); !strings.Contains(p, "All 1 cards closed") {
		t.Errorf("progress after settling: %q", p)
	}
	for _, id := range []string{"gh", "prompt"} {
		if got := field(id); got != noReview {
			t.Errorf("%s copies a settled card with no note: %q", id, got)
		}
	}
	if s := field("cleared"); s != "open|false|1 card open" {
		t.Errorf("picking the answer again leaves the card %q", s)
	}
	if p := field("progress2"); !strings.Contains(p, "0 of 1 cards closed") {
		t.Errorf("progress after clearing: %q", p)
	}
}

// "Something else" asks for a change, and with no note the card says the
// note is missing.
func Test_something_else_without_a_note_is_a_change_request(t *testing.T) {
	dir, url := servedBrief(t, apiDown)
	field := driveBrief(t, dir, url, `
start(() => {}, (c) => {
  const hint = c.querySelector(".note-hint");
  put("hint0", String(hint.hidden));
  answer(c, "Something else").click();
  put("state", c.dataset.state + "|" + openCount());
  put("hint1", String(hint.hidden));
  put("gh", copied("Copy GitHub review"));
  put("decisions", copied("Copy decisions for the next round"));
  typeNote(c, "Rename it.");
  put("hint2", String(hint.hidden));
});`)
	if s := field("state"); s != "change|0 cards open" {
		t.Errorf("after Something else the card reads %q", s)
	}
	if h := field("hint0") + field("hint1") + field("hint2"); h != "truefalsetrue" {
		t.Errorf("the missing-note hint shows %q (before, without a note, with one), want truefalsetrue", h)
	}
	gh := field("gh")
	for _, want := range []string{"1 change request, 0 notes.", "**Change requested.**", "Answer: **Something else**\n"} {
		if !strings.Contains(gh+"\n", want) {
			t.Errorf("GitHub review lacks %q:\n%s", want, gh)
		}
	}
	f, err := parseDecisions([]byte(field("decisions")))
	if err != nil {
		t.Fatal(err)
	}
	if len(f.Decisions) != 1 || f.Decisions[0].Decision != decisionChange || f.Decisions[0].Choice != "Something else" || f.Decisions[0].Note != "" {
		t.Errorf("decisions %+v, want one change request answered Something else", f.Decisions)
	}
}

// The decisions block is the format merge reads: the settled card and the
// item with its change request, each with its ranges, and merge folds it
// into the kept file. The copied review leaves the settled card out.
func Test_decisions_block_round_trips_through_merge(t *testing.T) {
	s := serveBrief(t, apiDown, briefOpts{})
	field := driveBrief(t, s.dir, s.url, `
start(() => {}, (c) => {
  answer(c, "Keep the constant").click();
  button("Needs a change", item()).click();
  typeNote(item(), "Name it.");
  put("decisions", copied("Copy decisions for the next round"));
  put("gh", copied("Copy GitHub review"));
});`)
	text := field("decisions")
	if !strings.HasPrefix(text, "```json\n") || !strings.HasSuffix(strings.TrimSpace(text), "```") {
		t.Errorf("the block is not fenced:\n%s", text)
	}
	f, err := parseDecisions([]byte(text))
	if err != nil {
		t.Fatal(err)
	}
	if f.Format != decisionsFormat || f.StackKey != stackKeyOf(s.pin) || f.BottomPR != 1 || len(f.Heads) != 1 || f.Heads[0].PR != 1 || len(f.Heads[0].Head) != 40 {
		t.Errorf("block header %+v", f)
	}
	if _, err := parseTime(f.CopiedAt); err != nil {
		t.Errorf("copied_at: %v", err)
	}
	if len(f.Decisions) != 2 {
		t.Fatalf("decisions %+v, want the card and the item", f.Decisions)
	}
	card, it := f.Decisions[0], f.Decisions[1]
	if card.Card != "c1-0" || card.Decision != decisionSettled || card.Choice != "Keep the constant" || card.Question != hostile || card.Note != "" {
		t.Errorf("card decision %+v", card)
	}
	if it.Card != "c1-1" || it.Decision != decisionChange || it.Choice != "Needs a change" || it.Note != "Name it." || it.Question != "" {
		t.Errorf("item decision %+v", it)
	}
	for _, d := range f.Decisions {
		if len(d.Ranges) != 1 || d.Ranges[0].Hash == "" || d.Ranges[0].Side != SideNew || d.Ranges[0].Changed < 1 || !strings.HasPrefix(d.Ranges[0].Anchor, "src/a.rs") {
			t.Errorf("%s ranges %+v", d.Card, d.Ranges)
		}
	}
	in := filepath.Join(t.TempDir(), "decisions-in.json")
	if err := os.WriteFile(in, []byte(text), 0o644); err != nil {
		t.Fatal(err)
	}
	state := filepath.Join(t.TempDir(), "st", "decisions.json")
	out, err := MergeDecisionsFile(in, state, s.pin)
	if err != nil {
		t.Fatalf("merge refused the copied block: %v", err)
	}
	if len(out.Decisions) != 2 || out.Decisions[0].CopiedAt != f.CopiedAt {
		t.Errorf("merged %+v", out.Decisions)
	}
	gh := field("gh")
	if !strings.Contains(gh, "Name it.") || strings.Contains(gh, "A string constant joins a()") {
		t.Errorf("the GitHub review must list the item's change request and leave the settled card out:\n%s", gh)
	}
}

// Records written before answers carried `change` still read: a concern is
// a change request, and a record with only a choice takes its answer's.
func Test_legacy_records_read_as_decisions(t *testing.T) {
	dir, url := servedBrief(t, apiDown)
	field := driveBrief(t, dir, url, `
start((stack, layer) => {
  const [c, it] = layer.cards;
  for (const r of c.ranges) localStorage.setItem(key(stack, r.hash), JSON.stringify({ state: "concern", choice: null, note: "legacy concern", context: r.context }));
  for (const r of it.ranges) localStorage.setItem(key(stack, r.hash), JSON.stringify({ choice: "Fine", note: "", context: r.context }));
}, (c) => {
  put("card", c.dataset.state + "|" + c.querySelector(".chip.state").textContent + "|" + openCount());
  put("item", item().dataset.state);
  put("gh", copied("Copy GitHub review"));
});`)
	if s := field("card"); s != "change|change requested|0 cards open" {
		t.Errorf("a legacy concern reads %q", s)
	}
	if s := field("item"); s != "settled" {
		t.Errorf("a legacy choice of Fine reads %q", s)
	}
	if gh := field("gh"); !strings.Contains(gh, "**Change requested.**") || !strings.Contains(gh, "> legacy concern") {
		t.Errorf("the legacy concern is not in the review:\n%s", gh)
	}
}

// A Glance item is answered with its two buttons, and its summary chip
// follows.
func Test_glance_item_buttons(t *testing.T) {
	s := serveBrief(t, apiDown, briefOpts{glance: true})
	field := driveBrief(t, s.dir, s.url, `
start(() => {}, (c) => {
  const it = item();
  const read = () => it.dataset.state + "|" + it.querySelector("summary .chip.state").textContent;
  put("h3", [...doc().querySelectorAll("h3")].map((h) => h.textContent).join("|"));
  put("s0", read());
  button("Fine", it).click();
  put("s1", read() + "|" + button("Fine", it).getAttribute("aria-pressed"));
  put("gh1", copied("Copy GitHub review"));
  button("Needs a change", it).click();
  typeNote(it, "Split it.");
  put("s2", read() + "|" + button("Fine", it).getAttribute("aria-pressed"));
  put("gh2", copied("Copy GitHub review"));
  button("Needs a change", it).click();
  put("s3", read());
});`)
	if h := field("h3"); !strings.Contains(h, "Glance") {
		t.Fatalf("no Glance section: %q", h)
	}
	for id, want := range map[string]string{"s0": "open|open", "s1": "settled|settled|true", "s2": "change|change requested|false", "s3": "open|open"} {
		if got := field(id); got != want {
			t.Errorf("%s: the item reads %q, want %q", id, got, want)
		}
	}
	if g := field("gh1"); g != noReview {
		t.Errorf("a Fine item with no note is copied: %q", g)
	}
	if g := field("gh2"); !strings.Contains(g, "**Change requested.**") || !strings.Contains(g, "> Split it.") {
		t.Errorf("the item's change request is not in the review:\n%s", g)
	}
}

// A decided card folds to its title and answer, so what still needs the user
// stays in view: a card settled on an earlier visit loads folded and after
// the open one, "Show card" unfolds it, a keep answer folds at once, and a
// change answer stays open for its note until folded.
func Test_a_decided_card_folds(t *testing.T) {
	s := serveBrief(t, apiDown, briefOpts{twoCards: true})
	field := driveBrief(t, s.dir, s.url, `
const cards = () => [...doc().querySelectorAll("article.card")];
const read = (c) => [c.id, c.classList.contains("folded"), c.querySelector(".card-body").hidden, c.querySelector("button.fold").hidden ? "-" : c.querySelector("button.fold").textContent, c.querySelector(".answer-line").textContent].join("|");
start((stack, layer) => {
  const r = layer.cards.find((c) => c.id === "c1-0").ranges[0];
  localStorage.setItem(key(stack, r.hash), JSON.stringify({ choice: "Keep the constant", change: false, note: "fine by me\nsecond line", context: r.context }));
}, () => {
  const [first, second] = cards();
  put("load", read(first) + "\n" + read(second));
  button("Show card", second).click();
  put("shown", read(second));
  answer(second, "Keep the constant").click();
  put("cleared", read(second));
  answer(second, "Keep the constant").click();
  put("kept", read(second));
  answer(first, "No").click();
  put("change", read(first));
  button("Fold", first).click();
  put("folded", read(first));
});`)
	for id, want := range map[string]string{
		"load": "card-1-c1-1|false|false|-|\n" +
			"card-1-c1-0|true|true|Show card|You settled it: Keep the constant · fine by me",
		"shown":   "card-1-c1-0|false|false|Fold|You settled it: Keep the constant · fine by me",
		"cleared": "card-1-c1-0|false|false|-|",
		"kept":    "card-1-c1-0|true|true|Show card|You settled it: Keep the constant · fine by me",
		"change":  "card-1-c1-1|false|false|Fold|You asked for a change: No",
		"folded":  "card-1-c1-1|true|true|Show card|You asked for a change: No",
	} {
		if got := field(id); got != want {
			t.Errorf("%s:\n got %q\nwant %q", id, got, want)
		}
	}
}

// A brief rendered with the decisions of an earlier round shows what was
// settled under "Settled earlier", with the earlier answer and note, the
// ledger counts it and still verifies, and a change request whose code is
// unchanged says it is not done yet.
func Test_settled_earlier_brief(t *testing.T) {
	s := serveBrief(t, apiDown, briefOpts{earlier: map[string]UserDecision{
		"c1-0": {Decision: decisionChange, Choice: "Drop it", Note: "drop it, said earlier"},
		"c1-1": {Decision: decisionSettled, Choice: "Fine", Note: "fine, said earlier"},
	}})
	pl := readLayer(t, s.dir, 1)
	if len(pl.Settled) != 1 || pl.Sums.Settled == 0 {
		t.Fatalf("fixture: settled %d items, %d lines", len(pl.Settled), pl.Sums.Settled)
	}
	dom := dumpDOM(t, s.url)
	if strings.Contains(dom, `id="integrity-error"`) {
		t.Fatalf("a brief with settled lines was refused:\n%s", tail(dom))
	}
	for _, want := range []string{`id="ledger"`, `data-state="verified"`, `class="seg-settled"`, " settled earlier · ",
		`id="settled-1"`, "<strong>Settled earlier</strong>", "fine, said earlier", "You settled this in an earlier round: ",
		"you asked for a change here: not done yet", "drop it, said earlier", "0 cards open",
		`class="card folded"`, "You asked for a change: Drop it (in an earlier round; not done yet)"} {
		if !strings.Contains(dom, want) {
			t.Errorf("page DOM lacks %q", want)
		}
	}
	i := strings.Index(dom, `id="settled-1"`)
	if j := strings.Index(dom[max(i, 0):], `class="chip state state-settled">settled<`); i < 0 || j < 0 {
		t.Errorf("the settled item does not read as settled")
	}
}

// With the network up, the pinned mermaid loads from cdnjs, passes its SRI
// check and draws the card's diagram. It needs the network, so it runs only
// with BRIEF_NETWORK=1.
func Test_diagram_draws_with_mermaid(t *testing.T) {
	if os.Getenv("BRIEF_NETWORK") != "1" {
		t.Skip("set BRIEF_NETWORK=1 to load mermaid from cdnjs")
	}
	_, url := servedBrief(t, apiDown)
	dom := dumpDOMNet(t, url, true)
	if !regexp.MustCompile(`class="diagram"><svg`).MatchString(dom) {
		i := strings.Index(dom, `class="diagram"`)
		t.Fatalf("the diagram was not drawn:\n%s", dom[max(i, 0):min(max(i, 0)+400, len(dom))])
	}
}

// A diagram that reaches the page with markup in it (render refuses one, so
// the test writes it into the data) is shown as text and runs nothing.
func Test_hostile_diagram_is_text(t *testing.T) {
	dir, url := servedBrief(t, apiDown)
	p := filepath.Join(dir, "layer-1.json")
	var pl PageLayer
	b, _ := os.ReadFile(p)
	if err := json.Unmarshal(b, &pl); err != nil {
		t.Fatal(err)
	}
	pl.Cards[0].Diagram = "flowchart LR\n  A[\"" + hostile + "\"]"
	b, _ = json.Marshal(pl)
	if err := os.WriteFile(p, b, 0o644); err != nil {
		t.Fatal(err)
	}
	var m Manifest
	mb, _ := os.ReadFile(filepath.Join(dir, "manifest.json"))
	if err := json.Unmarshal(mb, &m); err != nil {
		t.Fatal(err)
	}
	sum := sha256.Sum256(b)
	m.Files["layer-1.json"] = hex.EncodeToString(sum[:])
	mb, _ = json.MarshalIndent(m, "", "  ")
	if err := os.WriteFile(filepath.Join(dir, "manifest.json"), mb, 0o644); err != nil {
		t.Fatal(err)
	}
	dom := dumpDOM(t, url)
	i := strings.Index(dom, `class="diagram"`)
	if i < 0 || !strings.Contains(dom[i:], `<pre class="text">flowchart LR`+"\n"+`  A["&lt;img src=x onerror=`) {
		t.Fatalf("the hostile diagram is not on the page as text:\n%s", tail(dom))
	}
	if strings.Contains(dom, "<title>pwned") || strings.Contains(dom, `<img src="x"`) {
		t.Fatal("the diagram's markup ran")
	}
}

func Test_page_draws_a_valid_brief(t *testing.T) {
	_, url := servedBrief(t, func(w http.ResponseWriter, head string) {
		fmt.Fprintf(w, `{"head":{"sha":%q}}`, head)
	})
	dom := dumpDOM(t, url)
	for _, want := range []string{`id="ledger"`, `data-state="verified"`, "1 card open", ">current<", "<title>Review brief #1</title>",
		"Cites: <code>#1 src/a.rs:2</code>, <code>#1 src/a.rs:2</code>"} {
		if !strings.Contains(dom, want) {
			t.Errorf("page DOM lacks %q", want)
		}
	}
	if strings.Contains(dom, "[object ") {
		t.Error("the page stringified an element instead of inserting it")
	}
	if strings.Contains(dom, `id="integrity-error"`) {
		t.Error("a valid brief was refused")
	}
}

func Test_hostile_text_is_escaped(t *testing.T) {
	_, url := servedBrief(t, apiDown)
	dom := dumpDOM(t, url)
	if !strings.Contains(dom, "&lt;img src=x onerror=") {
		t.Fatal("the hostile claim is not on the page as text")
	}
	if strings.Contains(dom, "<img src=\"x\"") || strings.Contains(dom, "<title>pwned") || strings.Contains(dom, "<b>bold</b>") {
		t.Fatal("hostile text was parsed as HTML")
	}
	if !strings.Contains(dom, "&lt;b&gt;bold&lt;/b&gt;") {
		t.Fatal("the hostile code line is not in the embedded diff as text")
	}
}

func Test_manifest_mismatch_blocks_ledger(t *testing.T) {
	dir, url := servedBrief(t, apiDown)
	p := filepath.Join(dir, "layer-1.json")
	b, _ := os.ReadFile(p)
	if err := os.WriteFile(p, append(b, ' '), 0o644); err != nil {
		t.Fatal(err)
	}
	dom := dumpDOM(t, url)
	if !strings.Contains(dom, `id="integrity-error"`) || !strings.Contains(dom, "does not match its manifest checksum") {
		t.Fatalf("a tampered layer file was not refused:\n%s", tail(dom))
	}
	if strings.Contains(dom, `id="ledger"`) {
		t.Fatal("the ledger was drawn over a checksum mismatch")
	}
}

func Test_bad_sums_block_ledger(t *testing.T) {
	dir, url := servedBrief(t, apiDown)
	p := filepath.Join(dir, "manifest.json")
	b, _ := os.ReadFile(p)
	if err := os.WriteFile(p, []byte(strings.Replace(string(b), `"counted": 0`, `"counted": 1`, 1)), 0o644); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(b), `"counted": 0`) {
		t.Fatal("fixture: the manifest has no counted sum to corrupt")
	}
	dom := dumpDOM(t, url)
	if !strings.Contains(dom, `id="integrity-error"`) || strings.Contains(dom, `id="ledger"`) {
		t.Fatalf("a manifest whose sums disagree was not refused:\n%s", tail(dom))
	}
}

func Test_missing_file_blocks_ledger(t *testing.T) {
	dir, url := servedBrief(t, apiDown)
	if err := os.Remove(filepath.Join(dir, "stack.json")); err != nil {
		t.Fatal(err)
	}
	dom := dumpDOM(t, url)
	if !strings.Contains(dom, `id="integrity-error"`) || !strings.Contains(dom, "did not load") {
		t.Fatalf("a missing file was not refused:\n%s", tail(dom))
	}
}

func Test_stale_check_failure_says_unknown(t *testing.T) {
	_, url := servedBrief(t, apiDown)
	dom := dumpDOM(t, url)
	if !strings.Contains(dom, "freshness unknown") || strings.Contains(dom, ">current<") {
		t.Fatalf("a failed freshness check does not say unknown:\n%s", tail(dom))
	}
}

func Test_moved_branch_shows_stale(t *testing.T) {
	_, url := servedBrief(t, func(w http.ResponseWriter, _ string) {
		fmt.Fprint(w, `{"head":{"sha":"0123456789abcdef0123456789abcdef01234567"}}`)
	})
	dom := dumpDOM(t, url)
	if !strings.Contains(dom, "stale: the branch is now at 012345678") {
		t.Fatalf("a moved branch is not shown as stale:\n%s", tail(dom))
	}
}

func tail(s string) string {
	if len(s) > 3000 {
		return s[len(s)-3000:]
	}
	return s
}
