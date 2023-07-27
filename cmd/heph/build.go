package main

import (
	"github.com/hephbuild/heph/cmd/heph/search"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/tgt"
	"github.com/lithammer/fuzzysearch/fuzzy"
	"sort"
	"strings"
)

func sortedTargets(targets []*tgt.Target, skipPrivate bool) []*tgt.Target {
	stargets := make([]*tgt.Target, 0)
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

func sortedTargetNames(targets []*tgt.Target, skipPrivate bool) []string {
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

func autocompleteTargetName(targets specs.Targets, s string) (bool, []string) {
	if s == "" {
		return false, targets.FQNs()
	}

	if strings.HasPrefix(s, "/") {
		return false, autocompletePrefix(nil, targets.FQNs(), s)
	}

	return true, fuzzyFindTargetName(targets, s, 10)
}

func fuzzyFindTargetName(targets specs.Targets, s string, max int) []string {
	suggestions := search.FuzzyFindTarget(targets, s, max)
	return suggestions.FQNs()
}

var labelChars = []rune(specs.Alphanum + `_`)

func autocompleteLabel(labels []string, s string) []string {
	if s == "" {
		return labels
	}

	if !specs.ContainsOnly(s, labelChars) {
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

func autocompleteLabelOrTarget(targets specs.Targets, labels []string, s string) (bool, []string) {
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
