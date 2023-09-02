package xprogress

type Counter struct {
	n int64
}

func (u *Counter) SetN(n int64) {
	u.n = n
}

func (u *Counter) AddN(n int64) {
	u.n += n
}

func (u *Counter) N() int64 {
	return u.n
}

func NewCounter() *Counter {
	return &Counter{}
}
