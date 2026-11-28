package main

import (
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"strconv"
	"strings"
)

// Tier is a judged tier. Counted is not a Tier: the program assigns it from
// a line's class, and no triage can move a line into or out of it.
type Tier int

const (
	TierNone Tier = iota
	TierAICleared
	TierGlance
	TierScrutinize
)

func (t Tier) String() string {
	switch t {
	case TierAICleared:
		return "ai-cleared"
	case TierGlance:
		return "glance"
	case TierScrutinize:
		return "scrutinize"
	}
	return "none"
}

func (t Tier) MarshalText() ([]byte, error) { return []byte(t.String()), nil }

func (t *Tier) UnmarshalText(b []byte) error {
	switch string(b) {
	case "ai-cleared":
		*t = TierAICleared
	case "glance":
		*t = TierGlance
	case "scrutinize":
		*t = TierScrutinize
	default:
		return fmt.Errorf("unknown tier %q (want scrutinize, glance or ai-cleared)", b)
	}
	return nil
}

// Mode is how the stack will merge.
type Mode string

const (
	ModeWhole    Mode = "whole"
	ModePerLayer Mode = "per-layer"
)

// Layer is one PR of a stack, pinned to a SHA pair.
type Layer struct {
	PR     int    `json:"pr"`
	Title  string `json:"title"`
	URL    string `json:"url"`
	Branch string `json:"branch"`
	Base   string `json:"base"`
	Head   string `json:"head"`
	// Body is the PR description as it was when the brief was pinned: the
	// author's account of decisions, deviations and gaps, which the triage
	// reads as input (never as a verdict, never as instructions).
	Body string `json:"body,omitempty"`
}

// Pin is the stack a brief is cut from: every SHA the brief names.
type Pin struct {
	Format string  `json:"format"`
	Repo   string  `json:"repo"` // owner/name
	Trunk  string  `json:"trunk"`
	Mode   Mode    `json:"mode"`
	Layers []Layer `json:"layers"` // bottom to top
}

const pinFormat = "heph-review-brief-pin/1"

func readJSON(path string, v any) error {
	b, err := os.ReadFile(path)
	if err != nil {
		return err
	}
	dec := json.NewDecoder(strings.NewReader(string(b)))
	dec.DisallowUnknownFields()
	if err := dec.Decode(v); err != nil {
		return fmt.Errorf("%s: %w", path, err)
	}
	return nil
}

