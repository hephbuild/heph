package internal

import (
	"errors"
	"fmt"
	"strings"
)

type Ref2 struct {
	Pkg     string
	Name    string
	Args    map[string]string
	Out     *string
	Filters string
}

func FastParse(s, pkg string, optPkg bool) (Ref2, error) {
	return fastParse(s, pkg, optPkg, false)
}

func FastParseWithOut(s, pkg string, optPkg bool) (Ref2, error) {
	return fastParse(s, pkg, optPkg, true)
}

func fastParse(s, pkg string, optPkg, withOut bool) (Ref2, error) {
	if optPkg && IsRelative(s) {
		s = "//" + pkg + s
	}

	p := &parser2{withOut: withOut}

	return p.parse(s)
}

type parser2 struct {
	withOut bool
}

func (p parser2) parse(s string) (Ref2, error) {
	s, err := p.parseRoot(s)
	if err != nil {
		return Ref2{}, err
	}

	pkg, rest, err := p.parsePackage(s)
	if err != nil {
		return Ref2{}, err
	}

	name, rest, err := p.parseName(rest)
	if err != nil {
		return Ref2{}, err
	}

	args, rest, err := p.parseArgs(rest)
	if err != nil {
		return Ref2{}, err
	}

	var out *string
	var filters string
	if p.withOut {
		out, rest, err = p.parseOut(rest)
		if err != nil {
			return Ref2{}, err
		}

		filters, rest, err = p.parseFilters(rest)
		if err != nil {
			return Ref2{}, err
		}
	}

	if rest != "" {
		return Ref2{}, fmt.Errorf("unexpected: %q", rest)
	}

	return Ref2{
		Pkg:     pkg,
		Name:    name,
		Args:    args,
		Out:     out,
		Filters: filters,
	}, nil
}

func (p parser2) parseRoot(s string) (string, error) {
	rest, ok := strings.CutPrefix(s, "//")
	if !ok {
		return "", errors.New("expected //")
	}

	return rest, nil
}

func (p parser2) parsePackage(s string) (string, string, error) {
	i := strings.Index(s, ":")
	if i < 0 {
		return "", "", errors.New("expected :") //nolint:staticcheck
	}

	return s[:i], s[i:], nil
}

func (p parser2) parseName(s string) (string, string, error) {
	s, ok := strings.CutPrefix(s, ":")
	if !ok {
		return "", "", errors.New("expected :") //nolint:staticcheck
	}

	i := strings.IndexAny(s, "@| ")
	if i > 0 {
		return s[:i], s[i:], nil
	}

	return s, "", nil
}

func (p parser2) parseArgs(rest string) (map[string]string, string, error) {
	rest, ok := strings.CutPrefix(rest, "@")
	if !ok {
		return nil, rest, nil
	}

	var argsStr string
	i := strings.Index(rest, "|")
	if i > 0 {
		argsStr = rest[:i]
		rest = rest[i:]
	} else {
		argsStr = rest
		rest = ""
	}

	args := map[string]string{}
	pairs := strings.Split(argsStr, ",")
	for _, pair := range pairs {
		k, v, ok := strings.Cut(pair, "=")
		if !ok {
			return nil, "", errors.New("expected =")
		}

		args[k] = v
	}

	return args, rest, nil
}

func (p parser2) parseOut(rest string) (*string, string, error) {
	rest, ok := strings.CutPrefix(rest, "|")
	if !ok {
		return nil, rest, nil
	}

	var out string
	i := strings.Index(rest, " ")
	if i > 0 {
		out = rest[:i]
		rest = rest[i:]
	} else {
		out = rest
		rest = ""
	}

	return &out, rest, nil
}

func (p parser2) parseFilters(rest string) (string, string, error) {
	rest, ok := strings.CutPrefix(rest, " filters=")
	if !ok {
		return "", rest, nil
	}

	var filters string
	i := strings.Index(rest, " ")
	if i > 0 {
		filters = rest[:i]
		rest = rest[i+1:]
	} else {
		filters = rest
		rest = ""
	}

	return filters, rest, nil
}
