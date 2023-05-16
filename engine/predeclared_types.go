package engine

import (
	"fmt"
	"github.com/hephbuild/heph/utils/ads"
	"go.starlark.net/starlark"
	"go.starlark.net/starlarkstruct"
)

type TargetArgs struct {
	Name                string
	Pkg                 string
	Doc                 string
	Run                 ArrayMap
	ConcurrentExecution bool
	FileContent         string
	Entrypoint          string
	Platforms           TargetArgsPlatforms
	RunInCwd            bool
	Quiet               bool
	PassArgs            bool
	Cache               TargetArgsCache
	RestoreCache        bool
	SupportFiles        Array
	SandboxEnabled      bool
	OutInSandbox        bool
	Gen                 bool
	Codegen             string
	Deps                ArrayMap
	HashDeps            ArrayMap
	Tools               ArrayMap
	Labels              ArrayMap
	Out                 ArrayMap
	Env                 ArrayMap
	PassEnv             ArrayMap
	RuntimePassEnv      ArrayMap
	RuntimeEnv          ArrayMap
	SrcEnv              SrcEnv
	OutEnv              string
	HashFile            string
	Transitive          TargetArgsTransitive
	Timeout             string
	GenDepsMeta         bool
}

type TargetArgsPlatforms []*starlark.Dict

func (c *TargetArgsPlatforms) Unpack(v starlark.Value) error {
	if _, ok := v.(starlark.NoneType); ok {
		return nil
	}

	if v, ok := v.(*starlark.Dict); ok {
		*c = []*starlark.Dict{v}
		return nil
	}

	if v, ok := v.(*starlark.List); ok {
		platforms := make([]*starlark.Dict, 0, v.Len())
		it := v.Iterate()
		var e starlark.Value
		for it.Next(&e) {
			e := e
			platforms = append(platforms, e.(*starlark.Dict))
		}

		*c = platforms
		return nil
	}

	return fmt.Errorf("platforms must be list")
}

type TargetArgsTransitive struct {
	Deps           ArrayMap
	Tools          ArrayMap
	Env            ArrayMap
	PassEnv        ArrayMap
	RuntimeEnv     ArrayMap
	RuntimePassEnv ArrayMap
}

func (c *TargetArgsTransitive) Unpack(v starlark.Value) error {
	if _, ok := v.(starlark.NoneType); ok {
		return nil
	}

	if d, ok := v.(*starlarkstruct.Struct); ok {
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
			case "runtime_env":
				err := cs.RuntimeEnv.Unpack(v)
				if err != nil {
					return err
				}
			case "pass_env":
				err := cs.PassEnv.Unpack(v)
				if err != nil {
					return err
				}
			case "runtime_pass_env":
				err := cs.RuntimePassEnv.Unpack(v)
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
	History int
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
			case "history":
				vsi, err := starlark.NumberToInt(v)
				if err != nil {
					return err
				}

				vi, _ := vsi.Int64()

				cs.History = int(vi)
			default:
				return fmt.Errorf("invalid arg %v, call heph.cache()", n)
			}
		}

		*c = cs
		return nil
	}

	b, ok := v.(starlark.Bool)
	if ok {
		*c = TargetArgsCache{
			Enabled: bool(b),
		}
		return nil
	}

	return fmt.Errorf("cache must be bool or call heph.cache(), got %v", v.Type())
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
		arr := make([]string, 0, e.Len())
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

	if vb, ok := v.(starlark.Bool); ok {
		if vb {
			return fmt.Errorf("True is not a valid value")
		}

		*d = Array{}
		return nil
	}

	if vs, ok := v.(starlark.String); ok {
		*d = Array{string(vs)}
		return nil
	}

	if vl, ok := v.(*starlark.List); ok {
		arr := make([]string, 0, vl.Len())

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

	if vs, ok := v.(starlark.String); ok {
		*d = ArrayMap{
			Array: []string{string(vs)},
		}
		return nil
	}

	if vd, ok := v.(*starlark.Dict); ok {
		arr := make([]string, 0, vd.Len())
		arrMap := make(map[string][]string, vd.Len())
		strMap := make(map[string]string, vd.Len())

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
				arrMap[key] = ads.Grow(arrMap[key], val.Len())

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

	if vl, ok := v.(*starlark.List); ok {
		arr := make([]string, 0, vl.Len())

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

				arr = ads.Grow(arr, e.Len())

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
			Array: arr,
		}

		return nil
	}

	return fmt.Errorf("must be dict, list or string, got %v", v.Type())
}

type SrcEnv struct {
	Default string
	Named   map[string]string
}

func (d *SrcEnv) Unpack(v starlark.Value) error {
	if _, ok := v.(starlark.NoneType); ok {
		return nil
	}

	if vs, ok := v.(starlark.String); ok {
		*d = SrcEnv{
			Default: string(vs),
		}
		return nil
	}

	if vd, ok := v.(*starlark.Dict); ok {
		def := ""
		named := make(map[string]string, vd.Len())

		for _, e := range vd.Items() {
			keyv := e.Index(0)
			skey, ok := keyv.(starlark.String)
			if !ok {
				return fmt.Errorf("key must be string, got %v", keyv.Type())
			}

			key := string(skey)

			valuev := e.Index(1)
			svalue, ok := valuev.(starlark.String)
			if !ok {
				return fmt.Errorf("value must be string, got %v", valuev.Type())
			}

			value := string(svalue)

			if key == "_default" {
				def = value
			} else {
				named[key] = value
			}
		}

		*d = SrcEnv{
			Default: def,
			Named:   named,
		}
		return nil
	}

	return fmt.Errorf("must be string or dict, got %v", v.Type())
}
