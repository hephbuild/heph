package main

import (
	"bufio"
	"fmt"
	"os"
	"regexp"
	"strings"

	"golang.org/x/tools/go/analysis"
)

// suppressor decides whether a diagnostic should be dropped before it reaches
// the report, replicating the two golangci-lint features that live in its runner
// (not in the analyzers): `//nolint` directives and `linters.exclusions.rules`.
//
// It is applied by wrapping each selected analyzer's Pass.Report (see
// wrapAnalyzers): the analysis itself is unchanged, only emission is filtered.
type suppressor struct {
	excludes []excludeRule
	// nolint is the lazily-scanned per-file directive index: file -> line ->
	// set of golangci linter names ("*" = all linters).
	nolint map[string]map[int]map[string]bool
}

type excludeRule struct {
	linters map[string]bool // empty → applies to every linter
	path    *regexp.Regexp  // nil → any path
	text    *regexp.Regexp  // nil → any message
}

func newSuppressor(cfg golangciConfig) (*suppressor, error) {
	s := &suppressor{nolint: map[string]map[int]map[string]bool{}}
	for i, r := range cfg.Linters.Exclusions.Rules {
		var er excludeRule
		if len(r.Linters) > 0 {
			er.linters = map[string]bool{}
			for _, l := range r.Linters {
				er.linters[l] = true
			}
		}
		if r.Path != "" {
			re, err := regexp.Compile(r.Path)
			if err != nil {
				return nil, fmt.Errorf("exclusions.rules[%d].path: %w", i, err)
			}
			er.path = re
		}
		if r.Text != "" {
			re, err := regexp.Compile(r.Text)
			if err != nil {
				return nil, fmt.Errorf("exclusions.rules[%d].text: %w", i, err)
			}
			er.text = re
		}
		s.excludes = append(s.excludes, er)
	}
	return s, nil
}

// suppressed reports whether a finding by golangci `linter` at file:line with
// `msg` should be dropped.
func (s *suppressor) suppressed(linter, file string, line int, msg string) bool {
	for _, er := range s.excludes {
		if len(er.linters) > 0 && !er.linters[linter] {
			continue
		}
		if er.path != nil && !er.path.MatchString(file) {
			continue
		}
		if er.text != nil && !er.text.MatchString(msg) {
			continue
		}
		return true
	}
	return s.nolintCovers(linter, file, line)
}

func (s *suppressor) nolintCovers(linter, file string, line int) bool {
	byLine, ok := s.nolint[file]
	if !ok {
		byLine = scanNolint(file)
		s.nolint[file] = byLine
	}
	set, ok := byLine[line]
	if !ok {
		return false
	}
	return set["*"] || set[linter]
}

var nolintRe = regexp.MustCompile(`//\s*nolint(:\s*([\w,\s-]+))?`)

// scanNolint indexes a file's `//nolint[:l1,l2]` directives by the lines they
// cover. A trailing directive covers its own line; a directive on its own line
// also covers the next line (golangci's leading-comment form). A missing or
// unreadable file yields an empty index.
func scanNolint(file string) map[int]map[string]bool {
	out := map[int]map[string]bool{}
	f, err := os.Open(file)
	if err != nil {
		return out
	}
	defer f.Close()

	sc := bufio.NewScanner(f)
	sc.Buffer(make([]byte, 0, 64*1024), 4*1024*1024)
	lineNo := 0
	for sc.Scan() {
		lineNo++
		text := sc.Text()
		m := nolintRe.FindStringSubmatch(text)
		if m == nil {
			continue
		}
		set := map[string]bool{}
		names := strings.TrimSpace(m[2])
		if names == "" {
			set["*"] = true
		} else {
			for _, n := range strings.Split(names, ",") {
				if n = strings.TrimSpace(n); n != "" {
					set[n] = true
				}
			}
		}
		addNolint(out, lineNo, set)
		// Own-line directive (only the comment on the line) also covers the
		// following line.
		if strings.HasPrefix(strings.TrimSpace(text), "//") {
			addNolint(out, lineNo+1, set)
		}
	}
	return out
}

func addNolint(out map[int]map[string]bool, line int, set map[string]bool) {
	dst, ok := out[line]
	if !ok {
		dst = map[string]bool{}
		out[line] = dst
	}
	for k := range set {
		dst[k] = true
	}
}

// wrapAnalyzers returns the selected analyzers with their emission filtered by
// the suppressor. `linterOf` maps an analyzer to its golangci linter name (for
// `//nolint:<linter>` matching).
//
// Only analyzers that are NOT in the Requires-closure of the selection are
// wrapped: wrapping one that another analyzer requires would make both the
// original (pulled in as a dependency) and the wrapped copy run, double-emitting.
// Such analyzers (rare for leaf linters) emit unfiltered.
func wrapAnalyzers(sel []*analysis.Analyzer, linterOf map[*analysis.Analyzer]string, s *suppressor) []*analysis.Analyzer {
	required := requiresClosure(sel)
	out := make([]*analysis.Analyzer, 0, len(sel))
	for _, a := range sel {
		if required[a] {
			out = append(out, a)
			continue
		}
		out = append(out, withFilter(a, linterOf[a], s))
	}
	return out
}

func requiresClosure(sel []*analysis.Analyzer) map[*analysis.Analyzer]bool {
	seen := map[*analysis.Analyzer]bool{}
	var visit func(a *analysis.Analyzer)
	visit = func(a *analysis.Analyzer) {
		for _, r := range a.Requires {
			if !seen[r] {
				seen[r] = true
				visit(r)
			}
		}
	}
	for _, a := range sel {
		visit(a)
	}
	return seen
}

func withFilter(a *analysis.Analyzer, linter string, s *suppressor) *analysis.Analyzer {
	w := *a
	orig := a.Run
	w.Run = func(pass *analysis.Pass) (any, error) {
		shim := *pass
		shim.Report = func(d analysis.Diagnostic) {
			pos := pass.Fset.Position(d.Pos)
			if s.suppressed(linter, pos.Filename, pos.Line, d.Message) {
				return
			}
			pass.Report(d)
		}
		return orig(&shim)
	}
	return &w
}
