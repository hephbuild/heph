package worker2

func NewGroup(deps ...Dep) *Group {
	return NewNamedGroup("", deps...)
}

func NewNamedGroup(name string, deps ...Dep) *Group {
	return NewGroupWith(GroupConfig{Name: name, Deps: deps})
}

func NewGroupWith(cfg GroupConfig) *Group {
	g := &Group{baseDep: newBase()}
	g.node = NewNode[Dep](cfg.Name, g)

	g.name = cfg.Name
	g.AddDep(cfg.Deps...)

	g.node.AddHook(func(event DAGEvent) {
		switch event := event.(type) {
		case DAGEventNewDep[Dep]:
			for _, hook := range g.hooks {
				hook(EventNewDep{Target: g, AddedDep: event.Node.V})
			}
		}
	})

	return g
}
