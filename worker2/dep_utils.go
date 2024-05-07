package worker2

import (
	"github.com/hephbuild/heph/utils/xsync"
	"golang.org/x/exp/maps"
	"sync/atomic"
)

func deepDoPrecomputed(a Dep, f func(Dep)) {
	f(a)
	for _, dep := range a.GetNode().Dependencies.TransitiveValues() {
		f(dep)
	}
}

var deepDoMapPool = xsync.Pool[map[Dep]struct{}]{New: func() map[Dep]struct{} {
	return map[Dep]struct{}{}
}}

func deepDo(a Dep, f func(Dep)) {
	if false {
		// This approach sounds good on paper, but in reality very CPU intensive since it requires
		// read & write to the deps set at every change, at every level...
		deepDoPrecomputed(a, f)
	} else {
		deepDoRecursive(a, f)
	}
}

func deepDoRecursive(a Dep, f func(Dep)) {
	m := deepDoMapPool.Get()
	maps.Clear(m)
	defer deepDoMapPool.Put(m)
	deepDoRecursiveInner(a, f, m)
}

func deepDoRecursiveInner(a Dep, f func(Dep), m map[Dep]struct{}) {
	if _, ok := m[a]; ok {
		return
	}
	m[a] = struct{}{}

	f(a)
	for _, dep := range a.GetNode().Dependencies.Values() {
		deepDoRecursiveInner(dep, f, m)
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

func CollectStats(dep Dep) Stats {
	s := Stats{}
	dep.DeepDo(func(dep Dep) {
		if _, ok := dep.(*Group); ok {
			return
		}

		s.record(dep)
	})

	return s
}
