package main

import (
	"bytes"
	"fmt"
	"os"
	"os/exec"
	"strings"
	"sync"
)

// Git runs git in one repository with the user's configuration switched
// off: a diff that depends on someone's diff.algorithm, diff.noprefix,
// diff.external or textconv would cut different ranges on different machines,
// and the brief's line numbers would stop matching GitHub's.
type Git struct {
	Dir string

	mu    sync.Mutex
	files map[string][]string // "<rev>:<path>" -> lines, nil when absent
}

func NewGit(dir string) *Git {
	return &Git{Dir: dir, files: map[string][]string{}}
}

// diffArgs are the options every diff the program takes shares, so a range
// cut for a card and the range the classifier counted are the same diff.
var diffArgs = []string{
	"--no-color", "--no-ext-diff", "--no-textconv", "--no-relative",
	"--diff-algorithm=myers", "--find-renames=50%", "--ignore-submodules=none",
	"--src-prefix=a/", "--dst-prefix=b/",
}

func (g *Git) cmd(args ...string) *exec.Cmd {
	full := append([]string{
		"-c", "core.quotePath=false",
		"-c", "diff.noprefix=false",
		"-c", "diff.mnemonicPrefix=false",
		"-c", "color.ui=false",
		"-c", "core.pager=cat",
	}, args...)
	c := exec.Command("git", full...)
	c.Dir = g.Dir
	c.Env = append(os.Environ(),
		"GIT_CONFIG_NOSYSTEM=1",
		"GIT_CONFIG_GLOBAL="+os.DevNull,
		"GIT_PAGER=cat",
		"LC_ALL=C",
	)
	return c
}

// Run returns git's stdout, or an error carrying its stderr.
func (g *Git) Run(args ...string) ([]byte, error) {
	c := g.cmd(args...)
	var stdout, stderr bytes.Buffer
	c.Stdout = &stdout
	c.Stderr = &stderr
	if err := c.Run(); err != nil {
		return nil, fmt.Errorf("git %s: %w: %s", strings.Join(args, " "), err, strings.TrimSpace(stderr.String()))
	}
	return stdout.Bytes(), nil
}

// RunStr is Run with the trailing newline trimmed.
func (g *Git) RunStr(args ...string) (string, error) {
	out, err := g.Run(args...)
	return strings.TrimSpace(string(out)), err
}

// RevParse resolves a revision to a full commit SHA.
func (g *Git) RevParse(rev string) (string, error) {
	return g.RunStr("rev-parse", "--verify", "--quiet", rev+"^{commit}")
}

// HasCommit reports whether the object exists locally.
func (g *Git) HasCommit(sha string) bool {
	_, err := g.Run("cat-file", "-e", sha+"^{commit}")
	return err == nil
}

// MergeBase is the merge-base of two revisions.
func (g *Git) MergeBase(a, b string) (string, error) {
	return g.RunStr("merge-base", a, b)
}

// IsAncestor reports whether a is an ancestor of b.
func (g *Git) IsAncestor(a, b string) bool {
	_, err := g.Run("merge-base", "--is-ancestor", a, b)
	return err == nil
}

// Lines returns a file's lines at a revision, without their newlines, or nil
// when the path does not exist there. Results are cached: classification
// reads the same file at the same revision several times.
func (g *Git) Lines(rev, path string) []string {
	key := rev + ":" + path
	g.mu.Lock()
	if l, ok := g.files[key]; ok {
		g.mu.Unlock()
		return l
	}
	g.mu.Unlock()
	out, err := g.Run("cat-file", "blob", key)
	var lines []string
	if err == nil {
		lines = splitLines(string(out))
		if lines == nil {
			lines = []string{}
		}
	}
	g.mu.Lock()
	g.files[key] = lines
	g.mu.Unlock()
	return lines
}

// splitLines splits on '\n' and drops the empty element after a final newline.
func splitLines(s string) []string {
	if s == "" {
		return nil
	}
	l := strings.Split(s, "\n")
	if l[len(l)-1] == "" {
		l = l[:len(l)-1]
	}
	return l
}
