package main

import (
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
)

// fixture is a throwaway git repository with origin/master and a stack of
// layer heads.
type fixture struct {
	t     *testing.T
	dir   string
	g     *Git
	rules string // a rules directory for the fixture
}

func newFixture(t *testing.T) *fixture {
	t.Helper()
	dir := t.TempDir()
	f := &fixture{t: t, dir: dir, g: NewGit(dir)}
	f.git("init", "-q", "-b", "master")
	f.rules = filepath.Join(t.TempDir(), "review")
	os.MkdirAll(f.rules, 0o755)
	f.setRules(defaultTestRules)
	return f
}

var defaultTestRules = map[string]string{
	"protected-paths": "core/\n",
	"tripwires":       "unsafe **/*.rs \\bunsafe\\b\n",
	"classes": strings.Join([]string{
		"harness **/fault_provider.rs",
		"harness testkit/",
		"lockfile Cargo.lock",
		"docs **/*.md",
		"test **/tests/",
		"test **/testdata/",
	}, "\n") + "\n",
}

func (f *fixture) setRules(m map[string]string) {
	for name, body := range m {
		if err := os.WriteFile(filepath.Join(f.rules, name), []byte(body), 0o644); err != nil {
			f.t.Fatal(err)
		}
	}
}

func (f *fixture) git(args ...string) string {
	f.t.Helper()
	full := append([]string{"-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false"}, args...)
	out, err := f.g.Run(full...)
	if err != nil {
		f.t.Fatal(err)
	}
	return strings.TrimSpace(string(out))
}

func (f *fixture) write(path, body string) {
	f.t.Helper()
	p := filepath.Join(f.dir, path)
	if err := os.MkdirAll(filepath.Dir(p), 0o755); err != nil {
		f.t.Fatal(err)
	}
	if err := os.WriteFile(p, []byte(body), 0o644); err != nil {
		f.t.Fatal(err)
	}
}

func (f *fixture) rm(path string) { f.git("rm", "-q", path) }

// commit stages everything and commits it, returning the SHA.
func (f *fixture) commit(msg string) string {
	f.t.Helper()
	f.git("add", "-A")
	f.git("commit", "-q", "--allow-empty", "-m", msg)
	return f.git("rev-parse", "HEAD")
}

// trunk points origin/master at a commit.
func (f *fixture) trunk(sha string) { f.git("update-ref", "refs/remotes/origin/master", sha) }

func (f *fixture) rulesLoaded() *Rules {
	f.t.Helper()
	r, err := LoadRules(f.rules)
	if err != nil {
		f.t.Fatal(err)
	}
	return r
}

// stack pins and analyses layers given as heads, bottom to top; PRs are
// numbered 1, 2, ...
func (f *fixture) stack(mode Mode, heads ...string) *Stack {
	f.t.Helper()
	var ls []Layer
	for i, h := range heads {
		ls = append(ls, Layer{PR: i + 1, Head: h, Title: "layer"})
	}
	pin, err := pinLayers(f.g, "o/r", "origin/master", mode, ls)
	if err != nil {
		f.t.Fatal(err)
	}
	st, err := Analyze(f.g, pin, f.rulesLoaded())
	if err != nil {
		f.t.Fatal(err)
	}
	return st
}

// lineAt finds a changed line by its text in a layer.
func lineAt(t *testing.T, d *LayerData, side Side, text string) *CLine {
	t.Helper()
	for _, cl := range d.Lines() {
		if cl.Side == side && cl.Text == text {
			return cl
		}
	}
	t.Fatalf("layer #%d has no %s line %q", d.PR, side, text)
	return nil
}

func classOfText(t *testing.T, d *LayerData, side Side, text string) Class {
	return lineAt(t, d, side, text).Class
}

// coverAll returns a triage with one AI-cleared item per live range of each
// layer (raised to Glance where the floor needs it), and full scout reads.
func coverAll(st *Stack) (*Triage, []Scout) {
	t := &Triage{Format: triageFormat}
	var scouts []Scout
	for _, d := range st.Layers {
		var live []*CLine
		for _, cl := range d.Lines() {
			if cl.Class == ClassLive {
				live = append(live, cl)
			}
		}
		rs := collapse(live)
		for i, r := range rs {
			tier := TierAICleared
			for l := r.Start; l <= r.End; l++ {
				tier = max(tier, d.Line(Loc{r.Path, r.Side, l}).Floor)
			}
			t.Items = append(t.Items, Item{ID: "c" + itoa(d.PR) + "-" + itoa(i), Layer: d.PR, Tier: tier,
				Claim: "covered", Ranges: []RangeRef{r}, ProposedBy: "scout"})
		}
		scouts = append(scouts, Scout{Format: scoutFormat, Agent: "scout:" + itoa(d.PR), Layer: d.PR, Reads: rs})
	}
	return t, scouts
}

func itoa(i int) string { return strconv.Itoa(i) }

var symlink = os.Symlink

// pagePath is the real page, for render.
func pagePath(t *testing.T) string {
	t.Helper()
	p, err := filepath.Abs("../page.html")
	if err != nil {
		t.Fatal(err)
	}
	return p
}

// render runs Render into a fresh directory and returns it.
func render(t *testing.T, st *Stack, in RenderInput) (string, error) {
	t.Helper()
	out := filepath.Join(t.TempDir(), "brief")
	err := st.Render(in, pagePath(t), out)
	return out, err
}

func mustRender(t *testing.T, st *Stack, in RenderInput) string {
	t.Helper()
	out, err := render(t, st, in)
	if err != nil {
		t.Fatal(err)
	}
	return out
}

func refusalsOf(t *testing.T, err error) Refusals {
	t.Helper()
	var r Refusals
	if !errors.As(err, &r) {
		t.Fatalf("want refusals, got %v", err)
	}
	return r
}

func wantRefusal(t *testing.T, err error, substr string) {
	t.Helper()
	for _, r := range refusalsOf(t, err) {
		if strings.Contains(r, substr) {
			return
		}
	}
	t.Fatalf("no refusal mentions %q:\n%v", substr, err)
}

func readLayer(t *testing.T, out string, pr int) *PageLayer {
	t.Helper()
	var pl PageLayer
	b, err := os.ReadFile(filepath.Join(out, "layer-"+itoa(pr)+".json"))
	if err != nil {
		t.Fatal(err)
	}
	if err := json.Unmarshal(b, &pl); err != nil {
		t.Fatal(err)
	}
	return &pl
}

func readDiffs(t *testing.T, out string, pr int) *PageDiffs {
	t.Helper()
	var d PageDiffs
	b, err := os.ReadFile(filepath.Join(out, "layer-"+itoa(pr)+"-diffs.json"))
	if err != nil {
		t.Fatal(err)
	}
	if err := json.Unmarshal(b, &d); err != nil {
		t.Fatal(err)
	}
	return &d
}

func lines(n int, f func(i int) string) string {
	var b strings.Builder
	for i := 0; i < n; i++ {
		b.WriteString(f(i))
		b.WriteByte('\n')
	}
	return b.String()
}
