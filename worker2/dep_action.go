package worker2

import (
	"context"
	"github.com/hephbuild/heph/worker2/dag"
)

type ActionConfig struct {
	Ctx       context.Context
	Name      string
	Deps      []Dep
	Hooks     []Hook
	Scheduler Scheduler
	Requests  map[string]float64
	Do        func(ctx context.Context, ins InStore, outs OutStore) error
}

func NewAction(cfg ActionConfig) *Action {
	a := &Action{baseDep: newBase()}
	a.node = dag.NewNode[Dep](cfg.Name, a)

	a.name = cfg.Name
	a.ctx = cfg.Ctx
	a.name = cfg.Name
	a.AddDep(cfg.Deps...)
	for _, hook := range cfg.Hooks {
		a.AddHook(hook)
	}
	a.scheduler = cfg.Scheduler
	a.requests = cfg.Requests
	a.do = cfg.Do

	for _, hook := range a.hooks {
		if hook == nil {
			continue
		}
		hook(EventDeclared{Dep: a})
	}

	a.node.AddHook(func(event dag.Event) {
		switch event := event.(type) {
		case dag.EventNewDep[Dep]:
			for _, hook := range a.hooks {
				hook(EventNewDep{Target: event.Node.V})
			}
		}
	})

	return a
}
