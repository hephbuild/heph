package targetspec

import (
	"encoding/json"
	"heph/exprs"
	"heph/packages"
	"time"
)

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

var (
	CodegenLink = "link"
	CodegenCopy = "copy"

	CodegenValues = []string{CodegenLink, CodegenCopy}
)

type TargetSpecSrcEnv struct {
	All   string
	Named map[string]string
}

func (e TargetSpecSrcEnv) Get(name string) string {
	if v, ok := e.Named[name]; ok {
		return v
	}

	return e.All
}

const SupportFilesOutput = "@support_files"

func SortOutputsForHashing(names []string) []string {
	index := -1
	for i, name := range names {
		if name == SupportFilesOutput {
			index = i
			break
		}
	}

	if index > 0 {
		n := make([]string, 0, len(names))
		n = append(n, SupportFilesOutput)
		for _, name := range names {
			if name == SupportFilesOutput {
				continue
			}
			n = append(n, name)
		}
		return n
	}

	return names
}

type TargetSpec struct {
	Name    string
	FQN     string
	Package *packages.Package

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
	RestoreCache      bool
	HasSupportFiles   bool
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
	SrcEnv            TargetSpecSrcEnv
	OutEnv            string
	HashFile          string
	Transitive        TargetSpecTransitive
	Timeout           time.Duration
}

type TargetSpecTransitive struct {
	Deps       TargetSpecDeps
	Tools      TargetSpecTools
	Env        map[string]string
	PassEnv    []string
	RuntimeEnv map[string]string
}

func (t TargetSpec) IsGroup() bool {
	return len(t.Run) == 1 && t.Run[0] == "group"
}

func (t TargetSpec) IsTextFile() bool {
	return len(t.Run) == 2 && t.Run[0] == "text_file"
}

func (t TargetSpec) Json() []byte {
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
	tj := t.Json()
	sj := spec.Json()

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
	Package *packages.Package
	Expr    exprs.Expr
}

type TargetSpecDepFile struct {
	Name string
	Path string
}

type TargetSpecOutFile struct {
	Name    string
	Package *packages.Package
	Path    string
}

type TargetSpecCache struct {
	Enabled bool
	Named   []string
	History int
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
