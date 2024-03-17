package worker2

type InStore interface {
	Copy(OutStore)
	Get(key string) (any, error)
}

type OutStore interface {
	Set(Value)
	Get() Value
}

type inStore struct {
	m map[string]Value
}

func (s *inStore) Copy(outs OutStore) {
	mv := make(MapValue, len(s.m))
	for k, v := range s.m {
		mv.Set(k, v)
	}
	outs.Set(mv)
}

func (s *inStore) Get(name string) (any, error) {
	return s.m[name].Get()
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
