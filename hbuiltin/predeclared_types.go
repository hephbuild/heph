package hbuiltin

import (
	"fmt"
	"github.com/hephbuild/heph/utils/xstarlark"
	"go.starlark.net/starlark"
)

type TargetArgs struct {
	Name                string
	Pkg                 string
	Doc                 string
	Run                 xstarlark.Listable[string]
	ConcurrentExecution bool
	FileContent         string
	Entrypoint          string
	Platforms           xstarlark.Listable[xstarlark.Distruct]
	RunInCwd            bool
	PassArgs            bool
	Cache               TargetArgsCache
	RestoreCache        TargetArgsRestoreCache
	SupportFiles        xstarlark.Listable[string]
	SandboxEnabled      bool
	OutInSandbox        bool
	Gen                 xstarlark.Listable[string]
	Codegen             string
	Deps                ArrayMapStrArray
	HashDeps            ArrayMapStrArray
	RuntimeDeps         ArrayMapStrArray
	Tools               ArrayMapStr
	Labels              xstarlark.Listable[string]
	Out                 ArrayMapStrArray
	Env                 ArrayMapStr
	PassEnv             xstarlark.Listable[string]
	RuntimePassEnv      xstarlark.Listable[string]
	RuntimeEnv          ArrayMapStr
	SrcEnv              SrcEnv
	OutEnv              string
	HashFile            string
	Transitive          TargetArgsTransitive
	Timeout             string
	GenDepsMeta         bool
	Annotations         xstarlark.Distruct
}

type TargetArgsTransitive struct {
	Deps           ArrayMapStrArray
	HashDeps       ArrayMapStrArray
	RuntimeDeps    ArrayMapStrArray
	Tools          ArrayMapStr
	Env            ArrayMapStr
	PassEnv        xstarlark.Listable[string]
	RuntimeEnv     ArrayMapStr
	RuntimePassEnv xstarlark.Listable[string]
	Platforms      xstarlark.Listable[xstarlark.Distruct]
}

func (c *TargetArgsTransitive) Unpack(v starlark.Value) error {
	if _, ok := v.(starlark.NoneType); ok {
		return nil
	}

	d, err := xstarlark.UnpackDistruct(v)
	if err != nil {
		return err
	}

	var cs TargetArgsTransitive
	err = starlark.UnpackArgs("", nil, d.Items().Tuples(),
		"deps?", &cs.Deps,
		"runtime_deps?", &cs.RuntimeDeps,
		"hash_deps?", &cs.HashDeps,
		"tools?", &cs.Tools,
		"env?", &cs.Env,
		"runtime_env?", &cs.RuntimeEnv,
		"pass_env?", &cs.PassEnv,
		"runtime_pass_env?", &cs.RuntimePassEnv,
		"platforms?", &cs.Platforms,
	)
	if err != nil {
		return err
	}

	*c = cs
	return nil
}

type TargetArgsCache struct {
	Enabled bool
	Named   BoolArray
	History int
}

func (c *TargetArgsCache) Unpack(v starlark.Value) error {
	if _, ok := v.(starlark.NoneType); ok {
		return nil
	}

	b, ok := v.(starlark.Bool)
	if ok {
		*c = TargetArgsCache{
			Enabled: bool(b),
		}
		return nil
	}

	cs := TargetArgsCache{
		Enabled: true,
	}

	err := xstarlark.UnpackDistructTo(v,
		"named?", &cs.Named,
		"history?", &cs.History,
	)
	if err != nil {
		return err
	}

	*c = cs

	return nil
}

type TargetArgsRestoreCache struct {
	Enabled bool
	Key     string
	Paths   xstarlark.Listable[string]
	Env     string
}

func (c *TargetArgsRestoreCache) Unpack(v starlark.Value) error {
	if _, ok := v.(starlark.NoneType); ok {
		return nil
	}

	if b, ok := v.(starlark.Bool); ok {
		*c = TargetArgsRestoreCache{
			Enabled: bool(b),
		}
		return nil
	}

	var l xstarlark.Listable[string]
	err := xstarlark.UnpackOne(v, &l)
	if err == nil {
		*c = TargetArgsRestoreCache{
			Enabled: true,
			Paths:   l,
		}
		return nil
	}

	cs := TargetArgsRestoreCache{
		Enabled: true,
	}

	err = xstarlark.UnpackDistructTo(v,
		"paths?", &cs.Paths,
		"key?", &cs.Key,
		"env?", &cs.Env,
	)
	if err != nil {
		return err
	}

	*c = cs

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
			Bool:  bool(e),
			Array: []string{},
		}
		return nil
	case *starlark.List:
		var arr xstarlark.Listable[string]
		err := arr.Unpack(v)
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
		var arr xstarlark.Listable[string]
		err := arr.Unpack(v)
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

			var vals xstarlark.Listable[string]
			err := vals.Unpack(e.Index(1))
			if err != nil {
				return fmt.Errorf("\"%v\": %w", key, err)
			}

			arr = append(arr, vals...)
			arrMap[key] = append(arrMap[key], vals...)
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
