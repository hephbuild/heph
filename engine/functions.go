package engine

import (
	"fmt"
	"github.com/hephbuild/heph/exprs"
	"strconv"
	"strings"
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

		target := e.Targets.Find(fqn)
		if target == nil {
			return nil, NewTargetNotFoundError(fqn)
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

			universe, err := e.DAG().GetParents(t)
			if err != nil {
				return "", err
			}
			universe = append(universe, t)

			if !Contains(universe, t.FQN) {
				return "", fmt.Errorf("cannot get outdir of %v", t.FQN)
			}

			if t.OutExpansionRoot == nil {
				return "", fmt.Errorf("%v has not been cached yet", t.FQN)
			}

			return t.OutExpansionRoot.Join(t.Package.FullName).Abs(), nil
		},
		"hash_input": func(expr exprs.Expr) (string, error) {
			t, err := getTarget(expr)
			if err != nil {
				return "", err
			}

			universe, err := e.DAG().GetParents(t)
			if err != nil {
				return "", err
			}
			universe = append(universe, t)

			if !Contains(universe, t.FQN) {
				return "", fmt.Errorf("cannot get input of %v", t.FQN)
			}

			return e.hashInput(t), nil
		},
		"hash_output": func(expr exprs.Expr) (string, error) {
			fqn, err := expr.MustPosArg(0)
			if err != nil {
				return "", err
			}

			t := e.Targets.Find(fqn)
			if t == nil {
				return "", NewTargetNotFoundError(fqn)
			}

			universe, err := e.DAG().GetParents(t)
			if err != nil {
				return "", err
			}

			if !Contains(universe, t.FQN) {
				return "", fmt.Errorf("cannot get output of %v", t.FQN)
			}

			output := expr.PosArg(1, "")
			return e.hashOutput(t, output), nil
		},
		"repo_root": func(expr exprs.Expr) (string, error) {
			return e.Root.Abs(), nil
		},
	}

	for k, v := range utilFunctions {
		m[k] = v
	}

	return m
}

func (e *Engine) collect(t *Target, expr exprs.Expr) ([]*Target, error) {
	s, err := expr.MustPosArg(0)
	if err != nil {
		return nil, err
	}

	pkgMatcher := ParseTargetSelector(t.Package.FullName, s)

	var includeMatchers TargetMatchers
	var excludeMatchers TargetMatchers
	must := false

	for _, arg := range expr.NamedArgs {
		switch arg.Name {
		case "must":
			must = true
		case "include", "exclude":
			m := ParseTargetSelector(t.Package.FullName, arg.Value)
			if arg.Name == "exclude" {
				excludeMatchers = append(excludeMatchers, m)
			} else {
				includeMatchers = append(includeMatchers, m)
			}
		default:
			return nil, fmt.Errorf("unhandled %v arg `%v`", expr.Function, arg.Name)
		}
	}

	matchers := TargetMatchers{
		pkgMatcher,
	}
	if len(includeMatchers) > 0 {
		matchers = append(matchers, OrMatcher(includeMatchers...))
	}
	if len(excludeMatchers) > 0 {
		matchers = append(matchers, NotMatcher(OrMatcher(excludeMatchers...)))
	}

	matcher := AndMatcher(matchers...)

	targets := make([]*Target, 0)
	for _, target := range e.Targets.Slice() {
		if !matcher(target) {
			continue
		}

		targets = append(targets, target)
	}

	if must && len(targets) == 0 {
		return nil, fmt.Errorf("must match a target, found none")
	}

	return targets, nil
}

func (e *Engine) findParent(t *Target, expr exprs.Expr) (*Target, error) {
	selector, err := expr.MustPosArg(0)
	if err != nil {
		return nil, err
	}

	must, _ := strconv.ParseBool(expr.NamedArg("must"))

	if !strings.HasPrefix(selector, ":") {
		return nil, fmt.Errorf("must be a target selector, got `%v`", selector)
	}

	parts := strings.Split(t.Package.FullName, "/")
	for len(parts) > 0 {
		t := e.Targets.Find("//" + strings.Join(parts, "/") + selector)
		if t != nil {
			return t, nil
		}

		parts = parts[:len(parts)-1]
	}

	if must {
		return nil, fmt.Errorf("not target matching %v found in parent of %v", selector, t.FQN)
	}

	return nil, nil
}
