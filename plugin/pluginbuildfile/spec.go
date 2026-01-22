package pluginbuildfile

import (
	"fmt"

	"github.com/hephbuild/heph/internal/hstarlark"
	"go.starlark.net/starlark"
)

type TargetSpec struct {
	Name       string
	Driver     string
	Labels     hstarlark.List[string]
	Transitive TransitiveSpec
}

type TransitiveSpec struct {
	Tools          hstarlark.List[string]
	Deps           TransitiveSpecDeps
	PassEnv        hstarlark.List[string]
	RuntimePassEnv hstarlark.List[string]
}

type TransitiveSpecDeps map[string][]string

func (c *TransitiveSpecDeps) Unpack(v starlark.Value) error {
	if _, ok := v.(starlark.NoneType); ok {
		return nil
	}

	var vls hstarlark.List[string]
	err := vls.Unpack(v)
	if err == nil {
		*c = map[string][]string{
			"": vls,
		}

		return nil
	}

	if v, ok := v.(*starlark.Dict); ok {
		var m hstarlark.Distruct
		err = hstarlark.UnpackOne(v, &m)
		if err == nil {
			*c = map[string][]string{}
			for _, v := range m.Items() {
				var vv hstarlark.List[string]
				err = hstarlark.UnpackOne(v.Value, &vv)
				if err != nil {
					return err
				}

				(*c)[v.Key] = vv
			}

			return nil
		}
	}

	return fmt.Errorf("expected string, []string, map[string]string or map[string][]string, got %v", v.Type())
}

func (ts *TransitiveSpec) Empty() bool {
	return len(ts.Tools) == 0 && len(ts.PassEnv) == 0 && len(ts.RuntimePassEnv) == 0 && len(ts.Deps) == 0
}

func (c *TransitiveSpec) Unpack(v starlark.Value) error {
	if _, ok := v.(starlark.NoneType); ok {
		return nil
	}

	d, err := hstarlark.UnpackDistruct(v)
	if err != nil {
		return err
	}

	var cs TransitiveSpec
	err = starlark.UnpackArgs("", nil, d.Items().Tuples(),
		"tools?", &cs.Tools,
		"deps?", &cs.Deps,
		"pass_env?", &cs.PassEnv,
		"runtime_pass_env?", &cs.RuntimePassEnv,
	)
	if err != nil {
		return err
	}

	*c = cs

	return nil
}
