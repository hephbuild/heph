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
	Run                 ArrayStr
	ConcurrentExecution bool
	FileContent         string
	Entrypoint          string
	Platforms           TargetArgsPlatforms
	RunInCwd            bool
	Quiet               bool
	PassArgs            bool
	Cache               TargetArgsCache
	RestoreCache        bool
	SupportFiles        ArrayStr
	SandboxEnabled      bool
	OutInSandbox        bool
	Gen                 bool
	Codegen             string
	Deps                ArrayMapStrArray
	HashDeps            ArrayMapStrArray
	Tools               ArrayMapStr
	Labels              ArrayStr
	Out                 ArrayMapStrArray
	Env                 ArrayMapStr
	PassEnv             ArrayStr
	RuntimePassEnv      ArrayStr
	RuntimeEnv          ArrayMapStr
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
		defer it.Done()

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
	Deps           ArrayMapStrArray
	Tools          ArrayMapStr
	Env            ArrayMapStr
	PassEnv        ArrayStr
	RuntimeEnv     ArrayMapStr
	RuntimePassEnv ArrayStr
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

	return fmt.Errorf("please use heph.target_spec()")
}

type TargetArgsCache struct {
	Enabled bool
	Named   BoolArray
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

	return fmt.Errorf("cache must be bool or use heph.cache(), got %v", v.Type())
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

	return fmt.Errorf("must be bool or []string, got %v", v.Type())
}

type ArrayStr []string

func (d *ArrayStr) Unpack(v starlark.Value) error {
	if _, ok := v.(starlark.NoneType); ok {
		return nil
	}

	if vs, ok := v.(starlark.String); ok {
		*d = ArrayStr{string(vs)}
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

type ArrayMapStr struct {
	Array  []string
	ArrMap map[string]string
}

func (d *ArrayMapStr) Unpack(v starlark.Value) error {
	if _, ok := v.(starlark.NoneType); ok {
		return nil
	}

	var sa ArrayMapStrArray
	err := sa.Unpack(v)
	if err != nil {
		return err
	}

	*d = ArrayMapStr{Array: sa.Array}
	if sa.ArrMap != nil {
		d.ArrMap = make(map[string]string, len(sa.ArrMap))
		for k, a := range sa.ArrMap {
			switch len(a) {
			case 0:
				continue
			case 1:
				d.ArrMap[k] = a[0]
			default:
				return fmt.Errorf("%v: value must be a String, got List", k)
			}
		}
	}

	return nil
}

type ArrayMapStrArray struct {
	Array  []string
	ArrMap map[string][]string
}

func (d *ArrayMapStrArray) Unpack(v starlark.Value) error {
	switch v := v.(type) {
	case starlark.NoneType:
		return nil
	case starlark.String:
		*d = ArrayMapStrArray{
			Array: []string{string(v)},
		}
		return nil
	case *starlark.List:
		arr := make([]string, 0, v.Len())

		err := listForeach(v, func(i int, value starlark.Value) error {
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

				arr = ads.GrowExtra(arr, e.Len())

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

			return fmt.Errorf("at %v: element must be string []string, got %v", i, value.Type())
		})
		if err != nil {
			return err
		}

		*d = ArrayMapStrArray{
			Array: arr,
		}

		return nil
	case *starlark.Dict:
		arr := make([]string, 0, v.Len())
		arrMap := make(map[string][]string, v.Len())

		for _, e := range v.Items() {
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
				arrMap[key] = append(arrMap[key], string(val))
			case *starlark.List:
				arr = ads.GrowExtra(arr, val.Len())
				arrMap[key] = ads.GrowExtra(arrMap[key], val.Len())

				err := listForeach(val, func(i int, value starlark.Value) error {
					if _, ok := value.(starlark.NoneType); ok {
						return nil
					}

					val, ok := value.(starlark.String)
					if !ok {
						return fmt.Errorf("value must be string, got %v", value.Type())
					}

					arr = append(arr, string(val))
					arrMap[key] = append(arrMap[key], string(val))

					return nil
				})
				if err != nil {
					return err
				}
			default:
				return fmt.Errorf("val must be string or []string, got %v", v.Type())
			}
		}

		*d = ArrayMapStrArray{
			Array:  arr,
			ArrMap: arrMap,
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
