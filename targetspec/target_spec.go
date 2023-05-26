package targetspec

import (
	"encoding/json"
	"github.com/hephbuild/heph/exprs"
	"github.com/hephbuild/heph/packages"
	"github.com/hephbuild/heph/utils"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/maps"
	"go.starlark.net/syntax"
	"os/exec"
	"sort"
	"strings"
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
	EntrypointExec = "exec"
	EntrypointBash = "bash"
	EntrypointSh   = "sh"

	EntrypointValues = []string{EntrypointExec, EntrypointSh, EntrypointBash}
)

var (
	CodegenLink          = "link"
	CodegenCopy          = "copy"
	CodegenCopyNoExclude = "copy_noexclude"

	CodegenValues = []string{CodegenLink, CodegenCopy, CodegenCopyNoExclude}
)

type TargetSpecSrcEnv struct {
	Default string
	Named   map[string]string
}

func (e TargetSpecSrcEnv) Get(name string) string {
	if v, ok := e.Named[name]; ok {
		return v
	}

	return e.Default
}

const SupportFilesOutput = "@support_files"

func SortOutputsForHashing(names []string) []string {
	names = ads.Copy(names)
	sort.Slice(names, func(i, j int) bool {
		if names[i] == SupportFilesOutput {
			return true
		}
		return strings.Compare(names[i], names[j]) < 0
	})
	return names
}

type TargetSpecs []TargetSpec

func (ts TargetSpecs) FQNs() []string {
	fqns := make([]string, 0, len(ts))
	for _, t := range ts {
		fqns = append(fqns, t.FQN)
	}

	return fqns
}

func (ts TargetSpecs) Get(fqn string) (TargetSpec, bool) {
	for _, t := range ts {
		if t.FQN == fqn {
			return t, true
		}
	}

	return TargetSpec{}, false
}

type TargetSpec struct {
	Name    string
	FQN     string
	Package *packages.Package
	Doc     string

	Run                 []string
	FileContent         []byte // Used by special target `text_file`
	Entrypoint          string
	Platforms           []TargetPlatform
	ConcurrentExecution bool
	Quiet               bool
	Dir                 string
	PassArgs            bool
	Deps                TargetSpecDeps
	HashDeps            TargetSpecDeps
	DifferentHashDeps   bool
	Tools               TargetSpecTools
	Out                 []TargetSpecOutFile
	Cache               TargetSpecCache
	RestoreCache        bool
	HasSupportFiles     bool
	Sandbox             bool
	OutInSandbox        bool
	Codegen             string
	Labels              []string
	Env                 map[string]string
	PassEnv             []string
	RuntimePassEnv      []string
	RunInCwd            bool
	Gen                 bool
	Source              []TargetSource
	RuntimeEnv          map[string]string
	SrcEnv              TargetSpecSrcEnv
	OutEnv              string
	HashFile            string
	Transitive          TargetSpecTransitive
	Timeout             time.Duration
	GenDepsMeta         bool
}

type Specer interface {
	Spec() TargetSpec
}

func (t TargetSpec) Spec() TargetSpec {
	return t
}

func AsSpecers[T Specer](a []T) []Specer {
	return ads.Map(a, func(t T) Specer {
		return t
	})
}

type TargetSource struct {
	Name string
	Pos  syntax.Position
}

func (s TargetSource) String() string {
	return s.Name + " " + s.Pos.String()
}

type TargetPlatform struct {
	Labels  map[string]string
	Options map[string]interface{}
}

type TargetSpecTransitive struct {
	Deps           TargetSpecDeps
	Tools          TargetSpecTools
	Env            map[string]string
	PassEnv        []string
	RuntimePassEnv []string
	RuntimeEnv     map[string]string
}

func (t TargetSpec) IsPrivate() bool {
	return strings.HasPrefix(t.Name, "_")
}

func (t TargetSpec) IsGroup() bool {
	return len(t.Run) == 1 && t.Run[0] == "group"
}

func (t TargetSpec) IsTool() bool {
	return len(t.Run) > 0 && t.Run[0] == "heph_tool"
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
	Name    string
	BinName string
	Path    string
}

var binCache = maps.Map[string, utils.Once[string]]{}

func (t TargetSpecHostTool) ResolvedPath() (string, error) {
	if t.Path != "" {
		return t.Path, nil
	}

	if t.BinName == "heph" {
		panic("heph should be handled separately")
	}

	once := binCache.Get(t.BinName)

	return once.Do(func() (string, error) {
		return exec.LookPath(t.BinName)
	})
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

type TargetSpecDepMode string

const (
	TargetSpecDepModeCopy TargetSpecDepMode = "copy"
	TargetSpecDepModeLink TargetSpecDepMode = "link"
)

var TargetSpecDepModes = []TargetSpecDepMode{
	TargetSpecDepModeCopy,
	TargetSpecDepModeLink,
}

type TargetSpecDepTarget struct {
	Name   string
	Output string
	Target string
	Mode   TargetSpecDepMode
}

type TargetSpecDepExpr struct {
	Name string
	Expr exprs.Expr
}

type TargetSpecDepFile struct {
	Name string
	Path string
}

type TargetSpecOutFile struct {
	Name string
	Path string
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
