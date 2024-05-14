package worker2

import (
	"github.com/hephbuild/heph/utils/xsync"
	"golang.org/x/exp/maps"
	"sync"
	"sync/atomic"
)

func deepDo(a Dep, f func(Dep)) {
	if a.GetNode().IsFrozen() {
		// This approach sounds good on paper, but in reality very CPU intensive since it requires
		// read & write to the deps set at every change, at every level...
		deepDoPrecomputed(a, f)
	} else {
		deepDoRecursive(a, f)
	}
}

func deepDoPrecomputed(a Dep, f func(Dep)) {
	f(a)
	for _, dep := range a.GetNode().Dependencies.TransitiveValues() {
		f(dep)
	}
}

var deepDoMapPool = xsync.Pool[map[Dep]struct{}]{New: func() map[Dep]struct{} {
	return map[Dep]struct{}{}
}}

func deepDoRecursive(a Dep, f func(Dep)) {
	m := deepDoMapPool.Get()
	defer func() {
		maps.Clear(m)
		deepDoMapPool.Put(m)
	}()
	deepDoRecursiveInner(a, f, m)
}

func deepDoInner(a Dep, f func(Dep), m map[Dep]struct{}) bool {
	if _, ok := m[a]; ok {
		return false
	}
	m[a] = struct{}{}

	f(a)

	return true
}

func deepDoRecursiveInner(a Dep, f func(Dep), m map[Dep]struct{}) {
	if !deepDoInner(a, f, m) {
		return
	}

	if a.GetNode().IsFrozen() {
		for _, dep := range a.GetNode().Dependencies.TransitiveValues() {
			deepDoInner(dep, f, m)
		}
	} else {
		for _, dep := range a.GetNode().Dependencies.Values() {
			deepDoRecursiveInner(dep, f, m)
		}
	}
}

type Stats struct {
	All       uint64
	Completed uint64

	Scheduled uint64
	Waiting   uint64
	Succeeded uint64
	Failed    uint64
	Skipped   uint64
	Suspended uint64
	Running   uint64
}

func (s *Stats) record(dep Dep) {
	if _, ok := dep.(*Group); ok {
		return
	}

	atomic.AddUint64(&s.All, 1)

	j := dep.getExecution()
	if j == nil {
		return
	}

	if j.State.IsFinal() {
		atomic.AddUint64(&s.Completed, 1)
	}

	switch j.State {
	case ExecStateQueued:
		atomic.AddUint64(&s.Waiting, 1)
	case ExecStateSucceeded:
		atomic.AddUint64(&s.Succeeded, 1)
	case ExecStateFailed:
		atomic.AddUint64(&s.Failed, 1)
	case ExecStateSkipped:
		atomic.AddUint64(&s.Skipped, 1)
	case ExecStateSuspended:
		atomic.AddUint64(&s.Suspended, 1)
	case ExecStateRunning:
		atomic.AddUint64(&s.Running, 1)
	case ExecStateScheduled:
		atomic.AddUint64(&s.Scheduled, 1)
	}
}

// CollectStats can get quite expensive on large DAGs, prefer NewStatsCollector
func CollectStats(dep Dep) Stats {
	s := Stats{}
	dep.DeepDo(func(dep Dep) {
		s.record(dep)
	})

	return s
}

type StatsCollector struct {
	completed, skipped, succeeded, failed atomic.Uint64

	mu         sync.Mutex
	depsm      map[Dep]struct{}
	completedm map[uint64]struct{}
}

func NewStatsCollector() *StatsCollector {
	return &StatsCollector{
		depsm:      map[Dep]struct{}{},
		completedm: map[uint64]struct{}{},
	}
}

func (c *StatsCollector) Collect() Stats {
	c.mu.Lock()
	defer c.mu.Unlock()

	s := Stats{
		All:       c.completed.Load(),
		Completed: c.completed.Load(),
		Skipped:   c.skipped.Load(),
		Succeeded: c.succeeded.Load(),
		Failed:    c.failed.Load(),
	}
	for dep := range c.depsm {
		s.record(dep)
	}

	return s
}

func (c *StatsCollector) hook(event Event) {
	switch event := event.(type) {
	case EventNewDep:
		c.Register(event.AddedDep)
	case EventCompleted:
		c.onCompleted(event.Execution.Dep)
	case EventSkipped:
		c.onCompleted(event.Execution.Dep)
	}
}

func (c *StatsCollector) onCompleted(dep Dep) {
	c.mu.Lock()
	defer c.mu.Unlock()

	id := dep.getExecution().ID

	if _, ok := c.completedm[id]; ok {
		return
	}
	c.completedm[id] = struct{}{}
	delete(c.depsm, dep)

	if _, ok := dep.(*Group); ok {
		return
	}
	c.completed.Add(1)

	switch dep.GetState() {
	case ExecStateSucceeded:
		c.succeeded.Add(1)
	case ExecStateFailed:
		c.failed.Add(1)
	case ExecStateSkipped:
		c.skipped.Add(1)
	}
}

func (c *StatsCollector) Register(dep Dep) {
	c.mu.Lock()
	if _, ok := c.depsm[dep]; ok {
		c.mu.Unlock()
		return
	}

	if exec := dep.getExecution(); exec != nil {
		if _, ok := c.completedm[exec.ID]; ok {
			c.mu.Unlock()
			return
		}
	}

	c.depsm[dep] = struct{}{}
	c.mu.Unlock()

	dep.AddHook(c.hook)

	for _, dep := range dep.GetNode().Dependencies.Values() {
		c.Register(dep)
	}
}
