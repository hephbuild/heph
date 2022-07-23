package engine

import "github.com/heimdalr/dag"

type DAG struct {
	*dag.DAG
}

func (e *DAG) GetAncestors(target *Target) (Targets, error) {
	ancestors, err := e.DAG.GetAncestors(target.FQN)
	if err != nil {
		return nil, err
	}

	torun := make(Targets, 0)
	for _, anci := range ancestors {
		anc := anci.(*Target)
		torun = append(torun, anc)
	}

	return torun, nil
}
