package main

import (
	"crypto/sha256"
	"encoding/hex"
	"path"
	"regexp"
	"strings"
	"unicode"
	"unicode/utf8"
)

// The Rust scanner is a heuristic, not a parser: it lexes just enough
// (strings, raw strings, chars against lifetimes, nested comments) to find
// attributes and the extent of the item each one is on. Where it cannot
// trust its answer it says so, and the caller treats the lines as live.

type rustAttr struct {
	inner      bool
	start, end int // byte offsets of `#` and of the closing `]`
	body       string
}

type rustScan struct {
	src   string
	lines []int // byte offset of each line start
	attrs []rustAttr
}

func scanRust(src string) *rustScan {
	s := &rustScan{src: src, lines: []int{0}}
	for i := 0; i < len(src); i++ {
		if src[i] == '\n' {
			s.lines = append(s.lines, i+1)
		}
	}
	for i := 0; i < len(src); {
		if j, ok := skipTrivia(src, i); ok {
			i = j
			continue
		}
		if src[i] == '#' {
			k := i + 1
			inner := false
			if k < len(src) && src[k] == '!' {
				inner = true
				k++
			}
			k = skipSpace(src, k)
			if k < len(src) && src[k] == '[' {
				if end, ok := matchClose(src, k); ok {
					s.attrs = append(s.attrs, rustAttr{inner: inner, start: i, end: end, body: src[k+1 : end]})
					i = end + 1
					continue
				}
			}
		}
		i++
	}
	return s
}

// line returns the 1-based line of a byte offset.
func (s *rustScan) line(off int) int {
	lo, hi := 0, len(s.lines)-1
	for lo < hi {
		mid := (lo + hi + 1) / 2
		if s.lines[mid] <= off {
			lo = mid
		} else {
			hi = mid - 1
		}
	}
	return lo + 1
}

func (s *rustScan) indent(lineNo int) string {
	start := s.lines[lineNo-1]
	end := start
	for end < len(s.src) && (s.src[end] == ' ' || s.src[end] == '\t') {
		end++
	}
	return s.src[start:end]
}

func skipSpace(src string, i int) int {
	for i < len(src) && (src[i] == ' ' || src[i] == '\t' || src[i] == '\n' || src[i] == '\r') {
		i++
	}
	return i
}

// skipTrivia skips one comment, string or char literal starting at i.
func skipTrivia(src string, i int) (int, bool) {
	c := src[i]
	switch {
	case c == '/' && i+1 < len(src) && src[i+1] == '/':
		j := strings.IndexByte(src[i:], '\n')
		if j < 0 {
			return len(src), true
		}
		return i + j, true
	case c == '/' && i+1 < len(src) && src[i+1] == '*':
		depth := 0
		for j := i; j < len(src)-1; j++ {
			if src[j] == '/' && src[j+1] == '*' {
				depth++
				j++
			} else if src[j] == '*' && src[j+1] == '/' {
				depth--
				j++
				if depth == 0 {
					return j + 1, true
				}
			}
		}
		return len(src), true
	case c == '"':
		return skipString(src, i+1), true
	case (c == 'r' || c == 'b') && !identBefore(src, i):
		j := i
		if src[j] == 'b' {
			j++
			if j < len(src) && src[j] == '"' {
				return skipString(src, j+1), true
			}
			if j < len(src) && src[j] == '\'' {
				return skipChar(src, j), true
			}
		}
		if j < len(src) && src[j] == 'r' {
			k := j + 1
			hashes := 0
			for k < len(src) && src[k] == '#' {
				hashes++
				k++
			}
			if k < len(src) && src[k] == '"' {
				closer := "\"" + strings.Repeat("#", hashes)
				e := strings.Index(src[k+1:], closer)
				if e < 0 {
					return len(src), true
				}
				return k + 1 + e + len(closer), true
			}
		}
		return 0, false
	case c == '\'':
		return skipChar(src, i), true
	}
	return 0, false
}

func identBefore(src string, i int) bool {
	if i == 0 {
		return false
	}
	r, _ := utf8.DecodeLastRuneInString(src[:i])
	return r == '_' || unicode.IsLetter(r) || unicode.IsDigit(r)
}

