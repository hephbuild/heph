package main

import (
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"regexp"
	"strings"
	"unicode/utf8"
)

// What a card tells a reviewer who has not read the code: a title saying
// what changed, context saying what the code is for, what reaches it and how
// it shows up, the options the question offers with what each costs, and
// optionally examples, a diagram and the sites that use the code. The
// program only checks their shape; what they say is the models' (I2).

const (
	titleCap     = 90
	contextCap   = 600
	questionCap  = 300
	choiceCap    = 80
	impactCap    = 300
	minOptions   = 2
	maxOptions   = 4
	maxExamples  = 3
	labelCap     = 40
	exampleLines = 30
	diagramLines = 40
	maxUsedBy    = 3
	usedByLines  = 40
	// snippetMargin is how many lines around a used_by range its snippet shows.
	snippetMargin = 3
)

// bareIDRe matches a token shaped like a spec or decision id: U28, P20, D4,
// I11. The reviewer decides from the title, the question and the options,
// so those say what a rule says, not its number.
var bareIDRe = regexp.MustCompile(`\b[A-Z][0-9]{1,3}\b`)

// notSpecIDs are product names that share the shape.
var notSpecIDs = map[string]bool{"S3": true, "R2": true, "V8": true, "L1": true, "L2": true, "L3": true}

func bareIDs(s string) []string {
	seen := map[string]bool{}
	var out []string
	for _, m := range bareIDRe.FindAllString(s, -1) {
		if notSpecIDs[m] || seen[m] {
			continue
		}
		seen[m] = true
		out = append(out, m)
	}
	return out
}

func runes(s string) int { return utf8.RuneCountInString(s) }

// textLines counts the lines of a block of text, a final newline not
// starting another.
func textLines(s string) int {
	s = strings.TrimRight(s, "\n")
	if s == "" {
		return 0
	}
	return strings.Count(s, "\n") + 1
}

var diagramKinds = map[string]bool{"flowchart": true, "graph": true, "sequenceDiagram": true, "stateDiagram": true, "stateDiagram-v2": true}

// diagramProblems lists why render refuses a Mermaid source. The subset is
// small on purpose: the page hands it to mermaid, whose SVG is the one
// generated markup the page inserts, so nothing in it may link, script or
// reconfigure the renderer.
func diagramProblems(src string) []string {
	var out []string
	if n := textLines(src); n > diagramLines {
		out = append(out, fmt.Sprintf("is %d lines, over the cap of %d", n, diagramLines))
	}
	kind := ""
	for _, l := range strings.Split(src, "\n") {
		t := strings.TrimSpace(l)
		if t == "" || (strings.HasPrefix(t, "%%") && !strings.HasPrefix(t, "%%{")) {
			continue
		}
		first := strings.Fields(t)[0]
		if kind == "" {
			kind = first
		}
		switch first {
		case "click", "link", "links":
			out = append(out, fmt.Sprintf("has a %q line: a diagram may not link anywhere", first))
		}
	}
	if !diagramKinds[kind] {
		out = append(out, fmt.Sprintf("starts with %q: it must start with flowchart, graph, sequenceDiagram or stateDiagram(-v2)", kind))
	}
	if strings.Contains(src, "%%{") {
		out = append(out, "has a %%{ directive: the renderer's configuration is the page's")
	}
	if strings.Contains(src, "<") {
		out = append(out, "has a '<': no markup in a diagram")
	}
	if strings.Contains(strings.ToLower(src), "javascript:") {
		out = append(out, "has a javascript: URL")
	}
	return out
}

