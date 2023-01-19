package engine

import (
	_ "embed"
	"errors"
	"fmt"
	"go.starlark.net/starlark"
	"go.starlark.net/starlarkjson"
	"go.starlark.net/starlarkstruct"
	"heph/packages"
	"heph/targetspec"
	"heph/utils"
	"heph/utils/hash"
	"io/fs"
	"path/filepath"
	"runtime"
	"strings"
	"sync"
)

//go:embed predeclared.gotpl
var predeclaredSrc []byte
var predeclaredGlobals starlark.StringDict
var predeclaredHash string
var predeclaredOnce sync.Once

func predeclaredGlobalsOnce(config starlark.StringDict) {
	predeclaredOnce.Do(func() {
		computePredeclaredGlobals(config)
	})
}

func computePredeclaredGlobals(config starlark.StringDict) {
	_, mod, err := starlark.SourceProgram("<builtin>", predeclaredSrc, predeclared(nil).Has)
	if err != nil {
		panic(err)
	}

	thread := newStarlarkThread()

	globals, err := mod.Init(thread, predeclared(config))
	if err != nil {
		var eerr *starlark.EvalError
		if errors.As(err, &eerr) {
			panic(fmt.Errorf("%v: %v", eerr.Msg, eerr.Backtrace()))
		}
		panic(err)
	}
	predeclaredGlobals = globals
	predeclaredHash = hash.HashBytes(predeclaredSrc)
}

