package rcache

import "github.com/hephbuild/heph/utils/maps"

type Hint interface {
	Skip() bool
}

type HintNone struct{}

func (h HintNone) Skip() bool {
	return false
}

type HintSkip struct{}

func (h HintSkip) Skip() bool {
	return true
}

type hintKey struct {
	fqn, cache string
}

type HintStore struct {
	m maps.Map[hintKey, Hint]
}

func (s *HintStore) Set(fqn, cache string, hint Hint) {
	s.m.Set(hintKey{fqn, cache}, hint)
}

var defaultHint = HintNone{}

func (s *HintStore) Get(fqn, cache string) Hint {
	if hint, ok := s.m.GetOk(hintKey{fqn, cache}); ok {
		return hint
	}

	return defaultHint
}

func (s *HintStore) Reset() {
	s.m = maps.Map[hintKey, Hint]{}
}
