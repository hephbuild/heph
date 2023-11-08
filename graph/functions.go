package graph

import (
	"fmt"
	"github.com/hephbuild/heph/exprs"
	"github.com/hephbuild/heph/specs"
	"strconv"
	"strings"
)

func (e *State) collectMatcher(t specs.Target, expr exprs.Expr) (specs.Matcher, bool, error) {
	s, err := expr.MustPosArg(0)
	if err != nil {
		return nil, false, err
	}

	pkgMatcher, err := specs.ParseMatcherInPkg(t.Package.Path, s)
	if err != nil {
		return nil, false, err
	}

	var include, exclude []string
	must := false

	for _, arg := range expr.NamedArgs {
		switch arg.Name {
		case "must":
			must = true
		case "include", "exclude":
			if arg.Name == "exclude" {
				exclude = append(exclude, arg.Value)
			} else {
				include = append(include, arg.Value)
			}
		default:
			return nil, false, fmt.Errorf("unhandled %v arg `%v`", expr.Function, arg.Name)
		}
	}

	matcher, err := specs.MatcherFromIncludeExclude(t.Package.Path, include, exclude)
	if err != nil {
		return nil, false, err
	}

	if matcher != nil {
		matcher = specs.AndNodeFactory(pkgMatcher, matcher)
	} else {
		matcher = pkgMatcher
	}

	return matcher, must, nil
}

func (e *State) collect(t *Target, expr exprs.Expr) ([]*Target, error) {
	matcher, must, err := e.collectMatcher(t.Spec(), expr)
	if err != nil {
		return nil, err
	}

	targets, err := e.Targets().Filter(matcher)
	if err != nil {
		return nil, err
	}

	if must && targets.Len() == 0 {
		return nil, fmt.Errorf("must match a target, found none")
	}

	return targets.Slice(), nil
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
