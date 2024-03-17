package worker2

type Value interface {
	Get() (any, error)
}

func NewValue[T any](v T) MemValue[T] {
	return MemValue[T]{V: v}
}

type MemValue[T any] struct {
	V T
}

func (v MemValue[T]) Get() (any, error) {
	return v.V, nil
}

type MapValue map[string]Value

func (m MapValue) Get() (any, error) {
	out := make(map[string]any, len(m))
	for k, vv := range m {
		if vv == nil {
			continue
		}
		v, err := vv.Get()
		if err != nil {
			return nil, err
		}
		out[k] = v
	}

	return out, nil
}

func (m MapValue) Set(k string, v Value) {
	m[k] = v
}
