package worker2

import (
	"github.com/hephbuild/heph/utils/sets"
)

func NewRunningTracker() *RunningTracker {
	return &RunningTracker{
		running: sets.NewIdentitySet[*Execution](0),
		group:   NewGroup(),
	}
}

type RunningTracker struct {
	running *sets.Set[*Execution, *Execution]
	group   *Group
}

func (t *RunningTracker) Get() []*Execution {
	return t.running.Slice()
}

func (t *RunningTracker) Group() *Group {
	return t.group
}

func (t *RunningTracker) Hook() Hook {
	if t == nil {
		return nil
	}

	return func(event Event) {
		switch event := event.(type) {
		case EventDeclared:
			t.group.AddDep(event.Dep)
		case EventScheduled:
			t.group.AddDep(event.Execution.Dep)
		case EventStarted:
			t.running.Add(event.Execution)
		case EventSkipped:
			t.running.Remove(event.Execution)
		case EventCompleted:
			t.running.Remove(event.Execution)
		}
	}
}