func predeclared(globals ...starlark.StringDict) starlark.StringDict {
	p := starlark.StringDict{}
	p["_internal_target"] = starlark.NewBuiltin("_internal_target", internal_target)
	p["glob"] = starlark.NewBuiltin("glob", glob)
	p["get_os"] = starlark.NewBuiltin("get_os", get_os)
	p["get_arch"] = starlark.NewBuiltin("get_arch", get_arch)
	p["to_json"] = starlark.NewBuiltin("to_json", to_json)
	p["fail"] = starlark.NewBuiltin("fail", fail)
	p["struct"] = starlark.NewBuiltin("struct", starlarkstruct.Make)
	p["heph"] = &starlarkstruct.Module{
		Name: "heph",
		Members: starlark.StringDict{
			"canonicalize": starlark.NewBuiltin("heph.canonicalize", canonicalize),
			"is_target":    starlark.NewBuiltin("heph.is_target", is_target),
			"split":        starlark.NewBuiltin("heph.split", split),
			"param":        starlark.NewBuiltin("heph.param", param),
			"cache":        starlark.NewBuiltin("heph.cache", starlarkstruct.Make),
			"target_spec":  starlark.NewBuiltin("heph.target_spec", starlarkstruct.Make),
			//"normalize_target_name": starlark.NewBuiltin("heph.normalize_target_name", normalize_target_name),
			//"normalize_pkg_name":    starlark.NewBuiltin("heph.normalize_target_name", normalize_pkg_name),
			"pkg": &starlarkstruct.Module{
				Name: "heph.pkg",
				Members: starlark.StringDict{
					"name": starlark.NewBuiltin("heph.pkg.name", package_name),
					"dir":  starlark.NewBuiltin("heph.pkg.dir", package_dir),
					"addr": starlark.NewBuiltin("heph.pkg.addr", package_fqn),
				},
			},
			"path": &starlarkstruct.Module{
				Name: "heph.path",
				Members: starlark.StringDict{
					"base": starlark.NewBuiltin("heph.path.base", path_base),
					"dir":  starlark.NewBuiltin("heph.path.dir", path_dir),
					"join": starlark.NewBuiltin("heph.path.join", path_join),
				},
			},
		},
	}

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

func getPackage(thread *starlark.Thread) *packages.Package {
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
		"pkg?", &sargs.Pkg,
		"run?", &sargs.Run,
		"_file_content?", &sargs.FileContent,
		"executor?", &sargs.Executor,
		"run_in_cwd?", &sargs.RunInCwd,
		"quiet?", &sargs.Quiet,
		"pass_args?", &sargs.PassArgs,
		"pass_env?", &sargs.PassEnv,
		"deps?", &sargs.Deps,
		"hash_deps?", &sargs.HashDeps,
		"cache?", &sargs.Cache,
		"restore_cache?", &sargs.RestoreCache,
		"sandbox?", &sargs.SandboxEnabled,
		"out_in_sandbox?", &sargs.OutInSandbox,
		"codegen?", &sargs.Codegen,
		"tools?", &sargs.Tools,
		"labels?", &sargs.Labels,
		"out?", &sargs.Out,
		"support_files?", &sargs.SupportFiles,
		"env?", &sargs.Env,
		"gen?", &sargs.Gen,
		"runtime_env?", &sargs.RuntimeEnv,
		"src_env?", &sargs.SrcEnv,
		"out_env?", &sargs.OutEnv,
		"hash_file?", &sargs.HashFile,
		"transitive?", &sargs.Transitive,
		"timeout?", &sargs.Timeout,
	); err != nil {
		if sargs.Name != "" {
			return nil, fmt.Errorf("%v: %w", pkg.TargetPath(sargs.Name), err)
		}

		return nil, err
	}

	if sargs.Pkg != "" {
		tp, err := targetspec.TargetParse("", sargs.Pkg)
		if err != nil {
			return nil, err
		}

		pkg = e.createPkg(tp.Package)
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
	e := getEngine(thread)

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

	allExclude := exclude.Array
	allExclude = append(allExclude, "**/.heph")
	allExclude = append(allExclude, e.Config.Glob.Exclude...)

	elems := make([]starlark.Value, 0)
	err := utils.StarWalk(pkg.Root.Abs(), pattern, allExclude, func(path string, d fs.DirEntry, err error) error {
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
	return starlarkjson.Module.Members["encode"].(*starlark.Builtin).CallInternal(thread, args, kwargs)
}

func fail(thread *starlark.Thread, fn *starlark.Builtin, args starlark.Tuple, kwargs []starlark.Tuple) (starlark.Value, error) {
	var value starlark.Value
	if err := starlark.UnpackPositionalArgs(fn.Name(), args, kwargs, 1, &value); err != nil {
		return nil, err
	}

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

func canonicalize(thread *starlark.Thread, fn *starlark.Builtin, args starlark.Tuple, kwargs []starlark.Tuple) (starlark.Value, error) {
	var (
		value string
	)

	if err := starlark.UnpackArgs(
		fn.Name(), args, kwargs,
		"value", &value,
	); err != nil {
		return nil, err
	}

	pkg := getPackage(thread)

	tp, err := targetspec.TargetOutputParse(pkg.FullName, value)
	if err != nil {
		return nil, err
	}

	return starlark.String(tp.Full()), nil
}

func is_target(thread *starlark.Thread, fn *starlark.Builtin, args starlark.Tuple, kwargs []starlark.Tuple) (starlark.Value, error) {
	var (
		value string
	)

	if err := starlark.UnpackArgs(
		fn.Name(), args, kwargs,
		"value", &value,
	); err != nil {
		return nil, err
	}

	return starlark.Bool(strings.HasPrefix(value, ":") || strings.HasPrefix(value, "//")), nil
}

func split(thread *starlark.Thread, fn *starlark.Builtin, args starlark.Tuple, kwargs []starlark.Tuple) (starlark.Value, error) {
	var (
		value string
	)

	if err := starlark.UnpackArgs(
		fn.Name(), args, kwargs,
		"value", &value,
	); err != nil {
		return nil, err
	}

	pkg := getPackage(thread)

	tp, err := targetspec.TargetOutputParse(pkg.FullName, value)
	if err != nil {
		return nil, err
	}

	return starlark.Tuple{
		starlark.String(tp.Package),
		starlark.String(tp.Name),
		starlark.String(tp.Output),
	}, nil
}

func param(thread *starlark.Thread, fn *starlark.Builtin, args starlark.Tuple, kwargs []starlark.Tuple) (starlark.Value, error) {
	var (
		name string
	)

	if err := starlark.UnpackArgs(
		fn.Name(), args, kwargs,
		"name", &name,
	); err != nil {
		return nil, err
	}

	engine := getEngine(thread)

	return starlark.String(engine.Params[name]), nil
}

func path_base(thread *starlark.Thread, fn *starlark.Builtin, args starlark.Tuple, kwargs []starlark.Tuple) (starlark.Value, error) {
	var (
		path string
	)

	if err := starlark.UnpackArgs(
		fn.Name(), args, kwargs,
		"path", &path,
	); err != nil {
		return nil, err
	}

	return starlark.String(filepath.Base(path)), nil
}

func path_dir(thread *starlark.Thread, fn *starlark.Builtin, args starlark.Tuple, kwargs []starlark.Tuple) (starlark.Value, error) {
	var (
		path string
	)

	if err := starlark.UnpackArgs(
		fn.Name(), args, kwargs,
		"path", &path,
	); err != nil {
		return nil, err
	}

	return starlark.String(filepath.Dir(path)), nil
}

func path_join(thread *starlark.Thread, fn *starlark.Builtin, args starlark.Tuple, kwargs []starlark.Tuple) (starlark.Value, error) {
	parts := make([]string, 0, len(args))
	for _, arg := range args {
		parts = append(parts, string(arg.(starlark.String)))
	}
	return starlark.String(filepath.Join(parts...)), nil
}
