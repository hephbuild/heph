package worker2

type EventDeclared struct {
	Dep Dep
}

func NewAction(cfg ActionConfig) *Action {
	a := &Action{baseDep: newBase()}
	a.node = NewNode[Dep](cfg.Name, a, &a.m)

	a.name = cfg.Name
	a.ctx = cfg.Ctx
	a.name = cfg.Name
	a.AddDep(cfg.Deps...)
	a.hooks = cfg.Hooks
	a.scheduler = cfg.Scheduler
	a.requests = cfg.Requests
	a.do = cfg.Do

	for _, hook := range a.hooks {
		if hook == nil {
			continue
		}
		hook(EventDeclared{Dep: a})
	}

	return a
}
