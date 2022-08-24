package engine

import (
	_ "embed"
	"fmt"
	jsoniter "github.com/json-iterator/go"
	"go.starlark.net/starlark"
	"heph/utils"
	"io/fs"
	"runtime"
	"strings"
)

//go:embed predeclared.gotpl
var predeclaredSrc []byte
var predeclaredMod *starlark.Program

func init() {
	_, mod, err := starlark.SourceProgram("<builtin>", predeclaredSrc, predeclared(nil).Has)
	if err != nil {
		panic(err)
	}
	predeclaredMod = mod
}

func predeclared(globals ...starlark.StringDict) starlark.StringDict {
	p := starlark.StringDict{}
	p["internal_target"] = starlark.NewBuiltin("internal_target", internal_target)
	p["glob"] = starlark.NewBuiltin("glob", glob)
	p["package_name"] = starlark.NewBuiltin("package_name", package_name)
	p["package_dir"] = starlark.NewBuiltin("package_dir", package_dir)
	p["package_fqn"] = starlark.NewBuiltin("package_fqn", package_fqn)
	p["get_os"] = starlark.NewBuiltin("get_os", get_os)
	p["get_arch"] = starlark.NewBuiltin("get_arch", get_arch)
	p["to_json"] = starlark.NewBuiltin("to_json", to_json)
	p["fail"] = starlark.NewBuiltin("fail", fail)

	for _, globals := range globals {
		for name, value := range globals {
			if strings.HasPrefix(name, "_") {
				continue
			}

			if _, ok := p[name]; ok {
				panic(fmt.Sprintf("%v is already delcared", name))
			}

			p[name] = value
		}
	}
	p.Freeze()

	return p
}

func listForeach(l *starlark.List, f func(int, starlark.Value) error) error {
	iter := l.Iterate()
	defer iter.Done()

	var i int
	var e starlark.Value
	for iter.Next(&e) {
		err := f(i, e)
		if err != nil {
			return err
		}
		i++
	}

	return nil
}

func getPackage(thread *starlark.Thread) *Package {
	return getEngine(thread).pkg
}

func getEngine(thread *starlark.Thread) *runBuildEngine {
	pkg := thread.Local("engine").(*runBuildEngine)

	if pkg == nil {
		panic("engine is nil, not supposed to happen")
	}

	return pkg
}

func stackTrace(thread *starlark.Thread) []string {
	var source []string
	for _, c := range thread.CallStack() {
		source = append(source, fmt.Sprintf("%v %v", c.Name, c.Pos.String()))
	}

	return source
}

func internal_target(thread *starlark.Thread, fn *starlark.Builtin, args starlark.Tuple, kwargs []starlark.Tuple) (starlark.Value, error) {
	pkg := getPackage(thread)
	e := getEngine(thread)

	var sargs TargetArgs

	if err := starlark.UnpackArgs(
		"target", args, kwargs,
		"name?", &sargs.Name,
		"run?", &sargs.Run,
		"run_in_cwd?", &sargs.RunInCwd,
		"quiet?", &sargs.Quiet,
		"pass_args?", &sargs.PassArgs,
		"pass_env?", &sargs.PassEnv,
		"deps?", &sargs.Deps,
		"hash_deps?", &sargs.HashDeps,
		"cache?", &sargs.Cache,
		"sandbox?", &sargs.SandboxEnabled,
		"codegen?", &sargs.Codegen,
		"tools?", &sargs.Tools,
		"labels?", &sargs.Labels,
		"out?", &sargs.Out,
		"env?", &sargs.Env,
		"gen?", &sargs.Gen,
		"provide?", &sargs.Provide,
		"require_gen?", &sargs.RequireGen,
		"src_env?", &sargs.SrcEnv,
		"out_env?", &sargs.OutEnv,
	); err != nil {
		if sargs.Name != "" {
			return nil, fmt.Errorf("%v: %w", pkg.TargetPath(sargs.Name), err)
		}

		return nil, err
	}

	t, err := specFromArgs(sargs, pkg)
	if err != nil {
		return nil, err
	}

	t.Source = stackTrace(thread)

	err = e.registerTarget(t)
	if err != nil {
		return nil, err
	}

	return starlark.String(t.FQN), nil
}

func glob(thread *starlark.Thread, fn *starlark.Builtin, args starlark.Tuple, kwargs []starlark.Tuple) (starlark.Value, error) {
	pkg := getPackage(thread)

	var (
		pattern string
		exclude ArrayMap
	)

	if err := starlark.UnpackArgs(
		fn.Name(), args, kwargs,
		"pattern", &pattern,
		"exclude?", &exclude,
	); err != nil {
		return nil, err
	}

	elems := make([]starlark.Value, 0)
	err := utils.StarWalk(pkg.Root.Abs, pattern, exclude.Array, func(path string, d fs.DirEntry, err error) error {
		if d.IsDir() {
			return nil
		}

		elems = append(elems, starlark.String(path))

		return nil
	})
	if err != nil {
		return nil, err
	}

	return starlark.NewList(elems), nil
}

func package_name(thread *starlark.Thread, fn *starlark.Builtin, args starlark.Tuple, kwargs []starlark.Tuple) (starlark.Value, error) {
	pkg := getPackage(thread)

	return starlark.String(pkg.Name), nil
}

func package_dir(thread *starlark.Thread, fn *starlark.Builtin, args starlark.Tuple, kwargs []starlark.Tuple) (starlark.Value, error) {
	pkg := getPackage(thread)

	return starlark.String(pkg.FullName), nil
}

func package_fqn(thread *starlark.Thread, fn *starlark.Builtin, args starlark.Tuple, kwargs []starlark.Tuple) (starlark.Value, error) {
	pkg := getPackage(thread)

	return starlark.String("//" + pkg.FullName), nil
}

func get_os(thread *starlark.Thread, fn *starlark.Builtin, args starlark.Tuple, kwargs []starlark.Tuple) (starlark.Value, error) {
	return starlark.String(runtime.GOOS), nil
}

func get_arch(thread *starlark.Thread, fn *starlark.Builtin, args starlark.Tuple, kwargs []starlark.Tuple) (starlark.Value, error) {
	return starlark.String(runtime.GOARCH), nil
}

func to_json(thread *starlark.Thread, fn *starlark.Builtin, args starlark.Tuple, kwargs []starlark.Tuple) (starlark.Value, error) {
	value := args[0]

	var json = jsoniter.ConfigCompatibleWithStandardLibrary

	b, err := json.Marshal(utils.FromStarlark(value))
	if err != nil {
		return nil, err
	}

	return starlark.String(b), nil
}

func fail(thread *starlark.Thread, fn *starlark.Builtin, args starlark.Tuple, kwargs []starlark.Tuple) (starlark.Value, error) {
	value := args[0]

	trace := stackTrace(thread)
	for i, s := range trace {
		trace[i] = "  " + s
	}
	traceStr := strings.Join(trace, "\n")

	if s, ok := value.(starlark.String); ok {
		return nil, fmt.Errorf("%v\n%v", s.GoString(), traceStr)
	}

	return nil, fmt.Errorf("%s\n%v", value, traceStr)
}
