package cmd

import (
	"github.com/lithammer/fuzzysearch/fuzzy"
	"heph/engine"
	"sort"
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

func autocompleteTargetName(targets []string, s string) []string {
	if s == "" {
		return targets
	}

	matches := fuzzy.RankFindNormalizedFold(s, targets)
	sort.Sort(matches)

	suggestions := make([]string, 0)
	for _, s := range matches {
		suggestions = append(suggestions, s.Target)
	}

	return suggestions
}

func autocompleteLabel(labels []string, s string) []string {
	if s == "" {
		return labels
	}

	matches := fuzzy.RankFindNormalizedFold(s, labels)
	sort.Sort(matches)

	suggestions := make([]string, 0)
	for _, s := range matches {
		suggestions = append(suggestions, s.Target)
	}

	return suggestions
}

func autocompleteLabelOrTarget(targets, labels []string, s string) []string {
	tch := make(chan []string)
	lch := make(chan []string)
	go func() {
		tch <- autocompleteTargetName(targets, s)
	}()
	go func() {
		lch <- autocompleteLabel(labels, s)
	}()
	suggestions := <-tch
	suggestions = append(suggestions, <-lch...)

	return suggestions
}
