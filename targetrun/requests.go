package targetrun

import (
	"github.com/hephbuild/heph/graph"
)

type Requests []Request

func (rrs Requests) Has(t *graph.Target) bool {
	for _, rr := range rrs {
		if rr.Target.Addr == t.Addr {
			return true
		}
	}

	return false
}

func (rrs Requests) Get(t *graph.Target) Request {
	for _, rr := range rrs {
		if rr.Target.Addr == t.Addr {
			return rr
		}
	}

	return Request{Target: t}
}

func (rrs Requests) Targets() *graph.Targets {
	ts := graph.NewTargets(len(rrs))

	for _, rr := range rrs {
		ts.Add(rr.Target)
	}

	return ts
}

func (rrs Requests) Count(f func(rr Request) bool) int {
	c := 0
	for _, rr := range rrs {
		if f(rr) {
			c++
		}
	}

	return c
}
