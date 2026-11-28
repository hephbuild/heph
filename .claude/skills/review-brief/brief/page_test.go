package main

import (
	"context"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"path/filepath"
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
func dumpDOM(t *testing.T, url string) string {
	t.Helper()
	chrome := chromePath(t)
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	cmd := exec.CommandContext(ctx, chrome, "--headless=new", "--disable-gpu", "--no-sandbox",
		"--no-first-run", "--no-default-browser-check", "--disable-extensions",
		"--user-data-dir="+t.TempDir(), "--virtual-time-budget=10000", "--dump-dom", url)
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
	f.write("src/a.rs", "fn a() {}\n")
	f.trunk(f.commit("base"))
	f.write("src/a.rs", "fn a() {}\nconst S: &str = \"</script><b>bold</b>\";\n")
	st := f.stack(ModeWhole, f.commit("layer"))
	head := st.Layers[0].Head
	srv := httptest.NewServer(nil)
	t.Cleanup(srv.Close)
	tr, scouts := coverAll(st)
	tr.Items[0].Tier, tr.Items[0].Reason = TierScrutinize, "security"
	tr.Items[0].Claim, tr.Items[0].Question = hostile, hostile
	dir = filepath.Join(t.TempDir(), "brief")
	if err := st.Render(RenderInput{Triage: tr, Scouts: scouts, FreshnessAPI: srv.URL + "/api/pulls/"}, pagePath(t), dir); err != nil {
		t.Fatal(err)
	}
	mux := http.NewServeMux()
	mux.Handle("/brief/", http.StripPrefix("/brief/", http.FileServer(http.Dir(dir))))
	mux.HandleFunc("/api/pulls/", func(w http.ResponseWriter, r *http.Request) { api(w, head) })
	srv.Config.Handler = mux
	return dir, srv.URL + "/brief/index.html"
}

func apiDown(w http.ResponseWriter, _ string) { http.Error(w, "down", http.StatusInternalServerError) }

func Test_page_draws_a_valid_brief(t *testing.T) {
	_, url := servedBrief(t, func(w http.ResponseWriter, head string) {
		fmt.Fprintf(w, `{"head":{"sha":%q}}`, head)
	})
	dom := dumpDOM(t, url)
	for _, want := range []string{`id="ledger"`, `data-state="verified"`, "1 card open", ">current<", "<title>Review brief #1</title>"} {
		if !strings.Contains(dom, want) {
			t.Errorf("page DOM lacks %q", want)
		}
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
