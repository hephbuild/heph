package xio

type Tracker struct {
	Written int64
	OnWrite func(written int64)
}

func (t *Tracker) Write(b []byte) (int, error) {
	t.Written += int64(len(b))
	if t.OnWrite != nil {
		t.OnWrite(t.Written)
	}
	return len(b), nil
}
