package engine

import (
	"fmt"
	log "github.com/sirupsen/logrus"
	"heph/exprs"
	"heph/packages"
	"heph/targetspec"
	"heph/utils"
	"os"
	"os/exec"
	"sort"
	"strings"
)

func specFromArgs(args TargetArgs, pkg *packages.Package) (targetspec.TargetSpec, error) {
	t := targetspec.TargetSpec{
		FQN:         pkg.TargetPath(args.Name),
		Name:        args.Name,
		Run:         args.Run.Array,
		FileContent: []byte(args.FileContent),
		Executor:    args.Executor,
		Package:     pkg,
		PassArgs:    args.PassArgs,
		Quiet:       args.Quiet,
		Cache: targetspec.TargetSpecCache{
			Enabled: args.Cache.Enabled,
			Named:   args.Cache.Named,
			Files:   args.Cache.Files,
		},
		Sandbox:      args.SandboxEnabled,
		OutInSandbox: args.OutInSandbox,
		Codegen:      args.Codegen,
		Labels:       args.Labels.Array,
		Env:          args.Env.StrMap,
		PassEnv:      args.PassEnv.Array,
		RunInCwd:     args.RunInCwd,
		Gen:          args.Gen,
		RuntimeEnv:   args.RuntimeEnv.StrMap,
		SrcEnv:       args.SrcEnv,
		OutEnv:       args.OutEnv,
		HashFile:     args.HashFile,
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
	t.Transitive.PassEnv = args.Transitive.PassEnv.Array

	if len(args.Out.ArrMap) > 0 {
		for k, vs := range args.Out.ArrMap {
			if k == "" {
				return t, fmt.Errorf("named output must not be empty")
			}

			for _, v := range vs {
				t.Out = append(t.Out, targetspec.TargetSpecOutFile{
					Name:    k,
					Package: pkg,
					Path:    v,
				})
			}
		}
	} else if len(args.Out.StrMap) > 0 {
		for k, v := range args.Out.StrMap {
			if k == "" {
				return t, fmt.Errorf("named output must not be empty")
			}

			t.Out = append(t.Out, targetspec.TargetSpecOutFile{
				Name:    k,
				Package: pkg,
				Path:    v,
			})
		}
	} else {
		for _, file := range args.Out.Array {
			t.Out = append(t.Out, targetspec.TargetSpecOutFile{
				Package: pkg,
				Path:    file,
			})
		}
	}

	sort.SliceStable(t.Out, utils.MultiLess(
		func(i, j int) int {
			return strings.Compare(t.Out[i].Path, t.Out[j].Path)
		},
		func(i, j int) int {
			return strings.Compare(t.Out[i].Name, t.Out[j].Name)
		},
	))

	if t.SrcEnv == "" {
		if t.OutInSandbox {
			t.SrcEnv = targetspec.FileEnvAbs
		} else {
			t.SrcEnv = targetspec.FileEnvRelPkg
		}
	}
	if !validate(t.SrcEnv, targetspec.FileEnvValues) {
		return targetspec.TargetSpec{}, fmt.Errorf("src_env must be one of %v, got %v", printOneOf(targetspec.FileEnvValues), t.SrcEnv)
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

	if t.Executor == "" {
		t.Executor = targetspec.ExecutorBash
	}
	if !validate(t.Executor, targetspec.ExecutorValues) {
		return targetspec.TargetSpec{}, fmt.Errorf("executor must be one of %v, got %v", printOneOf(targetspec.ExecutorValues), t.Executor)
	}

	if t.Codegen != "" {
		if !validate(t.Codegen, targetspec.CodegenValues) {
			return targetspec.TargetSpec{}, fmt.Errorf("codegen must be one of %v, got %v", printOneOf(targetspec.CodegenValues), t.Codegen)
		}

		if !t.Sandbox {
			return targetspec.TargetSpec{}, fmt.Errorf("codegen is only suported in sandboxed targets")
		}

		for _, file := range t.Out {
			if strings.Contains(file.Path, "*") {
				return targetspec.TargetSpec{}, fmt.Errorf("codegen targets must not have glob outputs")
			}
		}
	}

	return t, nil
}

func depsSpecFromArr(t targetspec.TargetSpec, arr []string, name string) targetspec.TargetSpecDeps {
	td := targetspec.TargetSpecDeps{}

	for _, dep := range arr {
		if expr, err := exprs.Parse(dep); err == nil {
			td.Exprs = append(td.Exprs, targetspec.TargetSpecDepExpr{
				Name:    name,
				Package: t.Package,
				Expr:    expr,
			})
			continue
		}

		if dtp, err := targetspec.TargetOutputParse(t.Package.FullName, dep); err == nil {
			td.Targets = append(td.Targets, targetspec.TargetSpecDepTarget{
				Name:   name,
				Target: dtp.TargetPath.Full(),
				Output: dtp.Output,
			})
			continue
		}

		// Is probably file
		td.Files = append(td.Files, targetspec.TargetSpecDepFile{
			Name: name,
			Path: dep,
		})
	}

	return td
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

	lookPath := exec.LookPath
	if tool == "heph" {
		lookPath = func(file string) (string, error) {
			return os.Executable()
		}
	}

	binPath, err := lookPath(tool)
	if err != nil {
		return fmt.Errorf("%v is not a target, and cannot be found in PATH", tool)
	}

	log.Tracef("%v Using tool %v from %v", t.FQN, tool, binPath)

	if name == "" {
		name = tool
	}

	ts.Hosts = append(ts.Hosts, targetspec.TargetSpecHostTool{
		Name: name,
		Path: binPath,
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
			d := depsSpecFromArr(t, arr, name)
			td.Targets = append(td.Targets, d.Targets...)
			td.Exprs = append(td.Exprs, d.Exprs...)
			td.Files = append(td.Files, d.Files...)
		}
	} else {
		d := depsSpecFromArr(t, deps.Array, "")
		td.Targets = append(td.Targets, d.Targets...)
		td.Exprs = append(td.Exprs, d.Exprs...)
		td.Files = append(td.Files, d.Files...)
	}

	sort.SliceStable(td.Exprs, utils.MultiLess(
		func(i, j int) int {
			return strings.Compare(td.Exprs[i].Expr.String, td.Exprs[j].Expr.String)
		},
		func(i, j int) int {
			return strings.Compare(td.Exprs[i].Name, td.Exprs[j].Name)
		},
	))

	sort.SliceStable(td.Targets, utils.MultiLess(
		func(i, j int) int {
			return strings.Compare(td.Targets[i].Target, td.Targets[j].Target)
		},
		func(i, j int) int {
			return strings.Compare(td.Targets[i].Name, td.Targets[j].Name)
		},
	))

	sort.SliceStable(td.Files, utils.MultiLess(
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
