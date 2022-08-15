package engine

import (
	"fmt"
	"go.starlark.net/starlark"
)

type TargetArgs struct {
	Name           string
	Run            Runnable
	RunInCwd       bool
	Quiet          bool
	PassArgs       bool
	Cache          BoolArray
	SandboxEnabled bool
	Gen            bool
	Codegen        string
	Deps           ArrayMap
	HashDeps       ArrayMap
	Tools          ArrayMap
	Labels         ArrayMap
	Out            ArrayMap
	Env            ArrayMap
	PassEnv        ArrayMap
	Provide        ArrayMap
	RequireGen     bool
}

type Runnable struct {
	Cmds     []string
	Callable starlark.Callable
}

func (r *Runnable) Unpack(v starlark.Value) error {
	switch v := v.(type) {
	case starlark.Callable:
		r.Callable = v
		return nil
	case starlark.String:
		r.Cmds = []string{string(v)}
		return nil
	case *starlark.List:
		var a ArrayMap
		err := a.Unpack(v)
		if err != nil {
			return err
		}

		r.Cmds = a.Array
		return nil
	}

	return fmt.Errorf("must be string or []string, is %v", v.Type())
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
