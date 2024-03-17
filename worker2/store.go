package worker2

type InStore interface {
	Get(key string) any
}

type OutStore interface {
	Set(Value)
	Get() Value
}

type inStore struct {
	m map[string]any
}

func (s *inStore) Get(name string) any {
	return s.m[name]
}

type outStore struct {
	value Value
}

func (s *outStore) Set(v Value) {
	s.value = v
}

func (s *outStore) Get() Value {
	return s.value
}
