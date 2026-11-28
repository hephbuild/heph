package main

import (
	"bufio"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"strings"
)

// Rules are the checked-in lists under .claude/review/: the floor
// (protected-paths), the content tripwires (tripwires) and the path classes
// (classes). They change only through a PR (I3).
type Rules struct {
	Protected []Glob
	Tripwires []Tripwire
	Classes   []ClassRule
}

// Glob is a path pattern: `*` matches within one path segment, `**` matches
// any number of segments, a trailing `/` matches everything under a directory.
type Glob struct {
	Pattern string
	re      *regexp.Regexp
}

func compileGlob(p string) (Glob, error) {
	src := p
	if strings.HasSuffix(src, "/") {
		src += "**"
	}
	var b strings.Builder
	b.WriteString("^")
	for i := 0; i < len(src); i++ {
		c := src[i]
		switch {
		case c == '*' && i+1 < len(src) && src[i+1] == '*':
			i++
			if i+1 < len(src) && src[i+1] == '/' {
				i++
				b.WriteString("(?:.*/)?")
			} else {
				b.WriteString(".*")
			}
		case c == '*':
			b.WriteString("[^/]*")
		case c == '?':
			b.WriteString("[^/]")
		default:
			b.WriteString(regexp.QuoteMeta(string(c)))
		}
	}
	b.WriteString("$")
	re, err := regexp.Compile(b.String())
	if err != nil {
		return Glob{}, fmt.Errorf("glob %q: %w", p, err)
	}
	return Glob{Pattern: p, re: re}, nil
}

func (g Glob) Match(path string) bool { return g.re.MatchString(path) }

// Tripwire is a content pattern that puts a floor under a line wherever it
// is (D10): `unsafe`, a cfg split by OS or arch, an atomic ordering, a new
// dependency.
type Tripwire struct {
	Label string
	Paths Glob // the files it applies to
	re    *regexp.Regexp
}

// ClassRule assigns a Counted class (or `harness`, which is live with a
// Glance floor) to every line under a path.
type ClassRule struct {
	Class Class
	Glob  Glob
}

// LoadRules reads the three lists from dir (normally <repo>/.claude/review).
func LoadRules(dir string) (*Rules, error) {
	r := &Rules{}
	if err := eachLine(filepath.Join(dir, "protected-paths"), func(l string) error {
		g, err := compileGlob(l)
		r.Protected = append(r.Protected, g)
		return err
	}); err != nil {
		return nil, err
	}
	if err := eachLine(filepath.Join(dir, "tripwires"), func(l string) error {
		// <label> <path glob> <regexp>
		f := strings.SplitN(l, " ", 3)
		if len(f) != 3 {
			return fmt.Errorf("tripwire %q: want `<label> <path glob> <regexp>`", l)
		}
		g, err := compileGlob(f[1])
		if err != nil {
			return err
		}
		re, err := regexp.Compile(strings.TrimSpace(f[2]))
		if err != nil {
			return fmt.Errorf("tripwire %q: %w", l, err)
		}
		r.Tripwires = append(r.Tripwires, Tripwire{Label: f[0], Paths: g, re: re})
		return nil
	}); err != nil {
		return nil, err
	}
	if err := eachLine(filepath.Join(dir, "classes"), func(l string) error {
		f := strings.Fields(l)
		if len(f) != 2 {
			return fmt.Errorf("class rule %q: want `<class> <glob>`", l)
		}
		c := Class(f[0])
		switch c {
		case ClassGenerated, ClassLockfile, ClassDocs, ClassTest, ClassHarness:
		default:
			return fmt.Errorf("class rule %q: unknown class %q", l, f[0])
		}
		g, err := compileGlob(f[1])
		r.Classes = append(r.Classes, ClassRule{Class: c, Glob: g})
		return err
	}); err != nil {
		return nil, err
	}
	return r, nil
}

func eachLine(path string, fn func(string) error) error {
	f, err := os.Open(path)
	if err != nil {
		return fmt.Errorf("reading rules: %w", err)
	}
	defer f.Close()
	s := bufio.NewScanner(f)
	for s.Scan() {
		l := strings.TrimSpace(s.Text())
		if l == "" || strings.HasPrefix(l, "#") {
			continue
		}
		if err := fn(l); err != nil {
			return fmt.Errorf("%s: %w", path, err)
		}
	}
	return s.Err()
}

func (r *Rules) IsProtected(path string) bool {
	for _, g := range r.Protected {
		if g.Match(path) {
			return true
		}
	}
	return false
}

// PathClass is the first class rule matching the path, or "".
func (r *Rules) PathClass(path string) Class {
	// A harness rule wins over every other: test support is read as code.
	for _, c := range r.Classes {
		if c.Class == ClassHarness && c.Glob.Match(path) {
			return ClassHarness
		}
	}
	for _, c := range r.Classes {
		if c.Glob.Match(path) {
			return c.Class
		}
	}
	return ""
}

// Tripped returns the labels of the tripwires a line sets off.
func (r *Rules) Tripped(path, text string) []string {
	var out []string
	for _, t := range r.Tripwires {
		if t.Paths.Match(path) && t.re.MatchString(text) {
			out = append(out, t.Label)
		}
	}
	return out
}
