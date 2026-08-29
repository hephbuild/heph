package main

import (
	"bufio"
	"fmt"
	"os"
	"regexp"
	"strings"
	"sync"

	"golang.org/x/tools/go/analysis"
)

// suppressor decides whether a diagnostic should be dropped before it reaches
// the report, replicating the golangci-lint features that live in its runner
// (not in the analyzers): `//nolint` directives and the full
// `linters.exclusions` block — `generated`, `presets`, `rules`, `paths`, and
// `paths-except`.
//
// It is applied by wrapping each selected analyzer's Pass.Report (see
// wrapAnalyzers): the analysis itself is unchanged, only emission is filtered.
type suppressor struct {
	// excludes is the union of `exclusions.rules` and the expanded `presets`.
	excludes []excludeRule
	// paths / pathsExcept are whole-file filters: a file matching `paths` is
	// dropped entirely; when `pathsExcept` is non-empty, a file matching NONE of
	// them is dropped.
	paths       []*regexp.Regexp
	pathsExcept []*regexp.Regexp
	// generated is "lax" (default), "strict", or "disable".
	generated string
	// mu guards the two lazily-filled caches below. unitchecker runs the
	// selected analyzers concurrently, and every one of them reports through
	// this single shared suppressor (see withFilter), so both maps are read and
	// written from several goroutines at once.
	//
	// Unsynchronized, this is not a subtle corruption: Go aborts the process
	// with "fatal error: concurrent map writes", which is unrecoverable and
	// takes the whole lint target down with it. It stayed hidden while the
	// default analyzer set was small — widening it to `go vet`'s full suite
	// made two analyzers report at the same instant often enough to hit it.
	mu sync.Mutex
	// nolint is the lazily-scanned per-file directive index: file -> line ->
	// set of golangci linter names ("*" = all linters).
	nolint map[string]map[int]map[string]bool
	// genCache memoizes the per-file generated-detection result.
	genCache map[string]bool
}

type excludeRule struct {
	linters    map[string]bool // empty → applies to every linter
	path       *regexp.Regexp  // nil → any path
	pathExcept *regexp.Regexp  // non-nil → rule skipped when the path matches
	text       *regexp.Regexp  // nil → any message
}

func newSuppressor(cfg golangciConfig) (*suppressor, error) {
	ex := cfg.Linters.Exclusions
	s := &suppressor{
		nolint:    map[string]map[int]map[string]bool{},
		genCache:  map[string]bool{},
		generated: normalizeGenerated(ex.Generated),
	}

	// Explicit rules first, then the expansion of any enabled presets.
	for i, r := range ex.Rules {
		er, err := compileExcludeRule(r.Linters, r.Path, r.PathExcept, r.Text)
		if err != nil {
			return nil, fmt.Errorf("exclusions.rules[%d]: %w", i, err)
		}
		s.excludes = append(s.excludes, er)
	}
	presetRules, err := expandPresets(ex.Presets)
	if err != nil {
		return nil, err
	}
	s.excludes = append(s.excludes, presetRules...)

	for i, p := range ex.Paths {
		re, err := regexp.Compile(p)
		if err != nil {
			return nil, fmt.Errorf("exclusions.paths[%d]: %w", i, err)
		}
		s.paths = append(s.paths, re)
	}
	for i, p := range ex.PathsExcept {
		re, err := regexp.Compile(p)
		if err != nil {
			return nil, fmt.Errorf("exclusions.paths-except[%d]: %w", i, err)
		}
		s.pathsExcept = append(s.pathsExcept, re)
	}
	return s, nil
}

// normalizeGenerated maps the config value to a mode, defaulting empty to "lax"
// (golangci-lint's default: generated files are excluded unless opted out).
func normalizeGenerated(v string) string {
	switch v {
	case "strict", "disable":
		return v
	default:
		return "lax"
	}
}

// compileExcludeRule builds an excludeRule from its raw config strings.
func compileExcludeRule(linters []string, path, pathExcept, text string) (excludeRule, error) {
	var er excludeRule
	if len(linters) > 0 {
		er.linters = map[string]bool{}
		for _, l := range linters {
			er.linters[nolintLinterName(l)] = true
		}
	}
	if path != "" {
		re, err := regexp.Compile(path)
		if err != nil {
			return er, fmt.Errorf("path: %w", err)
		}
		er.path = re
	}
	if pathExcept != "" {
		re, err := regexp.Compile(pathExcept)
		if err != nil {
			return er, fmt.Errorf("path-except: %w", err)
		}
		er.pathExcept = re
	}
	if text != "" {
		re, err := regexp.Compile(text)
		if err != nil {
			return er, fmt.Errorf("text: %w", err)
		}
		er.text = re
	}
	return er, nil
}

// expandPresets turns the enabled preset names into exclude rules. An unknown
// preset name is an error (fail loudly, per heph's "fail or fix" rule).
func expandPresets(presets []string) ([]excludeRule, error) {
	if len(presets) == 0 {
		return nil, nil
	}
	enabled := map[string]bool{}
	for _, p := range presets {
		if !knownPresets[p] {
			return nil, fmt.Errorf("exclusions.presets: unknown preset %q "+
				"(known: comments, common-false-positives, legacy, std-error-handling)", p)
		}
		enabled[p] = true
	}
	var out []excludeRule
	for _, pp := range presetPatterns {
		if !enabled[pp.preset] {
			continue
		}
		er, err := compileExcludeRule(pp.linters, "", "", pp.text)
		if err != nil {
			return nil, fmt.Errorf("preset %s (%s): %w", pp.preset, pp.id, err)
		}
		out = append(out, er)
	}
	return out, nil
}

