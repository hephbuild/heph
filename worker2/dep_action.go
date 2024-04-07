package worker2

type EventDeclared struct {
	Dep Dep
}

type ActionOption func(*Action)

func WithActionName(name string) ActionOption {
	return func(action *Action) {
		action.m.Lock()
		defer action.m.Unlock()

		action.Name = name
	}
}

func WithActionDep(d Dep) ActionOption {
	return func(action *Action) {
		action.AddDep(d)
	}
}

func WithActionHook(hook Hook) ActionOption {
	return func(action *Action) {
		if hook == nil {
			return
		}

		action.m.Lock()
		defer action.m.Unlock()

		action.Hooks = append(action.Hooks, hook)

		hook(EventDeclared{Dep: action})
	}
}

func NewAction(opts ...ActionOption) *Action {
	a := &Action{}

	for _, opt := range opts {
		opt(a)
	}

	return a
}
