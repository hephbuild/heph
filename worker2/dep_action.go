package worker2

type EventDeclared struct {
	Dep Dep
}

func (EventDeclared) Replayable() bool {
	return true
}

func NewAction(cfg ActionConfig) *Action {
	a := &Action{baseDep: newBase()}
	a.node = NewNode[Dep](cfg.Name, a)

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

	a.node.AddHook(func(event DAGEvent) {
		switch event := event.(type) {
		case DAGEventNewDep[Dep]:
			for _, hook := range a.hooks {
				hook(EventNewDep{Target: event.Node.V})
			}
		}
	})

	return a
}
