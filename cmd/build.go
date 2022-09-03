package cmd

import (
	"github.com/lithammer/fuzzysearch/fuzzy"
	"heph/engine"
	"sort"
)

func targetNames(targets []*engine.Target) []string {
	stargets := make([]string, 0)
	for _, target := range targets {
		if target.Private() {
			continue
		}

		stargets = append(stargets, target.FQN)
	}

	return stargets
}

func sortedTargets(targets []*engine.Target, skipPrivate bool) []*engine.Target {
	stargets := make([]*engine.Target, 0)
	for _, target := range targets {
		if skipPrivate && target.Private() {
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

func autocompleteTargetName(targets *engine.Targets, s string) []string {
	if s == "" {
		return sortedTargetNames(targets.Slice(), true)
	}

	matches := fuzzy.RankFindNormalizedFold(s, targetNames(targets.Slice()))
	sort.Sort(matches)

	suggestions := make([]string, 0)
	for _, s := range matches {
		suggestions = append(suggestions, s.Target)
	}

	return suggestions
}

func autocompleteLabel(s string) []string {
	if s == "" {
		return Engine.Labels
	}

	matches := fuzzy.RankFindNormalizedFold(s, Engine.Labels)
	sort.Sort(matches)

	suggestions := make([]string, 0)
	for _, s := range matches {
		suggestions = append(suggestions, s.Target)
	}

	return suggestions
}
