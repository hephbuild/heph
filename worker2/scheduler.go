package worker2

import (
	"golang.org/x/exp/slices"
	"sync"
)

type Scheduler interface {
	Schedule(Dep, InStore) error
	Done(Dep)
}

type UnlimitedScheduler struct {
	ch chan struct{}
}

func (ls UnlimitedScheduler) Schedule(d Dep, ins InStore) error {
	return nil
}

func (ls UnlimitedScheduler) Done(d Dep) {}

func NewLimitScheduler(limit int) *LimitScheduler {
	return &LimitScheduler{
		ch: make(chan struct{}, limit),
	}
}

type LimitScheduler struct {
	ch chan struct{}
}

func (ls *LimitScheduler) Schedule(d Dep, ins InStore) error {
	select {
	case <-d.GetCtx().Done():
		return d.GetCtx().Err()
	case ls.ch <- struct{}{}:
		return nil
	}
}

func (ls *LimitScheduler) Done(d Dep) {
	<-ls.ch
}

type RunningTracker struct {
	deps []Dep
	m    sync.RWMutex
}

func (t *RunningTracker) Deps() []Dep {
	t.m.RLock()
	defer t.m.RUnlock()

	return t.deps[:]
}

func (t *RunningTracker) Scheduler(s Scheduler) Scheduler {
	return trackerScheduler{
		t: t,
		s: s,
	}
}

type trackerScheduler struct {
	t *RunningTracker
	s Scheduler
}

func (t trackerScheduler) Schedule(d Dep, ins InStore) error {
	err := t.s.Schedule(d, ins)
	if err != nil {
		return err
	}

	t.t.m.Lock()
	defer t.t.m.Unlock()

	t.t.deps = append(t.t.deps, d)

	return nil
}

func (t trackerScheduler) Done(d Dep) {
	t.t.m.Lock()
	defer t.t.m.Unlock()

	t.t.deps = slices.DeleteFunc(t.t.deps, func(dep Dep) bool {
		return dep == d
	})

	t.s.Done(d)
}
