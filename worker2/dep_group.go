package worker2

import "github.com/hephbuild/heph/worker2/dag"

func NewGroup(deps ...Dep) *Group {
	return NewNamedGroup("", deps...)
}

func NewNamedGroup(name string, deps ...Dep) *Group {
	return NewGroupWith(GroupConfig{Name: name, Deps: deps})
}

type GroupConfig struct {
	Name string
	Deps []Dep
}

func NewGroupWith(cfg GroupConfig) *Group {
	g := &Group{baseDep: newBase()}
	g.node = dag.NewNode[Dep](cfg.Name, g)

	g.name = cfg.Name
	g.AddDep(cfg.Deps...)

	g.node.AddHook(func(event dag.Event) {
		switch event := event.(type) {
		case dag.EventNewDep[Dep]:
			for _, hook := range g.hooks {
				hook(EventNewDep{Target: g, AddedDep: event.Node.V})
			}
		}
	})

	return g
}