func skipString(src string, i int) int {
	for i < len(src) {
		switch src[i] {
		case '\\':
			i += 2
		case '"':
			return i + 1
		default:
			i++
		}
	}
	return len(src)
}

// skipChar skips a char literal, or a lifetime (`'a`, `'static`), which has
// no closing quote.
func skipChar(src string, i int) int {
	j := i + 1
	if j >= len(src) {
		return j
	}
	if src[j] == '\\' {
		e := strings.IndexByte(src[j+2:], '\'')
		if e < 0 {
			return len(src)
		}
		return j + 2 + e + 1
	}
	_, size := utf8.DecodeRuneInString(src[j:])
	if j+size < len(src) && src[j+size] == '\'' {
		return j + size + 1
	}
	// A lifetime or label: skip the identifier.
	for j < len(src) {
		r, sz := utf8.DecodeRuneInString(src[j:])
		if r != '_' && !unicode.IsLetter(r) && !unicode.IsDigit(r) {
			break
		}
		j += sz
	}
	return j
}

// matchClose returns the offset of the bracket closing the one at open.
func matchClose(src string, open int) (int, bool) {
	depth := 0
	for i := open; i < len(src); {
		if j, ok := skipTrivia(src, i); ok {
			i = j
			continue
		}
		switch src[i] {
		case '(', '[', '{':
			depth++
		case ')', ']', '}':
			depth--
			if depth == 0 {
				return i, true
			}
		}
		i++
	}
	return 0, false
}

// itemEnd returns the offset of the last byte of the item starting at i: a
// `;` or `,` at depth 0 (a statement, a field, a variant, a match arm), the
// `}` closing the first `{` at depth 0, or the last byte before the
// enclosing block closes (a last field with no trailing comma).
func itemEnd(src string, i int) (int, bool) {
	depth, angle := 0, 0
	lastTok := -1
	for i < len(src) {
		if j, ok := skipTrivia(src, i); ok {
			if !strings.HasPrefix(src[i:], "//") && !strings.HasPrefix(src[i:], "/*") {
				lastTok = j - 1 // a literal is part of the item
			}
			i = j
			continue
		}
		switch src[i] {
		case '<':
			// A generic opens right after a name or `::`; `a < b` has a space.
			if i > 0 && (identBefore(src, i) || src[i-1] == ':') && (i+1 >= len(src) || (src[i+1] != '=' && src[i+1] != '<')) {
				angle++
			}
		case '>':
			if angle > 0 && i > 0 && src[i-1] != '-' && src[i-1] != '=' {
				angle--
			}
		case '(', '[':
			depth++
		case ')', ']':
			depth--
		case '{':
			if depth == 0 {
				return matchClose(src, i)
			}
			depth++
		case '}':
			if depth == 0 {
				return lastTok, lastTok >= 0
			}
			depth--
		case ';':
			if depth == 0 {
				return i, true
			}
		case ',':
			if depth == 0 && angle == 0 {
				return i, true
			}
		}
		if src[i] != ' ' && src[i] != '\t' && src[i] != '\n' && src[i] != '\r' {
			lastTok = i
		}
		i++
	}
	return 0, false
}

// afterAttrs skips whitespace, comments and further outer attributes.
func (s *rustScan) afterAttrs(i int, attrIdx int) (int, []rustAttr) {
	var stacked []rustAttr
	for {
		i = skipSpace(s.src, i)
		if i+1 < len(s.src) && s.src[i] == '/' && (s.src[i+1] == '/' || s.src[i+1] == '*') {
			j, _ := skipTrivia(s.src, i)
			i = j
			continue
		}
		if attrIdx+1 < len(s.attrs) && s.attrs[attrIdx+1].start == i && !s.attrs[attrIdx+1].inner {
			attrIdx++
			stacked = append(stacked, s.attrs[attrIdx])
			i = s.attrs[attrIdx].end + 1
			continue
		}
		return i, stacked
	}
}

// cfgTestOnly reports whether a cfg predicate holds only under test:
// `test`, `all(.., test, ..)`, `any(test, test)`. `not(test)` and
// `any(test, feature = "x")` do not: other crates link that code.
func cfgTestOnly(pred string) bool {
	pred = strings.TrimSpace(pred)
	if pred == "test" {
		return true
	}
	name, args, ok := splitCall(pred)
	if !ok {
		return false
	}
	switch name {
	case "all":
		for _, a := range args {
			if cfgTestOnly(a) {
				return true
			}
		}
	case "any":
		if len(args) == 0 {
			return false
		}
		for _, a := range args {
			if !cfgTestOnly(a) {
				return false
			}
		}
		return true
	}
	return false
}

