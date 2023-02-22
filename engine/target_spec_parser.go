package engine

import (
	"fmt"
	"go.starlark.net/starlark"
	"heph/exprs"
	"heph/packages"
	"heph/targetspec"
	"heph/utils"
	"runtime"
	"sort"
	"strconv"
	"strings"
	"time"
)

func specFromArgs(args TargetArgs, pkg *packages.Package) (targetspec.TargetSpec, error) {
	t := targetspec.TargetSpec{
		FQN:                 pkg.TargetPath(args.Name),
		Name:                args.Name,
		Run:                 args.Run.Array,
		Doc:                 args.Doc,
		FileContent:         []byte(args.FileContent),
		ConcurrentExecution: args.ConcurrentExecution,
		Entrypoint:          args.Entrypoint,
		Platforms: utils.Map(args.Platforms, func(d *starlark.Dict) targetspec.TargetPlatform {
			labels := map[string]string{}
			options := map[string]interface{}{}
			for _, k := range d.Keys() {
				ks := k.(starlark.String).GoString()
				if ks == "options" {
					v, _, _ := d.Get(k)

					for _, t := range v.(*starlark.Dict).Items() {
						options[t[0].(starlark.String).GoString()] = utils.FromStarlark(t[1])
					}

					continue
				}
				v, _, _ := d.Get(k)
				labels[ks] = v.(starlark.String).GoString()
			}

			return targetspec.TargetPlatform{
				Labels:  labels,
				Options: options,
			}
		}),
		Package:  pkg,
		PassArgs: args.PassArgs,
		Quiet:    args.Quiet,
		Cache: targetspec.TargetSpecCache{
			Enabled: args.Cache.Enabled,
			Named:   args.Cache.Named,
			History: args.Cache.History,
		},
		RestoreCache: args.RestoreCache,
		Sandbox:      args.SandboxEnabled,
		OutInSandbox: args.OutInSandbox,
		Codegen:      args.Codegen,
		Labels:       args.Labels.Array,
		Env:          args.Env.StrMap,
		PassEnv:      args.PassEnv.Array,
		RunInCwd:     args.RunInCwd,
		Gen:          args.Gen,
		RuntimeEnv:   args.RuntimeEnv.StrMap,
		SrcEnv: targetspec.TargetSpecSrcEnv{
			All:   args.SrcEnv.All,
			Named: args.SrcEnv.Named,
		},
		OutEnv:   args.OutEnv,
		HashFile: args.HashFile,
	}

	var err error

	t.Tools, err = toolsSpecFromArgs(t, args.Tools)
	if err != nil {
		return targetspec.TargetSpec{}, err
	}

	t.Deps, err = depsSpecFromArgs(t, args.Deps)
	if err != nil {
		return targetspec.TargetSpec{}, err
	}
	if args.HashDeps.Array != nil {
		t.DifferentHashDeps = true
		t.HashDeps, err = depsSpecFromArgs(t, args.HashDeps)
		if err != nil {
			return targetspec.TargetSpec{}, err
		}
	} else {
		t.HashDeps = t.Deps
	}

	t.Transitive.Tools, err = toolsSpecFromArgs(t, args.Transitive.Tools)
	if err != nil {
		return targetspec.TargetSpec{}, err
	}

	t.Transitive.Deps, err = depsSpecFromArgs(t, args.Transitive.Deps)
	if err != nil {
		return targetspec.TargetSpec{}, err
	}

	t.Transitive.Env = args.Transitive.Env.StrMap
	t.Transitive.RuntimeEnv = args.Transitive.RuntimeEnv.StrMap
	t.Transitive.PassEnv = args.Transitive.PassEnv.Array

	if len(args.Out.ArrMap) > 0 {
		for k, vs := range args.Out.ArrMap {
			if k == "" {
				return t, fmt.Errorf("named output must not be empty")
			}

			for _, v := range vs {
				t.Out = append(t.Out, targetspec.TargetSpecOutFile{
					Name: k,
					Path: v,
				})
			}
		}
	} else if len(args.Out.StrMap) > 0 {
		for k, v := range args.Out.StrMap {
			if k == "" {
				return t, fmt.Errorf("named output must not be empty")
			}

			t.Out = append(t.Out, targetspec.TargetSpecOutFile{
				Name: k,
				Path: v,
			})
		}
	} else {
		for _, file := range args.Out.Array {
			t.Out = append(t.Out, targetspec.TargetSpecOutFile{
				Path: file,
			})
		}
	}

	if len(args.SupportFiles) > 0 {
		t.HasSupportFiles = true

		for _, file := range args.SupportFiles {
			t.Out = append(t.Out, targetspec.TargetSpecOutFile{
				Name: targetspec.SupportFilesOutput,
				Path: file,
			})
		}
	}

	sort.Slice(t.Out, utils.MultiLess(
		func(i, j int) int {
			return strings.Compare(t.Out[i].Path, t.Out[j].Path)
		},
		func(i, j int) int {
			return strings.Compare(t.Out[i].Name, t.Out[j].Name)
		},
	))

	if t.SrcEnv.All == "" {
		if t.OutInSandbox {
			t.SrcEnv.All = targetspec.FileEnvAbs
		} else {
			t.SrcEnv.All = targetspec.FileEnvRelPkg
		}
	}

	for k, v := range t.SrcEnv.Named {
		if !validate(v, targetspec.FileEnvValues) {
			return targetspec.TargetSpec{}, fmt.Errorf("src_env[%v] must be one of %v, got %v", k, printOneOf(targetspec.FileEnvValues), v)
		}
	}

	if !validate(t.SrcEnv.All, targetspec.FileEnvValues) {
		return targetspec.TargetSpec{}, fmt.Errorf("src_env must be one of %v, got %v", printOneOf(targetspec.FileEnvValues), t.SrcEnv.All)
	}

	if t.OutEnv == "" {
		if t.OutInSandbox {
			t.OutEnv = targetspec.FileEnvAbs
		} else {
			t.OutEnv = targetspec.FileEnvRelPkg
		}
	}
	if !validate(t.OutEnv, targetspec.FileEnvValues) {
		return targetspec.TargetSpec{}, fmt.Errorf("out_env must be one of %v, got %v", printOneOf(targetspec.FileEnvValues), t.OutEnv)
	}

	if t.HashFile == "" {
		t.HashFile = targetspec.HashFileContent
	}
	if !validate(t.HashFile, targetspec.HashFileValues) {
		return targetspec.TargetSpec{}, fmt.Errorf("hash_file must be one of %v, got %v", printOneOf(targetspec.HashFileValues), t.HashFile)
	}

	if t.Entrypoint == "" {
		t.Entrypoint = targetspec.EntrypointBash
	}
	if !validate(t.Entrypoint, targetspec.EntrypointValues) {
		return targetspec.TargetSpec{}, fmt.Errorf("entrypoint must be one of %v, got %v", printOneOf(targetspec.EntrypointValues), t.Entrypoint)
	}

	if len(t.Platforms) == 0 {
		t.Platforms = []targetspec.TargetPlatform{{
			Labels: map[string]string{
				"name": "local",
				"os":   runtime.GOOS,
				"arch": runtime.GOARCH,
			},
		}}
	} else if len(t.Platforms) != 1 {
		return targetspec.TargetSpec{}, fmt.Errorf("only a single platform is supported, for now")
	}

	if t.Codegen != "" {
		if !validate(t.Codegen, targetspec.CodegenValues) {
			return targetspec.TargetSpec{}, fmt.Errorf("codegen must be one of %v, got %v", printOneOf(targetspec.CodegenValues), t.Codegen)
		}

		if !t.Sandbox {
			return targetspec.TargetSpec{}, fmt.Errorf("codegen is only suported in sandboxed targets")
		}

		for _, file := range t.Out {
			if utils.IsGlob(file.Path) {
				return targetspec.TargetSpec{}, fmt.Errorf("codegen targets must not have glob outputs")
			}
		}
	}

	if len(args.Timeout) > 0 {
		t.Timeout, err = time.ParseDuration(args.Timeout)
		if err != nil {
			if strings.Contains(err.Error(), "missing unit in duration") {
				v, err := strconv.ParseInt(args.Timeout, 10, 64)
				if err != nil {
					return targetspec.TargetSpec{}, err
				}

				t.Timeout = time.Duration(v) * time.Second
			} else {
				return targetspec.TargetSpec{}, fmt.Errorf("timeout: %w", err)
			}
		}
	}

	if args.Cache.Enabled && args.ConcurrentExecution {
		return targetspec.TargetSpec{}, fmt.Errorf("concurrent_execution and cache are incompatible")
	}

	return t, nil
}

