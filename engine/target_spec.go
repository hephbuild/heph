package engine

import (
	"fmt"
	log "github.com/sirupsen/logrus"
	"heph/utils"
	"os/exec"
)

type starlarkTargetArgs struct {
	name           string
	run            Runnable
	runInCwd       bool
	quiet          bool
	passArgs       bool
	cache          BoolArray
	sandboxEnabled bool
	gen            bool
	codegen        string
	deps           ArrayMap
	hashDeps       ArrayMap
	tools          ArrayMap
	labels         ArrayMap
	out            ArrayMap
	env            ArrayMap
	passEnv        ArrayMap
	provide        ArrayMap
	requireGen     bool
}

func specFromArgs(args starlarkTargetArgs, pkg *Package) (TargetSpec, error) {
	t := TargetSpec{
		FQN:         pkg.TargetPath(args.name),
		Name:        args.name,
		Cmds:        args.run.Cmds,
		Package:     pkg,
		PassArgs:    args.passArgs,
		Quiet:       args.quiet,
		ShouldCache: args.cache.Bool,
		CachedFiles: args.cache.Array,
		Sandbox:     args.sandboxEnabled,
		Codegen:     args.codegen,
		Labels:      args.labels.Array,
		Env:         args.env.Map,
		PassEnv:     args.passEnv.Array,
		RunInCwd:    args.runInCwd,
		Gen:         args.gen,
		Provide:     args.provide.Map,
		RequireGen:  args.requireGen,
	}

	var err error

	for _, tool := range args.tools.Array {
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

	t.Deps, err = depsSpecFromArgs(t, args.deps)
	if err != nil {
		return TargetSpec{}, err
	}
	if args.hashDeps.Array != nil {
		t.DifferentHashDeps = true
		t.HashDeps, err = depsSpecFromArgs(t, args.hashDeps)
		if err != nil {
			return TargetSpec{}, err
		}
	} else {
		t.HashDeps = t.Deps
	}

	if len(args.out.Map) > 0 {
		for k, v := range args.out.Map {
			t.Out = append(t.Out, TargetSpecOutFile{
				Name:    k,
				Package: pkg,
				Path:    v,
			})
		}
	} else {
		for _, file := range args.out.Array {
			t.Out = append(t.Out, TargetSpecOutFile{
				Package: pkg,
				Path:    file,
			})
		}
	}

	return t, nil
}

func depsSpecFromArgs(t TargetSpec, deps ArrayMap) (TargetSpecDeps, error) {
	td := TargetSpecDeps{}

	for _, dep := range deps.Array {
		if expr, err := utils.ExprParse(dep); err == nil {
			td.Exprs = append(td.Exprs, TargetSpecDepExpr{
				Expr: expr,
			})
			continue
		}

		// TODO: support named output
		if dtp, err := utils.TargetParse(t.Package.FullName, dep); err == nil {
			td.Targets = append(td.Targets, TargetSpecDepTarget{
				Target: dtp.Full(),
			})
			continue
		}

		// Is probably file
		td.Files = append(td.Files, TargetSpecDepFile{
			Package: t.Package,
			Path:    dep,
		})
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
	Target string
}

type TargetSpecDepExpr struct {
	Expr *utils.Expr
}

type TargetSpecDepFile struct {
	Package *Package
	Path    string
}

type TargetSpecOutFile struct {
	Name    string
	Package *Package
	Path    string
}
