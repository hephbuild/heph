package search

import (
	"github.com/hephbuild/heph/specs"
	"github.com/lithammer/fuzzysearch/fuzzy"
	"sort"
)

func FuzzyFindTarget(targets specs.Targets, s string, max int) specs.Targets {
	if s == "" {
		return nil
	}

	addrs := targets.Addrs()

	matches := fuzzy.RankFindNormalizedFold(s, addrs)
	sort.Sort(matches)

	var suggestions []string
	for _, s := range matches {
		suggestions = append(suggestions, s.Target)
	}

	if max > 0 && len(suggestions) > max {
		suggestions = suggestions[:max]
	}

	suggTargets := make(specs.Targets, 0, len(suggestions))
	for _, suggestion := range suggestions {
		spec, ok := targets.Get(suggestion)
		if !ok {
			continue
		}

		suggTargets = append(suggTargets, spec)
	}

	return suggTargets
}
