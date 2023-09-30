package graph

import (
	"fmt"
	"github.com/hephbuild/heph/exprs"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/utils/sets"
)

type exprFunc struct {
	Run     func(t *Target, expr exprs.Expr, breadcrumb *sets.StringSet) ([]*Target, error)
	Analyze func(t specs.Target, expr exprs.Expr, breadcrumb *sets.StringSet, required *sets.Set[string, specs.Matcher]) error
}

func (e *State) exprFunctions() map[string]exprFunc {
	return map[string]exprFunc{
		"collect": {
			Run: func(t *Target, expr exprs.Expr, breadcrumb *sets.StringSet) ([]*Target, error) {
				targets, err := e.collect(t, expr)
				if err != nil {
					return nil, fmt.Errorf("`%v`: %w", expr.String, err)
				}

				for _, target := range targets {
					err := e.LinkTarget(target, breadcrumb)
					if err != nil {
						return nil, fmt.Errorf("collect: %w", err)
					}
				}

				return targets, nil
			},
			Analyze: func(t specs.Target, expr exprs.Expr, breadcrumb *sets.StringSet, required *sets.Set[string, specs.Matcher]) error {
				matcher, _, err := e.collectMatcher(t, expr)
				if err != nil {
					return err
				}

				required.Add(matcher)

				return nil
			},
		},
		"find_parent": {
			Run: func(t *Target, expr exprs.Expr, breadcrumb *sets.StringSet) ([]*Target, error) {
				target, err := e.findParent(t, expr)
				if err != nil {
					return nil, fmt.Errorf("`%v`: %w", expr.String, err)
				}

				if target != nil {
					err = e.LinkTarget(target, breadcrumb)
					if err != nil {
						return nil, fmt.Errorf("find_parent: %w", err)
					}

					return []*Target{target}, nil
				}

				return []*Target{}, nil
			},
			Analyze: func(t specs.Target, expr exprs.Expr, breadcrumb *sets.StringSet, missing *sets.Set[string, specs.Matcher]) error {
				missing.Add(specs.AllMatcher)

				return nil
			},
		},
	}
}
