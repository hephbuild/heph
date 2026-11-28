package main

import (
	"fmt"
	"regexp"
	"sort"
	"strings"
)

var porcelainHdr = regexp.MustCompile(`^([0-9a-f]{40}) \d+ \d+`)

// lineArgs turns line numbers into `-L a,b` arguments, one per run.
func lineArgs(lines []int) []string {
	sort.Ints(lines)
	var args []string
	for i := 0; i < len(lines); {
		j := i
		for j+1 < len(lines) && lines[j+1] == lines[j]+1 {
			j++
		}
		args = append(args, "-L", fmt.Sprintf("%d,%d", lines[i], lines[j]))
		i = j + 1
	}
	return args
}

// blameSHAs runs a porcelain blame and returns the commits it names,
// leaving out boundary commits (those at or below the range's base).
func (g *Git) blameSHAs(args []string) ([]string, error) {
	out, err := g.Run(args...)
	if err != nil {
		return nil, err
	}
	boundary := map[string]bool{}
	var order []string
	seen := map[string]bool{}
	cur := ""
	for _, l := range splitLines(string(out)) {
		if strings.HasPrefix(l, "\t") {
			continue
		}
		if m := porcelainHdr.FindStringSubmatch(l); m != nil {
			cur = m[1]
			if !seen[cur] {
				seen[cur] = true
				order = append(order, cur)
			}
			continue
		}
		if l == "boundary" {
			boundary[cur] = true
		}
	}
	var shas []string
	for _, s := range order {
		if !boundary[s] {
			shas = append(shas, s)
		}
	}
	return shas, nil
}

// CommitsAdding returns the commits in base..head that last touched the
// given new-side lines (D4).
func (g *Git) CommitsAdding(base, head, path string, lines []int) ([]string, error) {
	if len(lines) == 0 {
		return nil, nil
	}
	args := append([]string{"blame", "--porcelain"}, lineArgs(lines)...)
	args = append(args, base+".."+head, "--", path)
	return g.blameSHAs(args)
}

// CommitsRemoving returns the commits in base..head that deleted the given
// old-side lines. `git blame` at the head cannot see a deleted line, so a
// reverse blame finds the last commit each line existed in, and the commit
// after it on the way to head is the one that removed it.
func (g *Git) CommitsRemoving(base, head, path string, lines []int) ([]string, error) {
	if len(lines) == 0 {
		return nil, nil
	}
	args := append([]string{"blame", "--porcelain", "--reverse"}, lineArgs(lines)...)
	args = append(args, base+".."+head, "--", path)
	out, err := g.Run(args...)
	if err != nil {
		return nil, err
	}
	var lastSeen []string
	seen := map[string]bool{}
	for _, l := range splitLines(string(out)) {
		if m := porcelainHdr.FindStringSubmatch(l); m != nil && !seen[m[1]] {
			seen[m[1]] = true
			lastSeen = append(lastSeen, m[1])
		}
	}
	var shas []string
	for _, c := range lastSeen {
		next, err := g.RunStr("rev-list", "--reverse", "--ancestry-path", c+".."+head)
		if err != nil {
			return nil, err
		}
		if first, _, _ := strings.Cut(next, "\n"); first != "" {
			shas = append(shas, first)
		}
	}
	return dedupe(shas), nil
}

func dedupe(s []string) []string {
	seen := map[string]bool{}
	var out []string
	for _, x := range s {
		if !seen[x] {
			seen[x] = true
			out = append(out, x)
		}
	}
	return out
}

// Subjects returns each commit's subject line.
func (g *Git) Subjects(shas []string) (map[string]string, error) {
	out := map[string]string{}
	if len(shas) == 0 {
		return out, nil
	}
	res, err := g.Run(append([]string{"log", "--no-walk=unsorted", "--format=%H%x09%s"}, shas...)...)
	if err != nil {
		return nil, err
	}
	for _, l := range splitLines(string(res)) {
		if sha, subj, ok := strings.Cut(l, "\t"); ok {
			out[sha] = subj
		}
	}
	return out, nil
}
