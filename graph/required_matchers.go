package graph

import (
	"fmt"
	"github.com/hephbuild/heph/exprs"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/utils/sets"
)

type requiredMatchersAnalyzer struct {
	visited   *sets.StringSet
	required  *sets.Set[string, specs.Matcher]
	targets   *Targets
	exprFuncs map[string]exprFunc
}

func newRequiredMatchersAnalyzer(targets *Targets, functions map[string]exprFunc) *requiredMatchersAnalyzer {
	return &requiredMatchersAnalyzer{
		visited: sets.NewStringSet(0),
		required: sets.NewSet(func(t specs.Matcher) string {
			return t.String()
		}, 0),
		targets:   targets,
		exprFuncs: functions,
	}
}

func (e *requiredMatchersAnalyzer) requiredMatchers(ts ...*Target) (specs.Matcher, error) {
	for _, t := range ts {
		err := e.targetRequiredMatchers(t, nil)
		if err != nil {
			return nil, err
		}
	}

	return specs.OrNodeFactory(e.required.Slice()...), nil
}

func (e *requiredMatchersAnalyzer) targetRequiredMatchers(t *Target, breadcrumb *sets.StringSet) error {
	if e.visited.Has(t.Addr) {
		return nil
	}
	e.visited.Add(t.Addr)

	if breadcrumb != nil {
		if breadcrumb.Has(t.Addr) {
			addrs := append(breadcrumb.Slice(), t.Addr)
			return fmt.Errorf("linking cycle: %v", addrs)
		}
		breadcrumb = breadcrumb.Copy()
	} else {
		breadcrumb = sets.NewStringSet(1)
	}
	breadcrumb.Add(t.Addr)

	spec := t.Spec()

	err := e.computeRequiredMatchersDeps(spec, spec.Deps, breadcrumb)
	if err != nil {
		return err
	}

	err = e.computeRequiredMatchersDeps(spec, spec.HashDeps, breadcrumb)
	if err != nil {
		return err
	}

	err = e.computeRequiredMatchersDeps(spec, spec.RuntimeDeps, breadcrumb)
	if err != nil {
		return err
	}

	err = e.computeRequiredMatchersTools(spec, spec.Tools, breadcrumb)
	if err != nil {
		return err
	}

	err = e.computeRequiredMatchersDeps(spec, spec.Transitive.Deps, breadcrumb)
	if err != nil {
		return err
	}

	err = e.computeRequiredMatchersTools(spec, spec.Transitive.Tools, breadcrumb)
	if err != nil {
		return err
	}

	return nil
}

func (e *requiredMatchersAnalyzer) computeRequiredMatchers(t string, breadcrumb *sets.StringSet) error {
	dt := e.targets.Find(t)
	if dt == nil {
		m, err := specs.ParseMatcher(t)
		if err != nil {
			return err
		}

		e.required.Add(m)
		return nil
	}

	err := e.targetRequiredMatchers(dt, breadcrumb)
	if err != nil {
		return err
	}

	return nil
}

func (e *requiredMatchersAnalyzer) computeRequiredMatchersDeps(t specs.Target, deps specs.Deps, breadcrumb *sets.StringSet) error {
	for _, spec := range deps.Targets {
		err := e.computeRequiredMatchers(spec.Target, breadcrumb)
		if err != nil {
			return err
		}
	}

	for _, expr := range deps.Exprs {
		err := e.computeRequiredMatchersExpr(t, expr.Expr, breadcrumb)
		if err != nil {
			return err
		}
	}

	return nil
}

func (e *requiredMatchersAnalyzer) computeRequiredMatchersTools(t specs.Target, tools specs.Tools, breadcrumb *sets.StringSet) error {
	for _, spec := range tools.Targets {
		err := e.computeRequiredMatchers(spec.Target, breadcrumb)
		if err != nil {
			return err
		}
	}

	for _, expr := range tools.Exprs {
		err := e.computeRequiredMatchersExpr(t, expr.Expr, breadcrumb)
		if err != nil {
			return err
		}
	}

	return nil
}

func (e *requiredMatchersAnalyzer) computeRequiredMatchersExpr(t specs.Target, expr exprs.Expr, breadcrumb *sets.StringSet) error {
	err := e.exprFuncs[expr.Function].Analyze(t, expr, breadcrumb, e.required)
	if err != nil {
		return err
	}

	return nil
}