// checkCard applies the card-text refusals to one item of layer d.
func (st *Stack) checkCard(d *LayerData, it *Item, where string, ref *Refusals) {
	if it.Tier == TierScrutinize {
		if strings.TrimSpace(it.Title) == "" {
			ref.add("%s: a Scrutinize card needs a title: what changed, in plain words", where)
		}
		if strings.TrimSpace(it.Context) == "" {
			ref.add("%s: a Scrutinize card needs context: what the code is for, what reaches it and how it shows up to whoever meets it", where)
		}
		if len(it.Options) == 0 {
			ref.add("%s: a Scrutinize card needs options: %d to %d answers to its question, each with its impact", where, minOptions, maxOptions)
		} else {
			keeps, changes := false, false
			for _, o := range it.Options {
				keeps = keeps || !o.Change
				changes = changes || o.Change
			}
			if !keeps {
				ref.add("%s: no option has change: false: a Scrutinize card offers at least one answer that keeps the code as it is, so \"it's fine\" is one click", where)
			}
			if !changes {
				ref.add("%s: no option has change: true: a Scrutinize card offers at least one answer that asks for a code change", where)
			}
		}
	}
	capped := func(field, s string, max int) {
		if n := runes(s); n > max {
			ref.add("%s: %s is %d characters, over the cap of %d", where, field, n, max)
		}
	}
	noIDs := func(field, s string) {
		if ids := bareIDs(s); len(ids) > 0 {
			ref.add("%s: %s names %s: the reviewer cannot look up a spec or decision id, so say what it says", where, field, strings.Join(ids, ", "))
		}
	}
	capped("title", it.Title, titleCap)
	capped("context", it.Context, contextCap)
	capped("question", it.Question, questionCap)
	noIDs("title", it.Title)
	noIDs("question", it.Question)
	if n := len(it.Options); n > 0 && (n < minOptions || n > maxOptions) {
		ref.add("%s: %d options: a question offers %d to %d", where, n, minOptions, maxOptions)
	}
	for i, o := range it.Options {
		f := fmt.Sprintf("option %d", i+1)
		if strings.TrimSpace(o.Choice) == "" || strings.TrimSpace(o.Impact) == "" {
			ref.add("%s: %s needs a choice and its impact", where, f)
		}
		capped(f+"'s choice", o.Choice, choiceCap)
		capped(f+"'s impact", o.Impact, impactCap)
		noIDs(f+"'s choice", o.Choice)
		noIDs(f+"'s impact", o.Impact)
	}
	if n := len(it.Examples); n > maxExamples {
		ref.add("%s: %d examples, over the cap of %d", where, n, maxExamples)
	}
	for i, e := range it.Examples {
		f := fmt.Sprintf("example %d", i+1)
		if strings.TrimSpace(e.Label) == "" || strings.TrimSpace(e.Text) == "" {
			ref.add("%s: %s needs a label and text", where, f)
		}
		capped(f+"'s label", e.Label, labelCap)
		if n := textLines(e.Text); n > exampleLines {
			ref.add("%s: %s is %d lines, over the cap of %d", where, f, n, exampleLines)
		}
	}
	if it.Diagram != "" {
		for _, p := range diagramProblems(it.Diagram) {
			ref.add("%s: the diagram %s", where, p)
		}
	}
	if n := len(it.UsedBy); n > maxUsedBy {
		ref.add("%s: %d used_by sites, over the cap of %d", where, n, maxUsedBy)
	}
	for _, u := range it.UsedBy {
		st.checkUse(d, u, where, ref)
	}
}

// checkUse refuses a used_by range that is not a short range of a file at
// the layer's head. It need not be in the diff.
func (st *Stack) checkUse(d *LayerData, u RangeRef, where string, ref *Refusals) {
	w := fmt.Sprintf("%s: used_by %s:%d-%d", where, u.Path, u.Start, u.End)
	if u.side() != SideNew {
		ref.add("%s is on the %s side: a use site is read at the layer's head", w, u.side())
		return
	}
	if strings.TrimSpace(u.Note) == "" {
		ref.add("%s has no note saying how it uses the change", w)
	}
	if u.Start < 1 || u.End < u.Start {
		ref.add("%s is empty or inverted", w)
		return
	}
	if n := u.End - u.Start + 1; n > usedByLines {
		ref.add("%s is %d lines, over the cap of %d: cite the lines that use the change", w, n, usedByLines)
	}
	src := st.git.Lines(d.Head, u.Path)
	if src == nil {
		ref.add("%s: %s does not exist at #%d's head %s", w, u.Path, d.PR, short(d.Head))
		return
	}
	if u.End > len(src) {
		ref.add("%s runs past the end of %s (%d lines at #%d's head %s)", w, u.Path, len(src), d.PR, short(d.Head))
	}
}

// snippet cuts a used_by range out of the file at the layer's head, with
// snippetMargin lines around it: [">" or " ", 0, line, text], ">" marking
// the cited lines.
func snippet(src []string, u RangeRef) []DiffLine {
	from, to := max(1, u.Start-snippetMargin), min(len(src), u.End+snippetMargin)
	out := make([]DiffLine, 0, to-from+1)
	for i := from; i <= to; i++ {
		k := " "
		if u.Start <= i && i <= u.End {
			k = ">"
		}
		out = append(out, DiffLine{k, 0, i, src[i-1]})
	}
	return out
}

// snippetKey is a snippet's key in the diffs file: a hash of where it is and
// what it holds, so the same site cited by two cards is stored once.
func snippetKey(path string, lines []DiffLine) string {
	var b strings.Builder
	for _, l := range lines {
		fmt.Fprintf(&b, "%s%d\t%s\n", l[0], l[2], l[3])
	}
	sum := sha256.Sum256([]byte(path + "\x00" + b.String()))
	return "use-" + hex.EncodeToString(sum[:])[:20]
}
