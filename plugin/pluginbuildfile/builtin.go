package pluginbuildfile

import (
	"errors"
	"fmt"
	"strings"

	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/internal/hstarlark"
	"github.com/hephbuild/heph/lib/tref"
	"go.starlark.net/starlark"
)

type BuiltinFunc = func(
	thread *starlark.Thread,
	fn *starlark.Builtin,
	args starlark.Tuple,
	kwargs []starlark.Tuple,
) (starlark.Value, error)

func (p *Plugin) builtinFile(name string) BuiltinFunc {
	return func(
		thread *starlark.Thread,
		fn *starlark.Builtin,
		args starlark.Tuple,
		kwargs []starlark.Tuple,
	) (starlark.Value, error) {
		execCtx := getExecContext(thread)

		var pattern string
		var exclude hstarlark.List[string]
		if err := starlark.UnpackArgs(
			name, args, kwargs,
			"pattern", &pattern,
			"exclude?", &exclude,
		); err != nil {
			return nil, err
		}

		base, pattern := hfs.GlobSplit(pattern)

		if strings.HasPrefix(base, "/") {
			return starlark.String(tref.FormatGlob(base, pattern, exclude)), nil
		}

		return starlark.String(tref.FormatGlob(tref.JoinPackage(execCtx.Package, base), pattern, exclude)), nil
	}
}

func (p *Plugin) builtinGetPkg() BuiltinFunc {
	return func(
		thread *starlark.Thread,
		fn *starlark.Builtin,
		args starlark.Tuple,
		kwargs []starlark.Tuple,
	) (starlark.Value, error) {
		execCtx := getExecContext(thread)

		return starlark.String(execCtx.Package), nil
	}
}

func (p *Plugin) builtinTarget() BuiltinFunc {
	return func(
		thread *starlark.Thread,
		fn *starlark.Builtin,
		args starlark.Tuple,
		kwargs []starlark.Tuple,
	) (starlark.Value, error) {
		execCtx := getExecContext(thread)

		var fkwargs []starlark.Tuple
		var otherkwargs = map[string]starlark.Value{}
		for _, item := range kwargs {
			name, arg := item[0].(starlark.String), item[1] //nolint:errcheck

			switch name {
			case "name", "driver", "labels", "transitive":
				fkwargs = append(fkwargs, item)
			default:
				otherkwargs[string(name)] = arg
			}
		}

		var tspec TargetSpec
		tspec.Driver = "bash"

		if err := starlark.UnpackArgs(
			"target", args, fkwargs,
			"name", &tspec.Name,
			"driver?", &tspec.Driver,
			"labels?", &tspec.Labels,
			"transitive?", &tspec.Transitive,
		); err != nil {
			if tspec.Name != "" {
				return nil, fmt.Errorf("%v: %w", tspec.Name, err)
			}

			return nil, err
		}

		payload := OnTargetPayload{
			Name:       tspec.Name,
			Package:    execCtx.Package,
			Driver:     tspec.Driver,
			Labels:     tspec.Labels,
			Transitive: tspec.Transitive,
			Args:       otherkwargs,
		}

		if payload.Name == "" {
			return nil, errors.New("missing name")
		}

		err := execCtx.OnTarget(execCtx.Ctx, payload)
		if err != nil {
			return nil, err
		}

		return starlark.String(tref.Format(tref.New(execCtx.Package, payload.Name, nil))), nil
	}
}

func (p *Plugin) builtinProviderState() BuiltinFunc {
	return func(
		thread *starlark.Thread,
		fn *starlark.Builtin,
		args starlark.Tuple,
		kwargs []starlark.Tuple,
	) (starlark.Value, error) {
		execCtx := getExecContext(thread)

		var fkwargs []starlark.Tuple
		var otherkwargs = map[string]starlark.Value{}
		for _, item := range kwargs {
			name, arg := item[0].(starlark.String), item[1] //nolint:errcheck

			switch name {
			case "provider":
				fkwargs = append(fkwargs, item)
			default:
				otherkwargs[string(name)] = arg
			}
		}

		payload := OnProviderStatePayload{
			Package: execCtx.Package,
			Args:    otherkwargs,
		}
		if err := starlark.UnpackArgs(
			"provider_state", args, fkwargs,
			"provider?", &payload.Provider,
		); err != nil {
			return nil, err
		}

		if payload.Provider == "" {
			return nil, errors.New("missing provider")
		}

		err := execCtx.OnProviderState(execCtx.Ctx, payload)
		if err != nil {
			return nil, err
		}

		return starlark.None, nil
	}
}

func (p *Plugin) builtinfsglob() BuiltinFunc {
	return func(
		thread *starlark.Thread,
		fn *starlark.Builtin,
		args starlark.Tuple,
		kwargs []starlark.Tuple,
	) (starlark.Value, error) {
		execCtx := getExecContext(thread)

		var pattern string
		var exclude hstarlark.List[string]
		if err := starlark.UnpackArgs(
			fn.Name(), args, kwargs,
			"pattern", &pattern,
			"exclude?", &exclude,
		); err != nil {
			return nil, err
		}

		paths := starlark.NewList(nil)
		err := hfs.Glob(execCtx.Ctx, p.repoRoot.At(tref.ToOSPath(execCtx.Package)), pattern, exclude, func(path string, d hfs.DirEntry) error {
			return paths.Append(starlark.String(path))
		})
		if err != nil {
			return nil, err
		}

		return paths, nil
	}
}
