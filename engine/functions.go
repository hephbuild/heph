package engine

import (
	"fmt"
	"github.com/hephbuild/heph/exprs"
	"github.com/hephbuild/heph/graph"
)

var utilFunctions = map[string]exprs.Func{
	"printf": func(expr exprs.Expr) (string, error) {
		f, err := expr.MustPosArg(0)
		if err != nil {
			return "", err
		}

		args := make([]any, 0, len(expr.PosArgs))
		for _, v := range expr.PosArgs[1:] {
			args = append(args, v)
		}
		return fmt.Sprintf(f, args...), nil
	},
}

func (e *Engine) queryFunctions(t *Target) map[string]exprs.Func {
	getTarget := func(expr exprs.Expr) (*Target, error) {
		fqn := expr.PosArg(0, t.FQN)

		target := e.Targets.FindFQN(fqn)
		if target == nil {
			return nil, NewTargetNotFoundError(fqn, e.Graph.Targets())
		}

		return target, nil
	}

	m := map[string]exprs.Func{
		"target_fqn": func(expr exprs.Expr) (string, error) {
			return t.FQN, nil
		},
		"outdir": func(expr exprs.Expr) (string, error) {
			t, err := getTarget(expr)
			if err != nil {
				return "", err
			}

			universe, err := e.Graph.DAG().GetParents(t.Target)
			if err != nil {
				return "", err
			}
			universe = append(universe, t.Target)

			if !graph.Contains(universe, t.FQN) {
				return "", fmt.Errorf("cannot get outdir of %v", t.FQN)
			}

			if t.OutExpansionRoot == nil {
				return "", fmt.Errorf("%v has not been cached yet", t.FQN)
			}

			return t.OutExpansionRoot.Join(t.Package.Path).Abs(), nil
		},
		"hash_input": func(expr exprs.Expr) (string, error) {
			t, err := getTarget(expr)
			if err != nil {
				return "", err
			}

			universe, err := e.Graph.DAG().GetParents(t.Target)
			if err != nil {
				return "", err
			}
			universe = append(universe, t.Target)

			if !graph.Contains(universe, t.FQN) {
				return "", fmt.Errorf("cannot get input of %v", t.FQN)
			}

			return e.LocalCache.HashInput(t)
		},
		"hash_output": func(expr exprs.Expr) (string, error) {
			fqn, err := expr.MustPosArg(0)
			if err != nil {
				return "", err
			}

			t := e.Graph.Targets().Find(fqn)
			if t == nil {
				return "", NewTargetNotFoundError(fqn, e.Graph.Targets())
			}

			universe, err := e.Graph.DAG().GetParents(t)
			if err != nil {
				return "", err
			}

			if !graph.Contains(universe, t.FQN) {
				return "", fmt.Errorf("cannot get output of %v", t.FQN)
			}

			output := expr.PosArg(1, "")
			return e.LocalCache.HashOutput(e.Targets.Find(t), output)
		},
		"repo_root": func(expr exprs.Expr) (string, error) {
			return e.Root.Root.Abs(), nil
		},
	}

	for k, v := range utilFunctions {
		m[k] = v
	}

	return m
}
