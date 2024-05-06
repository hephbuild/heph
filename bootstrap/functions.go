package bootstrap

import (
	"fmt"
	"github.com/hephbuild/heph/exprs"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/hroot"
	"github.com/hephbuild/heph/lcache"
	"github.com/hephbuild/heph/specs"
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

func QueryFunctions(
	root *hroot.State,
	localCache *lcache.LocalCacheState,
	g *graph.State,
	t *graph.Target,
) map[string]exprs.Func {
	getTarget := func(expr exprs.Expr) (*graph.Target, error) {
		addr := expr.PosArg(0, t.Addr)

		target := g.Targets().Find(addr)
		if target == nil {
			return nil, specs.NewTargetNotFoundError(addr, g.Targets())
		}

		return target, nil
	}

	m := map[string]exprs.Func{
		"target_addr": func(expr exprs.Expr) (string, error) {
			return t.Addr, nil
		},
		"outdir": func(expr exprs.Expr) (string, error) {
			t, err := getTarget(expr)
			if err != nil {
				return "", err
			}

			universe, err := g.DAG().GetParents(t.Target)
			if err != nil {
				return "", err
			}
			universe = append(universe, t)

			ltarget := localCache.Metas.Find(t)

			if !graph.Contains(universe, t.Addr) {
				return "", fmt.Errorf("cannot get outdir of %v", t.Addr)
			}

			return ltarget.OutExpansionRoot().Join(t.Package.Path).Abs(), nil
		},
		"shared_stage_dir": func(expr exprs.Expr) (string, error) {
			t, err := getTarget(expr)
			if err != nil {
				return "", err
			}

			universe, err := g.DAG().GetParents(t.Target)
			if err != nil {
				return "", err
			}
			universe = append(universe, t)

			ltarget := localCache.Metas.Find(t)

			if !graph.Contains(universe, t.Addr) {
				return "", fmt.Errorf("cannot get shared stage dir of %v", t.Addr)
			}

			return ltarget.SharedStageRoot().Abs(), nil
		},
		"hash_input": func(expr exprs.Expr) (string, error) {
			t, err := getTarget(expr)
			if err != nil {
				return "", err
			}

			universe, err := g.DAG().GetParents(t.Target)
			if err != nil {
				return "", err
			}
			universe = append(universe, t)

			if !graph.Contains(universe, t.Addr) {
				return "", fmt.Errorf("cannot get input of %v", t.Addr)
			}

			return localCache.HashInput(t)
		},
		"hash_output": func(expr exprs.Expr) (string, error) {
			addr, err := expr.MustPosArg(0)
			if err != nil {
				return "", err
			}

			t := g.Targets().Find(addr)
			if t == nil {
				return "", specs.NewTargetNotFoundError(addr, g.Targets())
			}

			universe, err := g.DAG().GetParents(t)
			if err != nil {
				return "", err
			}

			if !graph.Contains(universe, t.Addr) {
				return "", fmt.Errorf("cannot get output of %v", t.Addr)
			}

			output := expr.PosArg(1, "")
			return localCache.HashOutput(t, output)
		},
		"repo_root": func(expr exprs.Expr) (string, error) {
			return root.Root.Abs(), nil
		},
	}

	for k, v := range utilFunctions {
		m[k] = v
	}

	return m
}