func depsSpecFromArr(t targetspec.TargetSpec, arr []string, name string) (targetspec.TargetSpecDeps, error) {
	td := targetspec.TargetSpecDeps{}

	for _, dep := range arr {
		if expr, err := exprs.Parse(dep); err == nil {
			td.Exprs = append(td.Exprs, targetspec.TargetSpecDepExpr{
				Name: name,
				Expr: expr,
			})
			continue
		}

		if dtp, options, err := targetspec.TargetOutputOptionsParse(t.Package.FullName, dep); err == nil {
			tspec := targetspec.TargetSpecDepTarget{
				Name:   name,
				Target: dtp.TargetPath.Full(),
				Output: dtp.Output,
				Mode:   targetspec.TargetSpecDepModeCopy,
			}

			for k, v := range options {
				switch k {
				case "mode":
					mode := targetspec.TargetSpecDepMode(v)
					if !utils.Contains(targetspec.TargetSpecDepModes, mode) {
						return targetspec.TargetSpecDeps{}, fmt.Errorf("invalid mode: %v", v)
					}
					tspec.Mode = mode
				default:
					return targetspec.TargetSpecDeps{}, fmt.Errorf("invalid option %v=%v", k, v)
				}
			}
			td.Targets = append(td.Targets, tspec)
			continue
		}

		// Is probably file
		td.Files = append(td.Files, targetspec.TargetSpecDepFile{
			Name: name,
			Path: dep,
		})
	}

	return td, nil
}

