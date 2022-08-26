package engine

import (
	"fmt"
	"heph/utils"
	"path/filepath"
	"strings"
)

func (e *Engine) outdir(t *Target, expr *utils.Expr) (string, error) {
	return filepath.Join(t.OutRoot.Abs, t.Package.FullName), nil
}

func (e *Engine) collect(t *Target, expr *utils.Expr) (Targets, error) {
	pkgMatcher := ParseTargetSelector(t.Package.FullName, expr.PosArgs[0])

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

	targets := make(Targets, 0)
	for _, target := range e.Targets {
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

func (e *Engine) findParent(t *Target, expr *utils.Expr) (*Target, error) {
	selector := expr.PosArgs[0]

	if !strings.HasPrefix(selector, ":") {
		return nil, fmt.Errorf("must be a target selector, got `%v`", selector)
	}

	parts := strings.Split(t.Package.FullName, "/")
	for len(parts) > 0 {
		t := e.Targets.Find("//" + strings.Join(parts, "/") + selector)
		if t != nil {
			return t, nil
		}
	}

	return nil, fmt.Errorf("not target matching %v found in parent of %v", selector, t.FQN)
}
