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
	dir = filepath.Join(t.TempDir(), "brief")
	mux := http.NewServeMux()
	mux.Handle("/brief/", http.StripPrefix("/brief/", http.FileServer(http.Dir(dir))))
	mux.HandleFunc("/api/pulls/", func(w http.ResponseWriter, r *http.Request) { api(w, head) })
	srv := httptest.NewServer(mux)
	t.Cleanup(srv.Close)
	tr, scouts := coverAll(st)
	it := &tr.Items[0]
	it.Tier, it.Reason = TierScrutinize, "security"
	cardText(it)
	it.Title = "A string constant joins a()"
	it.Claim, it.Question = hostile, hostile
	it.Context = "Context for the constant. " + hostile
	it.Options = []Option{{Choice: "Keep the constant", Impact: "Callers of a() see it."}, {Choice: "Drop it", Impact: hostile}}
	it.Examples = []Example{{Label: "What a user sees", Text: "error: the constant is shown\n  at src/a.rs:2"}, {Label: "Before", Text: hostile}}
	it.Diagram = "flowchart LR\n  caller --> a"
	it.UsedBy = []RangeRef{{Path: "src/use.rs", Start: 2, End: 2, Note: "caller() calls a()"}}
	it.Tests = []TestRef{{Name: "pins_a"}}
	cited := LayerRef{Layer: 1, RangeRef: RangeRef{Path: "src/a.rs", Start: 2, End: 2}}
	audit := &Audit{Format: auditFormat, Assessment: Assessment{Text: "Hangs together.", Refs: []LayerRef{cited, cited}}}
	if err := st.Render(RenderInput{Triage: tr, Scouts: scouts, Audit: audit, FreshnessAPI: srv.URL + "/api/pulls/"}, pagePath(t), dir); err != nil {
		t.Fatal(err)
	}
	return dir, srv.URL + "/brief/index.html"
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
		`<span class="choice">Keep the constant</span><span class="impact">Callers of a() see it.</span>`,
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

// reviewDriver is served beside the brief, same origin, and drives it in an
// iframe the way a reviewer would. --dump-dom cannot click, so the driver
// writes what it saw into its own DOM.
const reviewDriver = `<!doctype html><meta charset="utf-8"><title>driver</title>
<pre id="gh"></pre><pre id="prompt"></pre><pre id="progress"></pre><pre id="persist"></pre><pre id="err"></pre>
<pre id="anchors"></pre><pre id="before"></pre><pre id="earlier"></pre><pre id="after"></pre>
<iframe id="f"></iframe>
<script>
const f = document.getElementById("f");
const put = (id, v) => { document.getElementById(id).textContent = v; };
const until = (cond, then, n = 0) => { const v = cond(); if (v) then(v); else if (n < 200) setTimeout(() => until(cond, then, n + 1), 25); else put("err", "timed out"); };
const doc = () => f.contentDocument;
const card = () => doc() && doc().querySelector("article.card");
const button = (text) => [...doc().querySelectorAll("button")].find((b) => b.textContent === text);
const copied = (text) => { button(text).click(); return doc().getElementById("review-text").value; };
// Seed a note on code that is gone, at the anchor both ranges share, as an
// earlier round would have left it.
(async () => {
  const stack = await (await fetch("stack.json")).json();
  const layer = await (await fetch(stack.layers[0].file)).json();
  const anchors = layer.cards.map((c) => c.ranges[0].anchor);
  put("anchors", anchors.join("|"));
  localStorage.setItem("brief:" + stack.stack_key + ":anchor:" + anchors[0], JSON.stringify({ hashes: { stale0: "old note" } }));
  f.src = "index.html";
  until(card, drive);
})().catch((e) => put("err", e.message));
function drive(c) {
  try {
    put("before", copied("Copy GitHub review"));
    put("earlier", (c.querySelector(".earlier-notes") || { textContent: "" }).textContent);
    c.querySelectorAll("button.option")[1].click();
    const note = c.querySelector(".review-note textarea");
    note.value = "Drop it: nobody reads S.\n\nAnd rename a().";
    note.dispatchEvent(new Event("input"));
    [...c.querySelectorAll(".states button")].find((b) => b.textContent === "Concern").click();
    put("progress", doc().querySelector(".layer summary").textContent);
    button("Copy GitHub review").click();
    put("gh", doc().getElementById("review-text").value);
    button("Copy fix prompt").click();
    put("prompt", doc().getElementById("review-text").value);
    f.src = "index.html?again";
    setTimeout(() => until(() => card() && card().querySelector(".review-note textarea") && f.contentWindow.location.search === "?again" && card(), (c2) => {
      put("persist", [c2.querySelector(".review-note textarea").value, c2.querySelector('button.option[aria-pressed="true"] .choice').textContent, c2.dataset.state].join("|"));
      put("after", copied("Copy GitHub review"));
    }), 50);
  } catch (e) { put("err", e.message); }
}
</script>`

// A reviewer picks an answer, writes a note and marks a concern; both copied
// reviews carry them, the layer counts the card closed, and a reload keeps
// all three.
func Test_review_answers_and_notes_reach_the_copied_review(t *testing.T) {
	dir, url := servedBrief(t, apiDown)
	if err := os.WriteFile(filepath.Join(dir, "driver.html"), []byte(reviewDriver), 0o644); err != nil {
		t.Fatal(err)
	}
	dom := dumpDOM(t, strings.TrimSuffix(url, "index.html")+"driver.html")
	field := func(id string) string {
		m := regexp.MustCompile(`(?s)<pre id="` + id + `">(.*?)</pre>`).FindStringSubmatch(dom)
		if m == nil {
			t.Fatalf("driver output lacks #%s:\n%s", id, tail(dom))
		}
		return html.UnescapeString(m[1])
	}
	if e := field("err"); e != "" {
		t.Fatalf("driver: %s", e)
	}
	if p := field("progress"); !strings.Contains(p, "All 1 cards closed") {
		t.Errorf("progress after answering: %q", p)
	}
	item := []string{
		"### #1 · A string constant joins a()",
		"[`src/a.rs:2`](https://github.com/o/r/",
		"**Concern.**",
		"Answer: **Drop it**: " + hostile,
		"Note:\n> Drop it: nobody reads S.\n> \n> And rename a().",
	}
	gh := field("gh")
	if !strings.HasPrefix(gh, "Review of #1, from its review brief: 1 concern, 1 answer.") {
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
	if p := field("persist"); p != "Drop it: nobody reads S.\n\nAnd rename a().|Drop it|concern" {
		t.Errorf("after a reload the card holds %q", p)
	}

	// The two ranges share an anchor, which is what made one note repeat.
	if a := strings.Split(field("anchors"), "|"); len(a) != 2 || a[0] != a[1] {
		t.Fatalf("anchors %q: the fixture needs two cards at one anchor", a)
	}
	// A note on code that is gone shows as an earlier note, and is nobody's
	// note: nothing is copied for it.
	if b := field("before"); b != "No card has a concern, an answer or a note yet." {
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