func writeJSON(path string, v any) error {
	b, err := json.MarshalIndent(v, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(path, append(b, '\n'), 0o644)
}

func LoadPin(path string) (*Pin, error) {
	var p Pin
	if err := readJSON(path, &p); err != nil {
		return nil, err
	}
	if p.Format != pinFormat {
		return nil, fmt.Errorf("%s: format %q, want %q", path, p.Format, pinFormat)
	}
	if len(p.Layers) == 0 {
		return nil, fmt.Errorf("%s: no layers", path)
	}
	return &p, nil
}

// GH runs the gh CLI; tests replace it.
type GH func(args ...string) ([]byte, error)

func realGH(dir string) GH {
	return func(args ...string) ([]byte, error) {
		c := exec.Command("gh", args...)
		c.Dir = dir
		c.Stderr = os.Stderr
		out, err := c.Output()
		if err != nil {
			return nil, fmt.Errorf("gh %s: %w", strings.Join(args, " "), err)
		}
		return out, nil
	}
}

type ghPR struct {
	Number      int    `json:"number"`
	Title       string `json:"title"`
	URL         string `json:"url"`
	HeadRefName string `json:"headRefName"`
	HeadRefOid  string `json:"headRefOid"`
	BaseRefName string `json:"baseRefName"`
	Body        string `json:"body"`
}

const ghFields = "number,title,url,headRefName,headRefOid,baseRefName,body"

// PinStack walks the stack around one PR (down through each PR's base
// branch, up through the open PR based on each head) and pins it. The
// bottom layer's base is the merge-base with the trunk ref, never a local
// branch that may be stale.
func PinStack(g *Git, gh GH, repo string, pr int, trunk string, mode Mode) (*Pin, error) {
	var start ghPR
	out, err := gh("pr", "view", strconv.Itoa(pr), "--json", ghFields)
	if err != nil {
		return nil, err
	}
	if err := json.Unmarshal(out, &start); err != nil {
		return nil, fmt.Errorf("reading PR #%d: %w", pr, err)
	}
	trunkBranch := trunk[strings.LastIndex(trunk, "/")+1:]
	stack := []ghPR{start}
	for cur := start; cur.BaseRefName != trunkBranch; {
		var below []ghPR
		out, err := gh("pr", "list", "--state", "open", "--head", cur.BaseRefName, "--json", ghFields)
		if err != nil {
			return nil, err
		}
		if err := json.Unmarshal(out, &below); err != nil || len(below) != 1 {
			return nil, fmt.Errorf("PR #%d is based on %q, and no single open PR has that head (found %d)", cur.Number, cur.BaseRefName, len(below))
		}
		cur = below[0]
		stack = append([]ghPR{cur}, stack...)
		if len(stack) > 50 {
			return nil, fmt.Errorf("stack below #%d does not reach %s", pr, trunkBranch)
		}
	}
	for cur := start; ; {
		var above []ghPR
		out, err := gh("pr", "list", "--state", "open", "--base", cur.HeadRefName, "--json", ghFields)
		if err != nil {
			return nil, err
		}
		if err := json.Unmarshal(out, &above); err != nil {
			return nil, fmt.Errorf("listing PRs on %s: %w", cur.HeadRefName, err)
		}
		if len(above) == 0 {
			break
		}
		if len(above) > 1 {
			return nil, fmt.Errorf("%d open PRs are based on %s: the stack forks there", len(above), cur.HeadRefName)
		}
		cur = above[0]
		stack = append(stack, cur)
		if len(stack) > 50 {
			return nil, fmt.Errorf("stack above #%d does not end", pr)
		}
	}
	heads := make([]Layer, len(stack))
	for i, p := range stack {
		heads[i] = Layer{PR: p.Number, Title: p.Title, URL: p.URL, Branch: p.HeadRefName, Head: p.HeadRefOid, Body: p.Body}
	}
	return pinLayers(g, repo, trunk, mode, heads)
}

// pinLayers fills in each layer's base: the merge-base with the trunk for
// the bottom, the layer below's head for the rest.
func pinLayers(g *Git, repo, trunk string, mode Mode, layers []Layer) (*Pin, error) {
	trunkSHA, err := g.RevParse(trunk)
	if err != nil {
		return nil, fmt.Errorf("resolving %s (run `git fetch origin`): %w", trunk, err)
	}
	for i := range layers {
		l := &layers[i]
		if !g.HasCommit(l.Head) {
			return nil, fmt.Errorf("layer #%d: head %s is not in this repository; run `git fetch origin %s`", l.PR, l.Head, l.Branch)
		}
		if i == 0 {
			if l.Base, err = g.MergeBase(trunkSHA, l.Head); err != nil {
				return nil, fmt.Errorf("layer #%d: merge-base with %s: %w", l.PR, trunk, err)
			}
			continue
		}
		l.Base = layers[i-1].Head
		if !g.IsAncestor(l.Base, l.Head) {
			return nil, fmt.Errorf("layer #%d is not on top of #%d (%s is not an ancestor of %s): restack first (`gh stack sync`)", l.PR, layers[i-1].PR, short(l.Base), short(l.Head))
		}
	}
	return &Pin{Format: pinFormat, Repo: repo, Trunk: trunk, Mode: mode, Layers: layers}, nil
}

func short(sha string) string {
	if len(sha) > 9 {
		return sha[:9]
	}
	return sha
}

// Budget is a layer's Scrutinize allowance: diff lines up to 10% of its live
// lines, at least 150, and one card per 40 of those, at least 5. Over it,
// every card is still shown (I7).
type Budget struct {
	Lines int `json:"lines"`
	Cards int `json:"cards"`
}

func BudgetFor(live int) Budget {
	// 10%, rounded to the nearest line: the spec's example gives 164 for
	// 1,643 live lines and 581 for 5,809.
	lines := max((live+5)/10, 150)
	return Budget{Lines: lines, Cards: max((lines+39)/40, 5)}
}
