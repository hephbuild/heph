package worker2

type EventDeclared struct {
	Dep Dep
}

func NewAction(cfg ActionConfig) *Action {
	a := &Action{deps: NewDeps()}
	a.deps.setOwner(a)

	a.name = cfg.Name
	a.ctx = cfg.Ctx
	a.name = cfg.Name
	a.deps.Add(cfg.Deps...)
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
