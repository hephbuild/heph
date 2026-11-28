package main

import (
	"fmt"
	"strconv"
	"strings"
	"unicode/utf8"
)

// FileChange is one file of a diff: the raw entry (status, modes, exact
// paths) zipped with its patch. Paths come from `--raw -z`, never from the
// patch header, so a path with a space or a non-ASCII byte is read exactly.
type FileChange struct {
	Status  byte   // A, D, M, R, T
	OldPath string // "" when added
	NewPath string // "" when deleted
	OldMode string
	NewMode string
	Binary  bool // git printed no text hunks for it
	NonUTF8 bool // a hunk line is not valid UTF-8
	Hunks   []Hunk
}

// Path is the file's path on the side that exists, preferring the new side.
func (f *FileChange) Path() string {
	if f.NewPath != "" {
		return f.NewPath
	}
	return f.OldPath
}

// Symlink reports whether either side is a symbolic link.
func (f *FileChange) Symlink() bool { return f.OldMode == "120000" || f.NewMode == "120000" }

// Submodule reports whether either side is a gitlink.
func (f *FileChange) Submodule() bool { return f.OldMode == "160000" || f.NewMode == "160000" }

// ModeOnly reports a change with no line in it (a mode flip, a pure rename).
func (f *FileChange) ModeOnly() bool {
	if f.Binary {
		return false
	}
	for _, h := range f.Hunks {
		for _, l := range h.Lines {
			if l.Kind != ' ' {
				return false
			}
		}
	}
	return true
}

// Hunk is one `@@` block.
type Hunk struct {
	OldStart, OldCount int
	NewStart, NewCount int
	Lines              []HunkLine
}

// HunkLine is one line of a hunk with its line number on each side it
// exists on (0 on the side it does not). At anchors a line on the side it
// does not exist on: for an added line, the old line it was inserted
// before; for a removed line, the new line it was removed before.
type HunkLine struct {
	Kind  byte // ' ', '+', '-'
	Old   int
	New   int
	At    int
	Text  string
	NoEOL bool
}

// Diff takes the diff between two revisions with `context` lines of context.
func (g *Git) Diff(base, head string, context int) ([]*FileChange, error) {
	raw, err := g.Run(append(append([]string{"diff", "--raw", "-z", "--no-abbrev"}, diffArgs...), base, head)...)
	if err != nil {
		return nil, err
	}
	entries, err := parseRaw(string(raw))
	if err != nil {
		return nil, err
	}
	patch, err := g.Run(append(append([]string{"diff", "-p", fmt.Sprintf("-U%d", context)}, diffArgs...), base, head)...)
	if err != nil {
		return nil, err
	}
	chunks := splitPatch(string(patch))
	// A type change (file <-> symlink) is one raw entry and two patches:
	// the old side deleted, the new side created.
	ci := 0
	for _, e := range entries {
		n := 1
		if e.Status == 'T' {
			n = 2
		}
		if ci+n > len(chunks) {
			return nil, fmt.Errorf("diff %s..%s: %d raw entries but %d patches", base, head, len(entries), len(chunks))
		}
		for _, c := range chunks[ci : ci+n] {
			if err := parseChunk(c, e); err != nil {
				return nil, fmt.Errorf("diff %s..%s, %s: %w", base, head, e.Path(), err)
			}
		}
		ci += n
	}
	if ci != len(chunks) {
		return nil, fmt.Errorf("diff %s..%s: %d raw entries but %d patches", base, head, len(entries), len(chunks))
	}
	return entries, nil
}

// parseRaw reads `git diff --raw -z` output.
func parseRaw(s string) ([]*FileChange, error) {
	parts := strings.Split(s, "\x00")
	var out []*FileChange
	for i := 0; i < len(parts); {
		head := parts[i]
		if head == "" {
			i++
			continue
		}
		if !strings.HasPrefix(head, ":") {
			return nil, fmt.Errorf("raw diff: unexpected %q", head)
		}
		f := strings.Fields(head[1:])
		if len(f) != 5 {
			return nil, fmt.Errorf("raw diff: malformed %q", head)
		}
		fc := &FileChange{OldMode: f[0], NewMode: f[1], Status: f[4][0]}
		i++
		switch fc.Status {
		case 'R', 'C':
			if i+1 >= len(parts) {
				return nil, fmt.Errorf("raw diff: truncated rename")
			}
			fc.OldPath, fc.NewPath = parts[i], parts[i+1]
			i += 2
			if fc.Status == 'C' {
				fc.OldPath = "" // a copy leaves its source in place
				fc.Status = 'A'
			}
		default:
			if i >= len(parts) {
				return nil, fmt.Errorf("raw diff: truncated entry")
			}
			p := parts[i]
			i++
			switch fc.Status {
			case 'A':
				fc.NewPath = p
			case 'D':
				fc.OldPath = p
			default:
				fc.OldPath, fc.NewPath = p, p
			}
		}
		out = append(out, fc)
	}
	return out, nil
}

// splitPatch splits `git diff -p` output into one string per file.
func splitPatch(s string) []string {
	var chunks []string
	start := -1
	for i := 0; i < len(s); {
		j := strings.IndexByte(s[i:], '\n')
		end := len(s)
		if j >= 0 {
			end = i + j + 1
		}
		if strings.HasPrefix(s[i:end], "diff --git ") {
			if start >= 0 {
				chunks = append(chunks, s[start:i])
			}
			start = i
		}
		i = end
	}
	if start >= 0 {
		chunks = append(chunks, s[start:])
	}
	return chunks
}

