package worker2

import "sync/atomic"

func deepDo(a Dep, f func(Dep)) {
	f(a)
	for _, dep := range a.GetNode().TransitiveDependencies() {
		f(dep)
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