// suppressed reports whether a finding by golangci `linter` at file:line with
// `msg` should be dropped.
func (s *suppressor) suppressed(linter, file string, line int, msg string) bool {
	// Whole-file filters first — cheapest, and they short-circuit everything.
	if s.generated != "disable" && s.isGenerated(file) {
		return true
	}
	for _, re := range s.paths {
		if re.MatchString(file) {
			return true
		}
	}
	if len(s.pathsExcept) > 0 {
		matched := false
		for _, re := range s.pathsExcept {
			if re.MatchString(file) {
				matched = true
				break
			}
		}
		if !matched {
			return true
		}
	}

	for _, er := range s.excludes {
		if len(er.linters) > 0 && !er.linters[linter] {
			continue
		}
		if er.path != nil && !er.path.MatchString(file) {
			continue
		}
		if er.pathExcept != nil && er.pathExcept.MatchString(file) {
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
	s.mu.Lock()
	byLine, ok := s.nolint[file]
	if !ok {
		// Held across the scan on purpose: the miss happens once per file, and
		// serializing it means concurrent reporters on the same file wait for
		// one scan rather than each running their own.
		byLine = scanNolint(file)
		s.nolint[file] = byLine
	}
	s.mu.Unlock()
	set, ok := byLine[line]
	if !ok {
		return false
	}
	return set["*"] || set[linter]
}

// strictGeneratedRe is the Go convention for a generated-file marker
// (https://go.dev/s/generatedcode): a line comment of exactly this form.
var strictGeneratedRe = regexp.MustCompile(`^//\s*Code generated .* DO NOT EDIT\.$`)

// laxGeneratedMarkers are the case-insensitive header substrings golangci-lint's
// "lax" mode also treats as marking a generated file.
var laxGeneratedMarkers = []string{
	"autogenerated by",
	"automatically generated",
	"code generated",
	"do not edit",
	"@generated",
	"generated by",
	"generated file",
	"this file was generated",
}

// isGenerated reports whether `file` is a generated file under the configured
// mode, memoized per file. "strict" honors only the Go-convention marker; "lax"
// (the default) also accepts the looser header substrings golangci uses.
func (s *suppressor) isGenerated(file string) bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	if g, ok := s.genCache[file]; ok {
		return g
	}
	g := detectGenerated(file, s.generated)
	s.genCache[file] = g
	return g
}

// generatedScanLines bounds how far into a file we look for a generated marker.
// Generators put it in the header, but not always above the `package` clause
// (some tools emit `package foo` first, then the `// Code generated … DO NOT
// EDIT` comment). golangci scans the file, not just the pre-package lines, so we
// scan a generous prefix rather than stopping at `package` — while capping the
// work on large hand-written files that will never carry a marker.
const generatedScanLines = 200

// detectGenerated scans a file's leading lines for a generated-file marker. The
// marker may sit above or just below the `package` clause, so the scan does not
// stop at `package`. "strict" honors only the Go-convention `// Code generated
// … DO NOT EDIT.` line; "lax" also accepts golangci's looser comment substrings.
func detectGenerated(file, mode string) bool {
	f, err := os.Open(file)
	if err != nil {
		return false
	}
	defer f.Close()

	sc := bufio.NewScanner(f)
	sc.Buffer(make([]byte, 0, 64*1024), 4*1024*1024)
	for line := 0; line < generatedScanLines && sc.Scan(); line++ {
		trimmed := strings.TrimSpace(sc.Text())
		if strictGeneratedRe.MatchString(trimmed) {
			return true
		}
		if mode == "lax" && strings.HasPrefix(trimmed, "//") {
			lower := strings.ToLower(trimmed)
			for _, m := range laxGeneratedMarkers {
				if strings.Contains(lower, m) {
					return true
				}
			}
		}
	}
	return false
}

// nolintRe matches a `//nolint` directive comment. The `\b` after `nolint` is
// load-bearing: without it, `//nolintlint`, `//nolinter`, etc. would match the
// `nolint` prefix with an empty linter group and be treated as a bare
// `//nolint` — silently suppressing *every* linter on that line. The boundary
// requires `nolint` to be followed by `:`, whitespace, or end-of-comment, as
// golangci-lint does.
var nolintRe = regexp.MustCompile(`//\s*nolint\b(:\s*([\w,\s-]+))?`)

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
				n = strings.TrimSpace(n)
				switch {
				case n == "":
					// skip empty entries (e.g. trailing comma)
				case n == "all":
					// golangci's `//nolint:all` is the wildcard — same as bare
					// `//nolint`, it suppresses every linter on the line.
					set["*"] = true
				default:
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
			if s.suppressed(linter, pos.Filename, pos.Line, matchText(linter, a.Name, d.Message)) {
				return
			}
			pass.Report(d)
		}
		return orig(&shim)
	}
	return &w
}

// matchText renders the message form that `exclusions.rules[].text` (and preset
// patterns) are matched against, mirroring golangci-lint. golangci prefixes
// honnef (staticcheck) messages with the check code — `SA1019: <message>` — and
// matches text rules against that form, so a golangci config's rule like
// `text: "SA1019: nrn.ResourceSite is deprecated"` works verbatim here. go vet
// messages are matched unprefixed, as golangci does.
func matchText(linter, analyzerName, message string) string {
	if linter == "staticcheck" {
		return analyzerName + ": " + message
	}
	return message
}
