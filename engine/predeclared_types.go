package engine

import (
	"fmt"
	"go.starlark.net/starlark"
	"go.starlark.net/starlarkstruct"
)

type TargetArgs struct {
	Name           string
	Run            ArrayMap
	FileContent    string
	Executor       string
	RunInCwd       bool
	Quiet          bool
	PassArgs       bool
	Cache          TargetArgsCache
	SandboxEnabled bool
	OutInSandbox   bool
	Gen            bool
	Codegen        string
	Deps           ArrayMap
	HashDeps       ArrayMap
	Tools          ArrayMap
	Labels         ArrayMap
	Out            ArrayMap
	Env            ArrayMap
	PassEnv        ArrayMap
	RuntimeEnv     ArrayMap
	SrcEnv         string
	OutEnv         string
	HashFile       string
	Transitive     TargetArgsTransitive
}

type TargetArgsTransitive struct {
	Deps    ArrayMap
	Tools   ArrayMap
	Env     ArrayMap
	PassEnv ArrayMap
}

func (c *TargetArgsTransitive) Unpack(v starlark.Value) error {
	d, ok := v.(*starlarkstruct.Struct)
	if ok {
		cs := TargetArgsTransitive{}

		for _, n := range d.AttrNames() {
			v, err := d.Attr(n)
			if err != nil {
				return err
			}

			switch n {
			case "deps":
				err := cs.Deps.Unpack(v)
				if err != nil {
					return err
				}
			case "tools":
				err := cs.Tools.Unpack(v)
				if err != nil {
					return err
				}
			case "env":
				err := cs.Env.Unpack(v)
				if err != nil {
					return err
				}
			case "pass_env":
				err := cs.PassEnv.Unpack(v)
				if err != nil {
					return err
				}
			default:
				return fmt.Errorf("invalid arg %v, call heph.target_spec()", n)
			}
		}

		*c = cs
		return nil
	}

	return fmt.Errorf("transitive must call heph.target_spec()")
}

type TargetArgsCache struct {
	Enabled bool
	Named   Array
	Files   Array
}

func (c *TargetArgsCache) Unpack(v starlark.Value) error {
	d, ok := v.(*starlarkstruct.Struct)
	if ok {
		cs := TargetArgsCache{
			Enabled: true,
		}

		for _, n := range d.AttrNames() {
			v, err := d.Attr(n)
			if err != nil {
				return err
			}

			switch n {
			case "named":
				err := cs.Named.Unpack(v)
				if err != nil {
					return err
				}
			case "files":
				err := cs.Files.Unpack(v)
				if err != nil {
					return err
				}
			default:
				return fmt.Errorf("invalid arg %v, call heph.cache()", n)
			}
		}

		*c = cs
		return nil
	}

	var ba BoolArray
	err := ba.Unpack(v)
	if err == nil {
		*c = TargetArgsCache{
			Enabled: ba.Bool,
			Files:   ba.Array,
		}
		return nil
	}

	return fmt.Errorf("cache must be bool, array, or call heph.cache()")
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

type Array []string

func (d *Array) Unpack(v starlark.Value) error {
	if _, ok := v.(starlark.NoneType); ok {
		return nil
	}

	vb, ok := v.(starlark.Bool)
	if ok {
		if vb {
			return fmt.Errorf("True is not a valid value")
		}

		*d = Array{}
		return nil
	}

	vs, ok := v.(starlark.String)
	if ok {
		*d = Array{string(vs)}
		return nil
	}

	vl, ok := v.(*starlark.List)
	if ok {
		arr := make([]string, 0)

		err := listForeach(vl, func(i int, value starlark.Value) error {
			switch e := value.(type) {
			case starlark.NoneType:
				// ignore
				return nil
			case starlark.String:
				arr = append(arr, string(e))
				return nil
			case *starlark.List:
				if e.Len() == 0 {
					return nil
				}

				err := listForeach(e, func(i int, value starlark.Value) error {
					if _, ok := value.(starlark.NoneType); ok {
						return nil
					}

					dep, ok := value.(starlark.String)
					if !ok {
						return fmt.Errorf("dep must be string, got %v", value.Type())
					}

					arr = append(arr, string(dep))
					return nil
				})
				return err
			}

			return fmt.Errorf("element at index %v must be string, got %v", i, value.Type())
		})
		if err != nil {
			return err
		}

		*d = arr

		return nil
	}

	return fmt.Errorf("must be list or string, got %v", v.Type())
}

type ArrayMap struct {
	Array []string
	// TODO Separate StrMap and ArrMap
	StrMap map[string]string
	ArrMap map[string][]string
}

func (d *ArrayMap) Unpack(v starlark.Value) error {
	if _, ok := v.(starlark.NoneType); ok {
		return nil
	}

	vs, ok := v.(starlark.String)
	if ok {
		*d = ArrayMap{
			Array: []string{string(vs)},
		}
		return nil
	}

	arr := make([]string, 0)
	arrMap := map[string][]string{}
	strMap := map[string]string{}

	vd, ok := v.(*starlark.Dict)
	if ok {
		for _, e := range vd.Items() {
			keyv := e.Index(0)
			skey, ok := keyv.(starlark.String)
			if !ok {
				return fmt.Errorf("key must be string, got %v", keyv.Type())
			}

			key := string(skey)

			valv := e.Index(1)
			switch val := valv.(type) {
			case starlark.NoneType:
				continue
			case starlark.String:
				arr = append(arr, string(val))
				strMap[key] = string(val)
				arrMap[key] = append(arrMap[key], string(val))
			case *starlark.List:
				err := listForeach(val, func(i int, value starlark.Value) error {
					if _, ok := value.(starlark.NoneType); ok {
						return nil
					}

					val, ok := value.(starlark.String)
					if !ok {
						return fmt.Errorf("value must be string, got %v", value.Type())
					}

					arr = append(arr, string(val))
					strMap[key] = string(val)
					arrMap[key] = append(arrMap[key], string(val))

					return nil
				})
				if err != nil {
					return err
				}
			default:
				return fmt.Errorf("val must be string or []string, got %v", valv.Type())
			}
		}

		*d = ArrayMap{
			Array:  arr,
			StrMap: strMap,
			ArrMap: arrMap,
		}
		return nil
	}

	vl, ok := v.(*starlark.List)
	if ok {
		err := listForeach(vl, func(i int, value starlark.Value) error {
			switch e := value.(type) {
			case starlark.NoneType:
				// ignore
				return nil
			case starlark.String:
				arr = append(arr, string(e))
				return nil
			case *starlark.List:
				if e.Len() == 0 {
					return nil
				}

				err := listForeach(e, func(i int, value starlark.Value) error {
					if _, ok := value.(starlark.NoneType); ok {
						return nil
					}

					dep, ok := value.(starlark.String)
					if !ok {
						return fmt.Errorf("dep must be string, got %v", value.Type())
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
			Array:  arr,
			StrMap: strMap,
		}

		return nil
	}

	return fmt.Errorf("must be dict, list or string, got %v", v.Type())
}
