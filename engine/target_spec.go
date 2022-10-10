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
		Cache: TargetSpecCache{
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
		return TargetSpec{}, err
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

	t.Transitive.Tools, err = toolsSpecFromArgs(t, args.Transitive.Tools)
	if err != nil {
		return TargetSpec{}, err
	}

	t.Transitive.Deps, err = depsSpecFromArgs(t, args.Transitive.Deps)
	if err != nil {
		return TargetSpec{}, err
	}

	t.Transitive.Env = args.Transitive.Env.StrMap
	t.Transitive.PassEnv = args.Transitive.PassEnv.Array

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
			if k == "" {
				return t, fmt.Errorf("named output must not be empty")
			}

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
		if t.OutInSandbox {
			t.SrcEnv = FileEnvAbs
		} else {
			t.SrcEnv = FileEnvRelPkg
		}
	}
	if !validate(t.SrcEnv, FileEnvValues) {
		return TargetSpec{}, fmt.Errorf("src_env must be one of %v, got %v", printOneOf(FileEnvValues), t.SrcEnv)
	}

	if t.OutEnv == "" {
		if t.OutInSandbox {
			t.OutEnv = FileEnvAbs
		} else {
			t.OutEnv = FileEnvRelPkg
		}
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
			Name: name,
			Path: dep,
		})
	}

	return td
}

func toolsSpecFromString(t TargetSpec, ts *TargetSpecTools, name, tool string) error {
	expr, err := exprs.Parse(tool)
	if err == nil {
		ts.Exprs = append(ts.Exprs, TargetSpecExprTool{
			Name: name,
			Expr: expr,
		})
		return nil
	}

	tp, err := utils.TargetOutputParse(t.Package.FullName, tool)
	if err == nil {
		ts.Targets = append(ts.Targets, TargetSpecTargetTool{
			Name:   name,
			Target: tp.Full(),
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

	ts.Hosts = append(ts.Hosts, TargetSpecHostTool{
		Name: name,
		Path: binPath,
	})

	return nil
}

func toolsSpecFromArgs(t TargetSpec, tools ArrayMap) (TargetSpecTools, error) {
	ts := TargetSpecTools{}

	if len(tools.StrMap) > 0 {
		for name, s := range tools.StrMap {
			err := toolsSpecFromString(t, &ts, name, s)
			if err != nil {
				return TargetSpecTools{}, err
			}
		}
	} else {
		for _, s := range tools.Array {
			err := toolsSpecFromString(t, &ts, "", s)
			if err != nil {
				return TargetSpecTools{}, err
			}
		}
	}

	return ts, nil
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
	FileContent       []byte `json:"-"` // Used by special target `text_file`
	Executor          string
	Quiet             bool
	Dir               string
	PassArgs          bool
	Deps              TargetSpecDeps
	HashDeps          TargetSpecDeps
	DifferentHashDeps bool
	Tools             TargetSpecTools
	Out               []TargetSpecOutFile
	Cache             TargetSpecCache
	Sandbox           bool
	OutInSandbox      bool
	Codegen           string
	Labels            []string
	Env               map[string]string
	PassEnv           []string
	RunInCwd          bool
	Gen               bool
	Source            []string
	RuntimeEnv        map[string]string
	SrcEnv            string
	OutEnv            string
	HashFile          string
	Transitive        TargetSpecTransitive
}

type TargetSpecTransitive struct {
	Deps    TargetSpecDeps
	Tools   TargetSpecTools
	Env     map[string]string
	PassEnv []string
}

func (t TargetSpec) IsGroup() bool {
	return len(t.Run) == 1 && t.Run[0] == "group"
}

func (t TargetSpec) IsTextFile() bool {
	return len(t.Run) == 1 && t.Run[0] == "text_file"
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
	Name   string
	Target string
	Output string
}

type TargetSpecExprTool struct {
	Name   string
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

type TargetSpecTools struct {
	Targets []TargetSpecTargetTool
	Hosts   []TargetSpecHostTool
	Exprs   []TargetSpecExprTool
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
	Name string
	Path string
}

type TargetSpecOutFile struct {
	Name    string
	Package *Package
	Path    string
}

type TargetSpecCache struct {
	Enabled bool
	Named   []string
	Files   []string
}

func (c TargetSpecCache) NamedEnabled(name string) bool {
	if c.Named == nil {
		return true
	}

	for _, n := range c.Named {
		if n == name {
			return true
		}
	}

	return false
}
