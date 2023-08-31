package xprogress

import (
	"github.com/machinebox/progress"
)

type Progresser interface {
	Progress() float64
}

func NewProgresser(size int64, c progress.Counter) Progresser {
	return progresser{
		c:    c,
		size: size,
	}
}

type progresser struct {
	c    progress.Counter
	size int64
}

func (p progresser) Progress() float64 {
	return float64(p.c.N()) / float64(p.size)
}

type weightedProgresser struct {
	p      Progresser
	weight int
}

type Multi struct {
	factors []weightedProgresser
}

func (u *Multi) Add(p Progresser, weight int) {
	u.factors = append(u.factors, weightedProgresser{
		p:      p,
		weight: weight,
	})
}

func (u *Multi) Progress() float64 {
	p := float64(0)
	w := 0

	for _, factor := range u.factors {
		w += factor.weight
		p += factor.p.Progress() * float64(factor.weight)
	}

	return p / float64(w)
}

type Counter = progress.Counter

type Unit struct {
	size, n int64
}

func (u *Unit) SetSize(size int64) {
	u.size = size
}

func (u *Unit) AddSize(size int64) {
	u.size += size
}

func (u *Unit) Size() int64 {
	return u.size
}

func (u *Unit) SetN(n int64) {
	u.n = n
}

func (u *Unit) AddN(n int64) {
	u.n += n
}

func (u *Unit) N() int64 {
	return u.n
}

func NewUnit() *Unit {
	return &Unit{}
}