// splitCall splits `name(a, b(c, d))` into name and top-level args.
func splitCall(s string) (string, []string, bool) {
	open := strings.IndexByte(s, '(')
	if open < 0 || !strings.HasSuffix(s, ")") {
		return "", nil, false
	}
	name := strings.TrimSpace(s[:open])
	inner := s[open+1 : len(s)-1]
	var args []string
	depth, last := 0, 0
	inStr := false
	for i := 0; i < len(inner); i++ {
		switch c := inner[i]; {
		case c == '"':
			inStr = !inStr
		case inStr:
		case c == '(':
			depth++
		case c == ')':
			depth--
		case c == ',' && depth == 0:
			args = append(args, strings.TrimSpace(inner[last:i]))
			last = i + 1
		}
	}
	if t := strings.TrimSpace(inner[last:]); t != "" {
		args = append(args, t)
	}
	return name, args, true
}

// attrCfgTest reports whether an attribute body is `cfg(<test-only>)`.
func attrCfgTest(body string) bool {
	name, args, ok := splitCall(strings.TrimSpace(body))
	return ok && name == "cfg" && len(args) == 1 && cfgTestOnly(args[0])
}

// RustTestRegions is what the scanner found in one file.
type RustTestRegions struct {
	WholeFile bool         // `#![cfg(test)]`
	Test      map[int]bool // lines inside a trusted test item
	Untrusted map[int]bool // lines of a test item whose extent is not trusted
	TestMods  []rustModDecl
}

type rustModDecl struct {
	Name string
	Path string // from #[path = "..."], or ""
}

var modDeclRe = regexp.MustCompile(`^(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;`)
var pathAttrRe = regexp.MustCompile(`^path\s*=\s*"([^"]*)"$`)

// rustTestRegions finds the lines of a file that only exist under test.
func rustTestRegions(src string) *RustTestRegions {
	s := scanRust(src)
	r := &RustTestRegions{Test: map[int]bool{}, Untrusted: map[int]bool{}}
	for idx, a := range s.attrs {
		if !attrCfgTest(a.body) {
			continue
		}
		if a.inner {
			r.WholeFile = true
			continue
		}
		itemStart, stacked := s.afterAttrs(a.end+1, idx)
		startLine := s.line(a.start)
		end, ok := itemEnd(src, itemStart)
		if !ok {
			// Runs to the end of the file (or out of its block): not trusted.
			for l := startLine; l <= len(s.lines); l++ {
				r.Untrusted[l] = true
			}
			continue
		}
		endLine := s.line(end)
		// An item that ends at another indent than its attribute is one the
		// scanner may have misread: its lines stay live, with a flag.
		trusted := endLine == startLine || s.indent(endLine) == s.indent(startLine)
		for l := startLine; l <= endLine; l++ {
			if trusted {
				r.Test[l] = true
			} else {
				r.Untrusted[l] = true
			}
		}
		if trusted && src[end] == ';' {
			if m := modDeclRe.FindStringSubmatch(src[itemStart : end+1]); m != nil {
				d := rustModDecl{Name: m[1]}
				for _, st := range stacked {
					if pm := pathAttrRe.FindStringSubmatch(strings.TrimSpace(st.body)); pm != nil {
						d.Path = pm[1]
					}
				}
				r.TestMods = append(r.TestMods, d)
			}
		}
	}
	for l := range r.Test {
		delete(r.Untrusted, l)
	}
	return r
}

// moduleParents returns the files that could declare `file` as a module,
// with the name they would declare it by.
func moduleParents(file string) (string, []string) {
	dir, base := path.Split(file)
	dir = strings.TrimSuffix(dir, "/")
	name := strings.TrimSuffix(base, ".rs")
	if name == "mod" {
		// x/name/mod.rs is declared from x/
		name = path.Base(dir)
		dir = path.Dir(dir)
	}
	if name == "lib" || name == "main" {
		return "", nil
	}
	parents := []string{
		path.Join(dir, "mod.rs"),
		dir + ".rs",
		path.Join(dir, "lib.rs"),
		path.Join(dir, "main.rs"),
	}
	return name, parents
}

