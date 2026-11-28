// Command brief is the program half of a review brief (see ../SKILL.md):
// everything that can be computed, so the model stages only judge.
//
//	brief pin    --pr N [--trunk origin/master] [--mode whole|per-layer] --out pin.json
//	brief pin    --layers PR:SHA,PR:SHA... --repo owner/name --out pin.json
//	brief index  --pin pin.json [--spec-tests names.txt] --out DIR
//	brief render --pin pin.json --triage triage.json [--audit audit.json]
//	             [--surface surface.json] --scout scout.json... [--spec-tests names.txt]
//	             --page page.html --out DIR [--allow-untriaged] [--decisions decisions.json]
//	brief decisions merge --pin pin.json --in block.json --state decisions.json
//
// decisions merge folds the block the page copies ("Copy decisions for the
// next round") into the file kept across rounds; index and render read it
// with --decisions (index also takes --decisions).
//
// index writes index.json and index.md: every changed line classified, the
// live lines as ranges with their floors, the weakenings, the files several
// layers touch and near-identical added blocks. render checks a triage and
// writes the page data; it exits 1 and writes nothing on any refusal, and
// exits 2 after writing when --allow-untriaged published untriaged lines.
//
// It runs from inside the repository it briefs; rules come from
// .claude/review/ there.
package main

import (
	"errors"
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"strings"
)

func main() {
	if len(os.Args) < 2 {
		fmt.Fprintln(os.Stderr, "usage: brief pin|index|render [flags]")
		os.Exit(64)
	}
	var err error
	switch os.Args[1] {
	case "pin":
		err = cmdPin(os.Args[2:])
	case "index":
		err = cmdIndex(os.Args[2:])
	case "render":
		err = cmdRender(os.Args[2:])
	case "decisions":
		if len(os.Args) < 3 || os.Args[2] != "merge" {
			err = errors.New("usage: brief decisions merge --pin pin.json --in block.json --state decisions.json")
			break
		}
		err = cmdDecisionsMerge(os.Args[3:])
	default:
		err = fmt.Errorf("unknown command %q", os.Args[1])
	}
	var u ErrUntriaged
	switch {
	case err == nil:
	case errors.As(err, &u):
		fmt.Fprintln(os.Stderr, "brief:", err)
		os.Exit(2)
	default:
		fmt.Fprintln(os.Stderr, "brief:", err)
		os.Exit(1)
	}
}

type multi []string

func (m *multi) String() string     { return strings.Join(*m, ",") }
func (m *multi) Set(v string) error { *m = append(*m, v); return nil }

func repoRoot() (string, error) {
	out, err := NewGit(".").RunStr("rev-parse", "--show-toplevel")
	if err != nil {
		return "", fmt.Errorf("not inside a git repository: %w", err)
	}
	return out, nil
}

func cmdPin(args []string) error {
	fs := flag.NewFlagSet("pin", flag.ExitOnError)
	pr := fs.Int("pr", 0, "any PR of the stack")
	layers := fs.String("layers", "", "bottom-to-top PR:SHA list, instead of asking GitHub")
	trunk := fs.String("trunk", "origin/master", "the trunk ref the bottom layer's base is merged from")
	mode := fs.String("mode", string(ModeWhole), "whole or per-layer")
	repo := fs.String("repo", "", "owner/name (default: gh repo view)")
	out := fs.String("out", "", "where to write pin.json")
	fs.Parse(args)
	if *out == "" {
		return errors.New("pin: --out is required")
	}
	if Mode(*mode) != ModeWhole && Mode(*mode) != ModePerLayer {
		return fmt.Errorf("pin: --mode %q (want whole or per-layer)", *mode)
	}
	root, err := repoRoot()
	if err != nil {
		return err
	}
	g := NewGit(root)
	gh := realGH(root)
	if *repo == "" {
		b, err := gh("repo", "view", "--json", "nameWithOwner", "--jq", ".nameWithOwner")
		if err != nil {
			return err
		}
		*repo = strings.TrimSpace(string(b))
	}
	var pin *Pin
	if *layers != "" {
		var ls []Layer
		for _, p := range strings.Split(*layers, ",") {
			n, sha, ok := strings.Cut(p, ":")
			num, err := strconv.Atoi(n)
			if !ok || err != nil {
				return fmt.Errorf("pin: --layers entry %q: want PR:SHA", p)
			}
			full, err := g.RevParse(sha)
			if err != nil {
				return fmt.Errorf("pin: %s: %w", sha, err)
			}
			ls = append(ls, Layer{PR: num, Head: full, URL: fmt.Sprintf("https://github.com/%s/pull/%d", *repo, num)})
		}
		pin, err = pinLayers(g, *repo, *trunk, Mode(*mode), ls)
	} else {
		if *pr == 0 {
			return errors.New("pin: --pr or --layers is required")
		}
		pin, err = PinStack(g, gh, *repo, *pr, *trunk, Mode(*mode))
	}
	if err != nil {
		return err
	}
	return writeJSON(*out, pin)
}

