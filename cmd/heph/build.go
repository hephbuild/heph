package main

import (
	"cmp"
	"github.com/hephbuild/heph/cmd/heph/search"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/lithammer/fuzzysearch/fuzzy"
	"golang.org/x/exp/slices"
	"strings"
)

func sortedTargets(targets []*graph.Target, skipPrivate bool) []*graph.Target {
	stargets := make([]*graph.Target, 0)
	for _, target := range targets {
		if skipPrivate && target.IsPrivate() {
			continue
		}

		stargets = append(stargets, target)
	}

	slices.SortFunc(stargets, func(a, b *graph.Target) int {
		return strings.Compare(a.Addr, b.Addr)
	})

	return stargets
}

func sortedTargetNames(targets []*graph.Target, skipPrivate bool) []string {
	names := make([]string, 0)
	for _, t := range sortedTargets(targets, skipPrivate) {
		names = append(names, t.Addr)
	}

	return names
}

func autocompletePrefix(suggestions, ss []string, comp string) []string {
	return append(suggestions, ads.Filter(ss, func(s string) bool {
		return strings.HasPrefix(s, comp)
	})...)
}

func autocompleteTargetName(targets specs.Targets, s string) (bool, []string) {
	if s == "" {
		return false, targets.Addrs()
	}

	if strings.HasPrefix(s, "/") {
		return false, autocompletePrefix(nil, targets.Addrs(), s)
	}

	return true, fuzzyFindTargetName(targets, s, 10)
}

func fuzzyFindTargetName(targets specs.Targets, s string, max int) []string {
	suggestions := search.FuzzyFindTarget(targets, s, max)
	return suggestions.Addrs()
}

func autocompleteLabel(labels []string, s string) []string {
	if s == "" {
		return labels
	}

	if err := specs.LabelValidate(s); err != nil {
		return nil
	}

	matches := fuzzy.RankFindNormalizedFold(s, labels)
	slices.SortStableFunc(matches, func(a, b fuzzy.Rank) int {
		return cmp.Compare(a.Distance, b.Distance)
	})

	suggestions := ads.Map(matches, func(t fuzzy.Rank) string {
		return t.Target
	})

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