func toolsSpecFromString(t targetspec.TargetSpec, ts *targetspec.TargetSpecTools, name, tool string) error {
	expr, err := exprs.Parse(tool)
	if err == nil {
		ts.Exprs = append(ts.Exprs, targetspec.TargetSpecExprTool{
			Name: name,
			Expr: expr,
		})
		return nil
	}

	tp, err := targetspec.TargetOutputParse(t.Package.FullName, tool)
	if err == nil {
		ts.Targets = append(ts.Targets, targetspec.TargetSpecTargetTool{
			Name:   name,
			Target: tp.TargetPath.Full(),
			Output: tp.Output,
		})
		return nil
	}

	if name == "" {
		name = tool
	}

	ts.Hosts = append(ts.Hosts, targetspec.TargetSpecHostTool{
		Name:    name,
		BinName: tool,
	})

	return nil
}

func toolsSpecFromArgs(t targetspec.TargetSpec, tools ArrayMap) (targetspec.TargetSpecTools, error) {
	ts := targetspec.TargetSpecTools{}

	if len(tools.StrMap) > 0 {
		for name, s := range tools.StrMap {
			err := toolsSpecFromString(t, &ts, name, s)
			if err != nil {
				return targetspec.TargetSpecTools{}, err
			}
		}
	} else {
		for _, s := range tools.Array {
			err := toolsSpecFromString(t, &ts, "", s)
			if err != nil {
				return targetspec.TargetSpecTools{}, err
			}
		}
	}

	return ts, nil
}

func depsSpecFromArgs(t targetspec.TargetSpec, deps ArrayMap) (targetspec.TargetSpecDeps, error) {
	td := targetspec.TargetSpecDeps{}

	if len(deps.ArrMap) > 0 {
		for name, arr := range deps.ArrMap {
			d, err := depsSpecFromArr(t, arr, name)
			if err != nil {
				return d, err
			}
			td.Targets = append(td.Targets, d.Targets...)
			td.Exprs = append(td.Exprs, d.Exprs...)
			td.Files = append(td.Files, d.Files...)
		}
	} else {
		d, err := depsSpecFromArr(t, deps.Array, "")
		if err != nil {
			return d, err
		}
		td.Targets = append(td.Targets, d.Targets...)
		td.Exprs = append(td.Exprs, d.Exprs...)
		td.Files = append(td.Files, d.Files...)
	}

	sort.Slice(td.Exprs, utils.MultiLess(
		func(i, j int) int {
			return strings.Compare(td.Exprs[i].Expr.String, td.Exprs[j].Expr.String)
		},
		func(i, j int) int {
			return strings.Compare(td.Exprs[i].Name, td.Exprs[j].Name)
		},
	))

	sort.Slice(td.Targets, utils.MultiLess(
		func(i, j int) int {
			return strings.Compare(td.Targets[i].Target, td.Targets[j].Target)
		},
		func(i, j int) int {
			return strings.Compare(td.Targets[i].Name, td.Targets[j].Name)
		},
	))

	sort.Slice(td.Files, utils.MultiLess(
		func(i, j int) int {
			return strings.Compare(td.Files[i].Path, td.Files[j].Path)
		},
		func(i, j int) int {
			return strings.Compare(td.Files[i].Name, td.Files[j].Name)
		},
	))

	return td, nil
}

func printOneOf(valid []string) string {
	for i, s := range valid {
		valid[i] = fmt.Sprintf("`%v`", s)
	}

	return strings.Join(valid, ", ")
}

func validate(s string, valid []string) bool {
	for _, vs := range valid {
		if vs == s {
			return true
		}
	}

	return false
}
