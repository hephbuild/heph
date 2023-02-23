package rcache

import "heph/utils/maps"

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

type HintStore struct {
	m maps.Map[string, Hint]
}

func (s *HintStore) Set(fqn string, hint Hint) {
	s.m.Set(fqn, hint)
}

var defaultHint = HintNone{}

func (s *HintStore) Get(fqn string) Hint {
	if hint, ok := s.m.GetOk(fqn); ok {
		return hint
	}

	return defaultHint
}

func (s *HintStore) Reset() {
	s.m = maps.Map[string, Hint]{}
}
