package search

import (
	"github.com/hephbuild/heph/targetspec"
	"github.com/lithammer/fuzzysearch/fuzzy"
	"sort"
)

func FuzzyFindTarget(targets targetspec.TargetSpecs, s string, max int) targetspec.TargetSpecs {
	if s == "" {
		return nil
	}

	fqns := targets.FQNs()

	matches := fuzzy.RankFindNormalizedFold(s, fqns)
	sort.Sort(matches)

	var suggestions []string
	for _, s := range matches {
		suggestions = append(suggestions, s.Target)
	}

	if max > 0 && len(suggestions) > max {
		suggestions = suggestions[:max]
	}

	suggTargets := make(targetspec.TargetSpecs, 0, len(suggestions))
	for _, suggestion := range suggestions {
		spec, ok := targets.Get(suggestion)
		if !ok {
			continue
		}

		suggTargets = append(suggTargets, spec)
	}

	return suggTargets
}
