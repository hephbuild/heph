package engine

import (
	"fmt"
	jsoniter "github.com/json-iterator/go"
	"go.starlark.net/starlark"
	"heph/utils"
	"io/fs"
	"path/filepath"
	"runtime"
)

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

type BoolArray struct {
	Bool  bool
	Array []string
}

func (d *BoolArray) Unpack(v starlark.Value) error {
	switch e := v.(type) {
	case starlark.Bool:
		*d = BoolArray{
			Bool: bool(e),
		}
		return nil
	case *starlark.List:
		arr := make([]string, 0)
		err := listForeach(e, func(i int, value starlark.Value) error {
			arr = append(arr, value.(starlark.String).GoString())
			return nil
		})
		if err != nil {
			return err
		}

		*d = BoolArray{
			Bool:  true,
			Array: arr,
		}
		return nil
	}

	return fmt.Errorf("must be bool or array, got %v", v.Type())
}

type ArrayMap struct {
	Array []string
	Map   map[string]string
}

func (d *ArrayMap) Unpack(v starlark.Value) error {
	arr := make([]string, 0)
	mapp := map[string]string{}

	vd, ok := v.(*starlark.Dict)
	if ok {
		for _, e := range vd.Items() {
			keyv := e.Index(0)
			key, ok := keyv.(starlark.String)
			if !ok {
				return fmt.Errorf("key must be string, got %v", keyv.Type())
			}

			valv := e.Index(1)
			val, ok := valv.(starlark.String)
			if !ok {
				return fmt.Errorf("val must be string, got %v", valv.Type())
			}

			arr = append(arr, string(val))
			mapp[string(key)] = string(val)
		}

		*d = ArrayMap{
			Array: arr,
			Map:   mapp,
		}
		return nil
	}

	vs, ok := v.(starlark.String)
	if ok {
		*d = ArrayMap{
			Array: []string{string(vs)},
		}
		return nil
	}

	vl, ok := v.(*starlark.List)
	if ok {
		err := listForeach(vl, func(i int, value starlark.Value) error {
			switch e := value.(type) {
			case starlark.String:
				arr = append(arr, string(e))
				return nil
			case starlark.Tuple:
				keyv := e.Index(0)
				key, ok := keyv.(starlark.String)
				if !ok {
					return fmt.Errorf("key must be string, got %v", keyv.Type())
				}

				depv := e.Index(1)
				dep, ok := depv.(starlark.String)
				if !ok {
					return fmt.Errorf("dep must be string, got %v", depv.Type())
				}

				arr = append(arr, string(dep))
				mapp[string(dep)] = string(key)
				return nil
			case *starlark.List:
				if e.Len() == 0 {
					return nil
				}

				err := listForeach(e, func(i int, value starlark.Value) error {
					dep, ok := value.(starlark.String)
					if !ok {
						return fmt.Errorf("dep must be string, got %v", dep.Type())
					}

					arr = append(arr, string(dep))
					return nil
				})
				return err
			}

			return fmt.Errorf("element at index %v must be string or (string, string), is %v", i, value.Type())
		})
		if err != nil {
			return err
		}

		*d = ArrayMap{
			Array: arr,
			Map:   mapp,
		}

		return nil
	}

	return fmt.Errorf("must be dict, list or string, got %v", v.Type())
}

func (e *runBuildEngine) getPackage(thread *starlark.Thread) *Package {
	pkg := thread.Local("pkg").(*Package)

	if pkg == nil {
		panic("pkg is nil, not supposed to happen")
	}

	return pkg
}

func (e *runBuildEngine) target(thread *starlark.Thread, fn *starlark.Builtin, args starlark.Tuple, kwargs []starlark.Tuple) (starlark.Value, error) {
	pkg := e.getPackage(thread)

	var sargs starlarkTargetArgs

	if err := starlark.UnpackArgs(
		"target", args, kwargs,
		"name?", &sargs.name,
		"run?", &sargs.run,
		"run_in_cwd?", &sargs.runInCwd,
		"quiet?", &sargs.quiet,
		"pass_args?", &sargs.passArgs,
		"pass_env?", &sargs.passEnv,
		"deps?", &sargs.deps,
		"hash_deps?", &sargs.hashDeps,
		"cache?", &sargs.cache,
		"sandbox?", &sargs.sandboxEnabled,
		"codegen?", &sargs.codegen,
		"tools?", &sargs.tools,
		"labels?", &sargs.labels,
		"out?", &sargs.out,
		"env?", &sargs.env,
		"gen?", &sargs.gen,
		"provide?", &sargs.provide,
		"require_gen?", &sargs.requireGen,
	); err != nil {
		if sargs.name != "" {
			return nil, fmt.Errorf("%v: %w", pkg.TargetPath(sargs.name), err)
		}

		return nil, err
	}

	t, err := specFromArgs(sargs, pkg)
	if err != nil {
		return nil, err
	}

	var source []string
	for _, c := range thread.CallStack() {
		source = append(source, fmt.Sprintf("%v %v", c.Name, c.Pos.String()))
	}

	t.Source = source

	err = e.registerTarget(t)
	if err != nil {
		return nil, err
	}

	return starlark.String(t.FQN), nil
}

func (e *runBuildEngine) glob(thread *starlark.Thread, fn *starlark.Builtin, args starlark.Tuple, kwargs []starlark.Tuple) (starlark.Value, error) {
	pkg := e.getPackage(thread)

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
	allExclude = append(allExclude, filepath.Base(e.HomeDir))

	elems := make([]starlark.Value, 0)
	err := utils.StarWalk(pkg.Root.Abs, pattern, allExclude, func(path string, d fs.DirEntry, err error) error {
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

func (e *runBuildEngine) package_name(thread *starlark.Thread, fn *starlark.Builtin, args starlark.Tuple, kwargs []starlark.Tuple) (starlark.Value, error) {
	pkg := e.getPackage(thread)

	return starlark.String(pkg.Name), nil
}

func (e *runBuildEngine) package_fqn(thread *starlark.Thread, fn *starlark.Builtin, args starlark.Tuple, kwargs []starlark.Tuple) (starlark.Value, error) {
	pkg := e.getPackage(thread)

	return starlark.String("//" + pkg.FullName), nil
}

func (e *runBuildEngine) get_os(thread *starlark.Thread, fn *starlark.Builtin, args starlark.Tuple, kwargs []starlark.Tuple) (starlark.Value, error) {
	return starlark.String(runtime.GOOS), nil
}

func (e *runBuildEngine) get_arch(thread *starlark.Thread, fn *starlark.Builtin, args starlark.Tuple, kwargs []starlark.Tuple) (starlark.Value, error) {
	return starlark.String(runtime.GOARCH), nil
}

func (e *runBuildEngine) to_json(thread *starlark.Thread, fn *starlark.Builtin, args starlark.Tuple, kwargs []starlark.Tuple) (starlark.Value, error) {
	value := args[0]

	var json = jsoniter.ConfigCompatibleWithStandardLibrary

	b, err := json.Marshal(utils.FromStarlark(value))
	if err != nil {
		return nil, err
	}

	return starlark.String(b), nil
}
