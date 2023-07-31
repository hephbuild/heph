package graph

import (
	"fmt"
	"github.com/hephbuild/heph/exprs"
	"strconv"
	"strings"
)

func (e *State) collect(t *Target, expr exprs.Expr) ([]*Target, error) {
	s, err := expr.MustPosArg(0)
	if err != nil {
		return nil, err
	}

	pkgMatcher := ParseTargetSelector(t.Package.Path, s)

	var includeMatchers TargetMatchers
	var excludeMatchers TargetMatchers
	must := false

	for _, arg := range expr.NamedArgs {
		switch arg.Name {
		case "must":
			must = true
		case "include", "exclude":
			m := ParseTargetSelector(t.Package.Path, arg.Value)
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
	for _, target := range e.Targets().Slice() {
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

func (e *State) findParent(t *Target, expr exprs.Expr) (*Target, error) {
	selector, err := expr.MustPosArg(0)
	if err != nil {
		return nil, err
	}

	must, _ := strconv.ParseBool(expr.NamedArg("must"))

	if !strings.HasPrefix(selector, ":") {
		return nil, fmt.Errorf("must be a target selector, got `%v`", selector)
	}

	parts := strings.Split(t.Package.Path, "/")
	for len(parts) > 0 {
		t := e.targets.Find("//" + strings.Join(parts, "/") + selector)
		if t != nil {
			return t, nil
		}

		parts = parts[:len(parts)-1]
	}

	if must {
		return nil, fmt.Errorf("not target matching %v found in parent of %v", selector, t.Addr)
	}

	return nil, nil
}
