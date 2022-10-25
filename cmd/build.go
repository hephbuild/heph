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

func autocompleteTargetName(targets []string, s string) []string {
	if s == "" {
		return targets
	}

	if strings.HasPrefix(s, "//") {
		return autocompletePrefix(nil, targets, s)
	}

	matches := fuzzy.RankFindNormalizedFold(s, targets)
	sort.Sort(matches)

	suggestions := autocompletePrefix(nil, targets, s)
	for _, s := range matches {
		suggestions = append(suggestions, s.Target)
	}

	if len(suggestions) > 10 {
		suggestions = suggestions[:10]
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

func autocompleteLabelOrTarget(targets, labels []string, s string) []string {
	tch := make(chan []string)
	lch := make(chan []string)
	go func() {
		lch <- autocompleteLabel(labels, s)
	}()
	go func() {
		tch <- autocompleteTargetName(targets, s)
	}()
	suggestions := <-lch
	suggestions = append(suggestions, <-tch...)

	return suggestions
}
