package worker2

func NewGroup(deps ...Dep) *Group {
	return NewGroupWith(GroupConfig{Deps: deps})
}

func NewGroupWith(cfg GroupConfig) *Group {
	g := &Group{}
	_ = g.GetDepsObj()

	g.name = cfg.Name
	g.AddDep(cfg.Deps...)

	return g
}