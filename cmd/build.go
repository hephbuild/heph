package cmd

import (
	"github.com/lithammer/fuzzysearch/fuzzy"
	"heph/engine"
	"heph/targetspec"
	"sort"
	"strings"
)

func sortedTargets(targets []*engine.Target, skipPrivate bool) []*engine.Target {
	stargets := make([]*engine.Target, 0)
	for _, target := range targets {
		if skipPrivate && target.IsPrivate() {
			continue
		}

		stargets = append(stargets, target)
	}

	sort.Slice(stargets, func(i, j int) bool {
		return stargets[i].FQN < stargets[j].FQN
	})

	return stargets
}

func sortedTargetNames(targets []*engine.Target, skipPrivate bool) []string {
	names := make([]string, 0)
	for _, t := range sortedTargets(targets, skipPrivate) {
		names = append(names, t.FQN)
	}

	return names
}

func autocompletePrefix(suggestions, ss []string, comp string) []string {
	for _, s := range ss {
		if strings.HasPrefix(s, comp) {
			suggestions = append(suggestions, s)
		}
	}

	return suggestions
}

func autocompleteTargetName(targets []string, s string) (bool, []string) {
	if s == "" {
		return false, targets
	}

	if strings.HasPrefix(s, "/") {
		return false, autocompletePrefix(nil, targets, s)
	}

	return true, fuzzyFindTargetName(targets, s, 10)
}

func fuzzyFindTargetName(targets []string, s string, max int) []string {
	if s == "" {
		return nil
	}

	matches := fuzzy.RankFindNormalizedFold(s, targets)
	sort.Sort(matches)

	suggestions := autocompletePrefix(nil, targets, s)
	for _, s := range matches {
		suggestions = append(suggestions, s.Target)
	}

	if max > 0 && len(suggestions) > max {
		suggestions = suggestions[:max]
	}

	return suggestions
}

var labelChars = []byte(targetspec.Alphanum + `_`)

func autocompleteLabel(labels []string, s string) []string {
	if s == "" {
		return labels
	}

	if !targetspec.ContainsOnly(s, labelChars) {
		return nil
	}

	matches := fuzzy.RankFindNormalizedFold(s, labels)
	sort.Sort(matches)

	suggestions := make([]string, 0)
	for _, s := range matches {
		suggestions = append(suggestions, s.Target)
	}

	if len(suggestions) > 5 {
		suggestions = suggestions[:5]
	}

	return suggestions
}

func autocompleteLabelOrTarget(targets, labels []string, s string) (bool, []string) {
	type res struct {
		f bool     // isFuzzy
		s []string // suggestions
	}
	tch := make(chan res)
	lch := make(chan res)
	go func() {
		lch <- res{s: autocompleteLabel(labels, s)}
	}()
	go func() {
		f, suggs := autocompleteTargetName(targets, s)
		tch <- res{f, suggs}
	}()

	var suggestions []string
	var isFuzzy bool
	for _, ch := range []chan res{lch, tch} {
		r := <-ch

		suggestions = append(suggestions, r.s...)
		if r.f {
			isFuzzy = true
		}
	}

	return isFuzzy, suggestions
}