// RustTests answers "is this line test code" for Rust files at one revision,
// resolving `#[cfg(test)] mod tests;` to the file it declares.
type RustTests struct {
	git      *Git
	rev      string
	regions  map[string]*RustTestRegions
	testFile map[string]bool
}

func NewRustTests(g *Git, rev string) *RustTests {
	return &RustTests{git: g, rev: rev, regions: map[string]*RustTestRegions{}, testFile: map[string]bool{}}
}

func (rt *RustTests) Regions(file string) *RustTestRegions {
	if r, ok := rt.regions[file]; ok {
		return r
	}
	lines := rt.git.Lines(rt.rev, file)
	var r *RustTestRegions
	if lines != nil {
		r = rustTestRegions(strings.Join(lines, "\n") + "\n")
	}
	rt.regions[file] = r
	return r
}

// IsTestFile reports whether a whole file is test code: `#![cfg(test)]`, or
// declared by a `#[cfg(test)] mod` (directly or through a test module).
func (rt *RustTests) IsTestFile(file string) bool {
	if v, ok := rt.testFile[file]; ok {
		return v
	}
	rt.testFile[file] = false // breaks cycles
	v := rt.isTestFile(file)
	rt.testFile[file] = v
	return v
}

func (rt *RustTests) isTestFile(file string) bool {
	if r := rt.Regions(file); r != nil && r.WholeFile {
		return true
	}
	name, parents := moduleParents(file)
	for _, p := range parents {
		r := rt.Regions(p)
		if r == nil {
			continue
		}
		for _, d := range r.TestMods {
			if d.Path != "" {
				if path.Clean(path.Join(path.Dir(p), d.Path)) == file {
					return true
				}
				continue
			}
			if d.Name == name {
				return true
			}
		}
		// A file declared by a parent that is itself test code is test code.
		if rt.declares(p, name) && rt.IsTestFile(p) {
			return true
		}
	}
	return false
}

func (rt *RustTests) declares(parent, name string) bool {
	for _, l := range rt.git.Lines(rt.rev, parent) {
		if m := modDeclRe.FindStringSubmatch(strings.TrimSpace(l)); m != nil && m[1] == name {
			return true
		}
	}
	return false
}

// IsTestLine reports whether a line is test code, and whether the scanner
// found a test region there it does not trust.
func (rt *RustTests) IsTestLine(file string, line int) (test, untrusted bool) {
	if rt.IsTestFile(file) {
		return true, false
	}
	r := rt.Regions(file)
	if r == nil {
		return false, false
	}
	return r.Test[line], r.Untrusted[line]
}

// TestFn is one test function, for the weakening checks.
type TestFn struct {
	Name       string
	Path       string
	Start, End int // lines
	Ignored    bool
	Asserts    int
	BodyHash   string
}

var assertRe = regexp.MustCompile(`\b(?:prop_)?assert\w*!|\bpanic!|\.expect_err\(|\.unwrap_err\(\)|\bshould_panic\b`)
var fnNameRe = regexp.MustCompile(`^(?:pub(?:\([^)]*\))?\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?(?:extern\s+"[^"]*"\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)`)

func isTestAttr(body string) bool {
	b := strings.TrimSpace(body)
	if i := strings.IndexAny(b, "( ="); i >= 0 {
		b = b[:i]
	}
	last := b[strings.LastIndex(b, "::")+1:]
	last = strings.TrimPrefix(last, ":")
	return last == "test" || last == "rstest" || last == "test_case"
}