func parseChunk(chunk string, fc *FileChange) error {
	lines := splitLines(chunk)
	i := 0
	for ; i < len(lines); i++ {
		l := lines[i]
		if strings.HasPrefix(l, "@@") {
			break
		}
		if strings.HasPrefix(l, "Binary files ") || l == "GIT binary patch" {
			fc.Binary = true
		}
	}
	for i < len(lines) {
		h, n, err := parseHunk(lines[i:])
		if err != nil {
			return err
		}
		for _, hl := range h.Lines {
			if !utf8.ValidString(hl.Text) {
				fc.NonUTF8 = true
			}
		}
		fc.Hunks = append(fc.Hunks, h)
		i += n
	}
	return nil
}

func parseHunk(lines []string) (Hunk, int, error) {
	var h Hunk
	hdr := lines[0]
	end := strings.Index(hdr[2:], "@@")
	if end < 0 {
		return h, 0, fmt.Errorf("malformed hunk header %q", hdr)
	}
	f := strings.Fields(hdr[2 : 2+end])
	if len(f) != 2 {
		return h, 0, fmt.Errorf("malformed hunk header %q", hdr)
	}
	var err error
	if h.OldStart, h.OldCount, err = parseRange(f[0], '-'); err != nil {
		return h, 0, err
	}
	if h.NewStart, h.NewCount, err = parseRange(f[1], '+'); err != nil {
		return h, 0, err
	}
	o, n := h.OldStart, h.NewStart
	if h.OldCount == 0 {
		o++ // `-5,0` means "after line 5"
	}
	if h.NewCount == 0 {
		n++
	}
	remOld, remNew := h.OldCount, h.NewCount
	i := 1
	for ; i < len(lines) && (remOld > 0 || remNew > 0 || strings.HasPrefix(lines[i], `\`)); i++ {
		l := lines[i]
		if l == "" {
			// git never emits an empty hunk line; treat it as a blank context line.
			l = " "
		}
		switch l[0] {
		case '\\':
			if k := len(h.Lines); k > 0 {
				h.Lines[k-1].NoEOL = true
			}
		case ' ':
			h.Lines = append(h.Lines, HunkLine{Kind: ' ', Old: o, New: n, Text: l[1:]})
			o++
			n++
			remOld--
			remNew--
		case '+':
			h.Lines = append(h.Lines, HunkLine{Kind: '+', New: n, At: o, Text: l[1:]})
			n++
			remNew--
		case '-':
			h.Lines = append(h.Lines, HunkLine{Kind: '-', Old: o, At: n, Text: l[1:]})
			o++
			remOld--
		default:
			return h, 0, fmt.Errorf("unexpected hunk line %q", l)
		}
	}
	return h, i, nil
}

func parseRange(s string, sign byte) (start, count int, err error) {
	if s == "" || s[0] != sign {
		return 0, 0, fmt.Errorf("malformed hunk range %q", s)
	}
	s = s[1:]
	count = 1
	if a, b, ok := strings.Cut(s, ","); ok {
		s = a
		if count, err = strconv.Atoi(b); err != nil {
			return 0, 0, fmt.Errorf("malformed hunk range %q", s)
		}
	}
	start, err = strconv.Atoi(s)
	return start, count, err
}

// Numstat is the added and removed line count git reports per range, the
// sum every brief must reproduce (I1). Binary files count zero.
func (g *Git) Numstat(base, head string) (int, error) {
	out, err := g.Run(append(append([]string{"diff", "--numstat", "-z"}, diffArgs...), base, head)...)
	if err != nil {
		return 0, err
	}
	total := 0
	for _, rec := range strings.Split(string(out), "\x00") {
		f := strings.SplitN(rec, "\t", 3)
		if len(f) < 3 {
			continue
		}
		a, errA := strconv.Atoi(f[0])
		d, errD := strconv.Atoi(f[1])
		if errA == nil && errD == nil {
			total += a + d
		}
	}
	return total, nil
}

// LineMap maps a file's old-side line numbers to new-side ones through a
// diff: a line inside a removed run maps to nothing.
type LineMap struct {
	NewPath string
	removed map[int]bool
	hunks   []Hunk
}

// NewLineMap builds the map for one file of a diff.
func NewLineMap(fc *FileChange) *LineMap {
	m := &LineMap{NewPath: fc.NewPath, removed: map[int]bool{}, hunks: fc.Hunks}
	for _, h := range fc.Hunks {
		for _, l := range h.Lines {
			if l.Kind == '-' {
				m.removed[l.Old] = true
			}
		}
	}
	return m
}

// Map returns the new-side line for an old-side line, or false when the
// diff removed it or the file is gone. The line moves by the lines inserted
// at or before it, less the lines removed before it.
func (m *LineMap) Map(old int) (int, bool) {
	if m.NewPath == "" || m.removed[old] {
		return 0, false
	}
	delta := 0
	for _, h := range m.hunks {
		for _, l := range h.Lines {
			switch {
			case l.Kind == '+' && l.At <= old:
				delta++
			case l.Kind == '-' && l.Old < old:
				delta--
			}
		}
	}
	return old + delta, true
}