func loadStack(pinPath string) (*Stack, error) {
	pin, err := LoadPin(pinPath)
	if err != nil {
		return nil, err
	}
	root, err := repoRoot()
	if err != nil {
		return nil, err
	}
	rules, err := LoadRules(filepath.Join(root, ".claude", "review"))
	if err != nil {
		return nil, err
	}
	return Analyze(NewGit(root), pin, rules)
}

func readNames(path string) ([]string, error) {
	if path == "" {
		return nil, nil
	}
	b, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	var out []string
	for _, l := range splitLines(string(b)) {
		if l = strings.TrimSpace(l); l != "" && !strings.HasPrefix(l, "#") {
			out = append(out, l)
		}
	}
	return out, nil
}

func cmdIndex(args []string) error {
	fs := flag.NewFlagSet("index", flag.ExitOnError)
	pinPath := fs.String("pin", "", "pin.json")
	specTests := fs.String("spec-tests", "", "file of test names the spec requires, one per line")
	out := fs.String("out", "", "directory for index.json and index.md")
	decPath := fs.String("decisions", "", "the decisions kept across rounds")
	fs.Parse(args)
	if *pinPath == "" || *out == "" {
		return errors.New("index: --pin and --out are required")
	}
	st, err := loadStack(*pinPath)
	if err != nil {
		return err
	}
	names, err := readNames(*specTests)
	if err != nil {
		return err
	}
	dec, err := optionalDecisions(*decPath)
	if err != nil {
		return err
	}
	ix, err := st.BuildIndex(names, dec)
	if err != nil {
		return err
	}
	if err := os.MkdirAll(*out, 0o755); err != nil {
		return err
	}
	if err := writeJSON(filepath.Join(*out, "index.json"), ix); err != nil {
		return err
	}
	f, err := os.Create(filepath.Join(*out, "index.md"))
	if err != nil {
		return err
	}
	defer f.Close()
	ix.WriteText(f)
	return nil
}

func cmdRender(args []string) error {
	fs := flag.NewFlagSet("render", flag.ExitOnError)
	pinPath := fs.String("pin", "", "pin.json")
	triagePath := fs.String("triage", "", "the decider's triage.json")
	auditPath := fs.String("audit", "", "the stack reviewer's audit.json")
	surfacePath := fs.String("surface", "", "the board's surface.json")
	specTests := fs.String("spec-tests", "", "file of test names the spec requires")
	page := fs.String("page", "", "page.html")
	out := fs.String("out", "", "output directory, replaced whole on success")
	allow := fs.Bool("allow-untriaged", false, "publish untriaged live lines as raw diff (exit 2)")
	decPath := fs.String("decisions", "", "the decisions kept across rounds")
	var scouts multi
	fs.Var(&scouts, "scout", "a scout's answer (repeatable)")
	fs.Parse(args)
	if *pinPath == "" || *triagePath == "" || *page == "" || *out == "" {
		return errors.New("render: --pin, --triage, --page and --out are required")
	}
	st, err := loadStack(*pinPath)
	if err != nil {
		return err
	}
	in := RenderInput{Triage: &Triage{}, AllowUntriaged: *allow}
	if err := readJSON(*triagePath, in.Triage); err != nil {
		return err
	}
	if *auditPath != "" {
		in.Audit = &Audit{}
		if err := readJSON(*auditPath, in.Audit); err != nil {
			return err
		}
	}
	if *surfacePath != "" {
		in.Surface = &Surface{}
		if err := readJSON(*surfacePath, in.Surface); err != nil {
			return err
		}
	}
	for _, p := range scouts {
		var s Scout
		if err := readJSON(p, &s); err != nil {
			return err
		}
		in.Scouts = append(in.Scouts, s)
	}
	if in.SpecTests, err = readNames(*specTests); err != nil {
		return err
	}
	if in.Decisions, err = optionalDecisions(*decPath); err != nil {
		return err
	}
	return st.Render(in, *page, *out)
}

func optionalDecisions(path string) (*DecisionsFile, error) {
	if path == "" {
		return nil, nil
	}
	return loadDecisions(path)
}

func cmdDecisionsMerge(args []string) error {
	fs := flag.NewFlagSet("decisions merge", flag.ExitOnError)
	pinPath := fs.String("pin", "", "pin.json, whose stack the block must be from")
	in := fs.String("in", "", "the pasted block")
	state := fs.String("state", "", "the decisions kept across rounds, created if missing")
	fs.Parse(args)
	if *pinPath == "" || *in == "" || *state == "" {
		return errors.New("decisions merge: --pin, --in and --state are required")
	}
	pin, err := LoadPin(*pinPath)
	if err != nil {
		return err
	}
	out, err := MergeDecisionsFile(*in, *state, pin)
	if err != nil {
		return err
	}
	settled := 0
	for _, d := range out.Decisions {
		if d.Decision == decisionSettled {
			settled++
		}
	}
	fmt.Printf("%s: %d decisions (%d settled, %d change requests)\n", *state, len(out.Decisions), settled, len(out.Decisions)-settled)
	return nil
}