// rustTestFns lists the test functions in a Rust file.
func rustTestFns(file, src string) []TestFn {
	s := scanRust(src)
	var out []TestFn
	seen := map[int]bool{}
	for idx, a := range s.attrs {
		if a.inner || seen[idx] || !isTestAttr(a.body) {
			continue
		}
		// Gather the whole attribute run around this one.
		first := idx
		for first > 0 && !s.attrs[first-1].inner && skipSpaceComments(src, s.attrs[first-1].end+1) == s.attrs[first].start {
			first--
		}
		itemStart, stacked := s.afterAttrs(s.attrs[first].end+1, first)
		all := append([]rustAttr{s.attrs[first]}, stacked...)
		ignored := false
		for k := range all {
			seen[first+k] = true
			b := strings.TrimSpace(all[k].body)
			if b == "ignore" || strings.HasPrefix(b, "ignore ") || strings.HasPrefix(b, "ignore=") || strings.HasPrefix(b, "ignore(") {
				ignored = true
			}
		}
		m := fnNameRe.FindStringSubmatch(src[itemStart:min(len(src), itemStart+300)])
		if m == nil {
			continue
		}
		end, ok := itemEnd(src, itemStart)
		if !ok {
			continue
		}
		body := src[itemStart : end+1]
		if nl := strings.IndexByte(body, '\n'); nl >= 0 {
			body = body[nl+1:] // the signature names the test; the body is the rest
		}
		out = append(out, TestFn{
			Name:     m[1],
			Path:     file,
			Start:    s.line(s.attrs[first].start),
			End:      s.line(end),
			Ignored:  ignored,
			Asserts:  len(assertRe.FindAllStringIndex(body, -1)),
			BodyHash: normHash(body),
		})
	}
	return out
}

func skipSpaceComments(src string, i int) int {
	for {
		i = skipSpace(src, i)
		if i+1 < len(src) && src[i] == '/' && (src[i+1] == '/' || src[i+1] == '*') {
			i, _ = skipTrivia(src, i)
			continue
		}
		return i
	}
}

var goTestRe = regexp.MustCompile(`^func (Test\w*)\(\w+ \*testing\.T\)`)
var goAssertRe = regexp.MustCompile(`\bt\.(?:Error|Errorf|Fatal|Fatalf|Fail|FailNow)\(|\b(?:require|assert)\.\w+\(`)

// goTestFns lists the Test functions of a Go test file: a body runs from the
// `func` line to the next `}` at column 0.
func goTestFns(file string, lines []string) []TestFn {
	var out []TestFn
	for i := 0; i < len(lines); i++ {
		m := goTestRe.FindStringSubmatch(lines[i])
		if m == nil {
			continue
		}
		j := i + 1
		for j < len(lines) && lines[j] != "}" {
			j++
		}
		body := strings.Join(lines[i+1:min(j, len(lines))], "\n")
		out = append(out, TestFn{
			Name:     m[1],
			Path:     file,
			Start:    i + 1,
			End:      min(j+1, len(lines)),
			Ignored:  strings.Contains(body, "t.Skip("),
			Asserts:  len(goAssertRe.FindAllStringIndex(body, -1)),
			BodyHash: normHash(body),
		})
		i = j
	}
	return out
}

// normHash hashes text with each line's surrounding whitespace removed.
func normHash(s string) string {
	h := sha256.New()
	for _, l := range strings.Split(s, "\n") {
		h.Write([]byte(strings.TrimSpace(l)))
		h.Write([]byte{'\n'})
	}
	return hex.EncodeToString(h.Sum(nil))[:16]
}

var symbolRe = regexp.MustCompile(`^(\s*)(?:pub(?:\([^)]*\))?\s+)?(?:const\s+|async\s+|unsafe\s+)*(fn|impl|struct|enum|trait|mod|func|type)\b\s*(?:<[^>]*>\s*)?([^\s({<;]*)`)

// enclosingSymbol names the nearest declaration above a line whose indent is
// at most the line's: good enough to say "the same function" across layers.
func enclosingSymbol(lines []string, line int) string {
	if line < 1 || line > len(lines) {
		return ""
	}
	target := lines[line-1]
	ind := len(target) - len(strings.TrimLeft(target, " \t"))
	for i := line - 1; i >= 0; i-- {
		m := symbolRe.FindStringSubmatch(lines[i])
		if m == nil {
			continue
		}
		if len(m[1]) <= ind || i == line-1 {
			name := m[3]
			if m[2] == "impl" || name == "" {
				name = strings.TrimSpace(strings.TrimSuffix(strings.TrimPrefix(lines[i], m[1]), "{"))
				if len(name) > 80 {
					name = name[:80]
				}
				return name
			}
			return m[2] + " " + name
		}
	}
	return ""
}
