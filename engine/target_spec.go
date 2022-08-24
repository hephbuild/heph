package engine

import (
	"fmt"
	log "github.com/sirupsen/logrus"
	"heph/utils"
	"os/exec"
)

func specFromArgs(args TargetArgs, pkg *Package) (TargetSpec, error) {
	t := TargetSpec{
		FQN:         pkg.TargetPath(args.Name),
		Name:        args.Name,
		Cmds:        args.Run.Cmds,
		Package:     pkg,
		PassArgs:    args.PassArgs,
		Quiet:       args.Quiet,
		ShouldCache: args.Cache.Bool,
		CachedFiles: args.Cache.Array,
		Sandbox:     args.SandboxEnabled,
		Codegen:     args.Codegen,
		Labels:      args.Labels.Array,
		Env:         args.Env.StrMap,
		PassEnv:     args.PassEnv.Array,
		RunInCwd:    args.RunInCwd,
		Gen:         args.Gen,
		Provide:     args.Provide.StrMap,
		RequireGen:  args.RequireGen,
		SrcEnv:      args.SrcEnv,
		OutEnv:      args.OutEnv,
	}

	var err error

	for _, tool := range args.Tools.Array {
		tp, err := utils.TargetOutputParse(t.Package.FullName, tool)
		if err != nil {
			binPath, err := exec.LookPath(tool)
			if err != nil {
				return TargetSpec{}, fmt.Errorf("%v is not a target, and cannot be found in PATH", tool)
			}

			log.Tracef("%v Using tool %v from %v", t.FQN, tool, binPath)

			t.HostTools = append(t.HostTools, TargetSpecHostTool{
				Name: tool,
				Path: binPath,
			})
			continue
		}

		t.TargetTools = append(t.TargetTools, TargetSpecTargetTool{
			Target: tp.Full(),
			Output: tp.Output,
		})
	}

	t.Deps, err = depsSpecFromArgs(t, args.Deps)
	if err != nil {
		return TargetSpec{}, err
	}
	if args.HashDeps.Array != nil {
		t.DifferentHashDeps = true
		t.HashDeps, err = depsSpecFromArgs(t, args.HashDeps)
		if err != nil {
			return TargetSpec{}, err
		}
	} else {
		t.HashDeps = t.Deps
	}

	if len(args.Out.ArrMap) > 0 {
		for k, vs := range args.Out.ArrMap {
			if k == "" {
				return t, fmt.Errorf("named output must not be empty")
			}

			for _, v := range vs {
				t.Out = append(t.Out, TargetSpecOutFile{
					Name:    k,
					Package: pkg,
					Path:    v,
				})
			}
		}
	} else if len(args.Out.StrMap) > 0 {
		for k, v := range args.Out.StrMap {
			t.Out = append(t.Out, TargetSpecOutFile{
				Name:    k,
				Package: pkg,
				Path:    v,
			})
		}
	} else {
		for _, file := range args.Out.Array {
			t.Out = append(t.Out, TargetSpecOutFile{
				Package: pkg,
				Path:    file,
			})
		}
	}

	validateEnv := func(v string) bool {
		return v == "ignore" || v == "rel_root" || v == "rel_pkg" || v == "abs"
	}

	if t.SrcEnv == "" {
		t.SrcEnv = "rel_pkg"
	}
	if !validateEnv(t.SrcEnv) {
		return TargetSpec{}, fmt.Errorf("src_env must be one of `ignore`, `rel_root`, got %v", t.SrcEnv)
	}

	if t.OutEnv == "" {
		t.OutEnv = "rel_pkg"
	}
	if !validateEnv(t.OutEnv) {
		return TargetSpec{}, fmt.Errorf("out_env must be one of `ignore`, `rel_root`, got %v", t.OutEnv)
	}

	return t, nil
}

func depsSpecFromArr(t TargetSpec, arr []string, name string) TargetSpecDeps {
	td := TargetSpecDeps{}

	for _, dep := range arr {
		if expr, err := utils.ExprParse(dep); err == nil {
			td.Exprs = append(td.Exprs, TargetSpecDepExpr{
				Name:    name,
				Package: t.Package,
				Expr:    expr,
			})
			continue
		}

		if dtp, err := utils.TargetOutputParse(t.Package.FullName, dep); err == nil {
			td.Targets = append(td.Targets, TargetSpecDepTarget{
				Name:   name,
				Target: dtp.Full(),
				Output: dtp.Output,
			})
			continue
		}

		// Is probably file
		td.Files = append(td.Files, TargetSpecDepFile{
			Name:    name,
			Package: t.Package,
			Path:    dep,
		})
	}

	return td
}

func depsSpecFromArgs(t TargetSpec, deps ArrayMap) (TargetSpecDeps, error) {
	td := TargetSpecDeps{}

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

	return td, nil
}

type TargetSpec struct {
	Name    string
	FQN     string
	Package *Package

	Cmds              []string
	Quiet             bool
	Dir               string
	PassArgs          bool
	Deps              TargetSpecDeps
	HashDeps          TargetSpecDeps
	DifferentHashDeps bool
	TargetTools       []TargetSpecTargetTool
	HostTools         []TargetSpecHostTool
	Out               []TargetSpecOutFile
	ShouldCache       bool
	CachedFiles       []string
	Sandbox           bool
	Codegen           string
	Labels            []string
	Env               map[string]string
	PassEnv           []string
	RunInCwd          bool
	Gen               bool
	Source            []string
	Provide           map[string]string
	RequireGen        bool
	SrcEnv            string
	OutEnv            string
}

func (t TargetSpec) IsNamedOutput() bool {
	for _, file := range t.Out {
		if len(file.Name) > 0 {
			return true
		}
	}

	return false
}

func (t TargetSpec) FindNamedOutput(name string) *TargetSpecOutFile {
	for _, file := range t.Out {
		if file.Name == name {
			return &file
		}
	}

	return nil
}

type TargetSpecTargetTool struct {
	Target string
	Output string
}

type TargetSpecHostTool struct {
	Name string
	Path string
}

type TargetSpecDeps struct {
	Targets []TargetSpecDepTarget
	Files   []TargetSpecDepFile
	Exprs   []TargetSpecDepExpr
}

type TargetSpecDepTarget struct {
	Name   string
	Output string
	Target string
}

type TargetSpecDepExpr struct {
	Name    string
	Package *Package
	Expr    *utils.Expr
}

type TargetSpecDepFile struct {
	Name    string
	Package *Package
	Path    string
}

type TargetSpecOutFile struct {
	Name    string
	Package *Package
	Path    string
}
