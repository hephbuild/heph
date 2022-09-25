package engine

import (
	"encoding/json"
	"fmt"
	log "github.com/sirupsen/logrus"
	"heph/exprs"
	"heph/utils"
	"os"
	"os/exec"
	"sort"
	"strings"
)

func specFromArgs(args TargetArgs, pkg *Package) (TargetSpec, error) {
	t := TargetSpec{
		FQN:         pkg.TargetPath(args.Name),
		Name:        args.Name,
		Run:         args.Run.Array,
		FileContent: []byte(args.FileContent),
		Executor:    args.Executor,
		Package:     pkg,
		PassArgs:    args.PassArgs,
		Quiet:       args.Quiet,
		ShouldCache: args.Cache.Bool,
		Cache:       args.Cache.Array,
		Sandbox:     args.SandboxEnabled,
		Codegen:     args.Codegen,
		Labels:      args.Labels.Array,
		Env:         args.Env.StrMap,
		PassEnv:     args.PassEnv.Array,
		RunInCwd:    args.RunInCwd,
		Gen:         args.Gen,
		RuntimeEnv:  args.RuntimeEnv.StrMap,
		RequireGen:  args.RequireGen,
		SrcEnv:      args.SrcEnv,
		OutEnv:      args.OutEnv,
		HashFile:    args.HashFile,
	}

	var err error

	for _, tool := range args.Tools.Array {
		expr, err := exprs.Parse(tool)
		if err == nil {
			t.ExprTools = append(t.ExprTools, TargetSpecExprTool{
				Expr: expr,
			})
			continue
		}

		tp, err := utils.TargetOutputParse(t.Package.FullName, tool)
		if err == nil {
			t.TargetTools = append(t.TargetTools, TargetSpecTargetTool{
				Target: tp.Full(),
				Output: tp.Output,
			})
			continue
		}

		lookPath := exec.LookPath
		if tool == "heph" {
			lookPath = func(file string) (string, error) {
				return os.Executable()
			}
		}

		binPath, err := lookPath(tool)
		if err != nil {
			return TargetSpec{}, fmt.Errorf("%v is not a target, and cannot be found in PATH", tool)
		}

		log.Tracef("%v Using tool %v from %v", t.FQN, tool, binPath)

		t.HostTools = append(t.HostTools, TargetSpecHostTool{
			Name: tool,
			Path: binPath,
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

	sort.SliceStable(t.Out, utils.MultiLess(
		func(i, j int) int {
			return strings.Compare(t.Out[i].Path, t.Out[j].Path)
		},
		func(i, j int) int {
			return strings.Compare(t.Out[i].Name, t.Out[j].Name)
		},
	))

	if t.SrcEnv == "" {
		t.SrcEnv = FileEnvRelPkg
	}
	if !validate(t.SrcEnv, FileEnvValues) {
		return TargetSpec{}, fmt.Errorf("src_env must be one of %v, got %v", printOneOf(FileEnvValues), t.SrcEnv)
	}

	if t.OutEnv == "" {
		t.OutEnv = FileEnvRelPkg
	}
	if !validate(t.OutEnv, FileEnvValues) {
		return TargetSpec{}, fmt.Errorf("out_env must be one of %v, got %v", printOneOf(FileEnvValues), t.OutEnv)
	}

	if t.HashFile == "" {
		t.HashFile = HashFileContent
	}
	if !validate(t.HashFile, HashFileValues) {
		return TargetSpec{}, fmt.Errorf("hash_file must be one of %v, got %v", printOneOf(HashFileValues), t.HashFile)
	}

	if t.Executor == "" {
		t.Executor = ExecutorBash
	}
	if !validate(t.Executor, ExecutorValues) {
		return TargetSpec{}, fmt.Errorf("executor must be one of %v, got %v", printOneOf(ExecutorValues), t.Executor)
	}

	return t, nil
}

func depsSpecFromArr(t TargetSpec, arr []string, name string) TargetSpecDeps {
	td := TargetSpecDeps{}

	for _, dep := range arr {
		if expr, err := exprs.Parse(dep); err == nil {
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

var (
	FileEnvIgnore  = "ignore"
	FileEnvRelRoot = "rel_root"
	FileEnvRelPkg  = "rel_pkg"
	FileEnvAbs     = "abs"

	FileEnvValues = []string{FileEnvIgnore, FileEnvRelRoot, FileEnvRelPkg, FileEnvAbs}
)

var (
	HashFileContent = "content"
	HashFileModTime = "mod_time"

	HashFileValues = []string{HashFileContent, HashFileModTime}
)

var (
	ExecutorBash = "bash"
	ExecutorExec = "exec"

	ExecutorValues = []string{ExecutorBash, ExecutorExec}
)

type TargetSpec struct {
	Name    string
	FQN     string
	Package *Package

	Run               []string
	FileContent       []byte // Used by special target `text_file`
	Executor          string
	Quiet             bool
	Dir               string
	PassArgs          bool
	Deps              TargetSpecDeps
	HashDeps          TargetSpecDeps
	DifferentHashDeps bool
	ExprTools         []TargetSpecExprTool
	TargetTools       []TargetSpecTargetTool
	HostTools         []TargetSpecHostTool
	Out               []TargetSpecOutFile
	ShouldCache       bool
	Cache             []string
	Sandbox           bool
	Codegen           string
	Labels            []string
	Env               map[string]string
	PassEnv           []string
	RunInCwd          bool
	Gen               bool
	Source            []string
	RuntimeEnv        map[string]string
	RequireGen        bool
	SrcEnv            string
	OutEnv            string
	HashFile          string
}

func (t TargetSpec) IsTextFile() bool {
	return len(t.Run) == 1 && t.Run[0] == "text_file"
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

func (t TargetSpec) json() []byte {
	t.Package = nil
	t.Source = nil

	b, err := json.Marshal(t)
	if err != nil {
		panic(err)
	}

	return b
}

func (t TargetSpec) Equal(spec TargetSpec) bool {
	return t.equalStruct(spec)
}

func (t TargetSpec) equalJson(spec TargetSpec) bool {
	tj := t.json()
	sj := spec.json()

	if len(tj) != len(sj) {
		return false
	}

	for i := 0; i < len(tj); i++ {
		if tj[i] != sj[i] {
			return false
		}
	}

	return true
}

type TargetSpecTargetTool struct {
	Target string
	Output string
}

type TargetSpecExprTool struct {
	Expr   exprs.Expr
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
	Expr    exprs.Expr
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
