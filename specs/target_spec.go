package specs

import (
	"encoding/json"
	"github.com/hephbuild/heph/exprs"
	"github.com/hephbuild/heph/packages"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/maps"
	"github.com/hephbuild/heph/utils/xsync"
	"go.starlark.net/syntax"
	"os/exec"
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
	CodegenNone          = ""
	CodegenLink          = "link"
	CodegenCopy          = "copy"
	CodegenCopyNoExclude = "copy_noexclude"

	CodegenValues = []string{CodegenNone, CodegenLink, CodegenCopy, CodegenCopyNoExclude}
)

type SrcEnv struct {
	Default string
	Named   map[string]string
}

func (e SrcEnv) Get(name string) string {
	if v, ok := e.Named[name]; ok {
		return v
	}

	return e.Default
}

const SupportFilesOutput = "@support_files"

func SortOutputsForHashing(names []string) []string {
	names = ads.Copy(names)
	ads.SortFunc(names, func(a, b string) bool {
		if a == SupportFilesOutput {
			return true
		}
		return strings.Compare(a, b) < 0
	})
	return names
}

type Targets []Target

func (ts Targets) Addrs() []string {
	return ads.Map(ts, func(t Target) string {
		return t.Addr
	})
}

func (ts Targets) Get(addr string) (Target, bool) {
	for _, t := range ts {
		if t.Addr == addr {
			return t, true
		}
	}

	return Target{}, false
}

type Target struct {
	Name    string
	Addr    string
	Package *packages.Package
	Doc     string

	Run                 []string
	FileContent         []byte // Used by special target `text_file`
	Entrypoint          string
	Platforms           []Platform
	ConcurrentExecution bool
	Dir                 string
	PassArgs            bool
	Deps                Deps
	HashDeps            Deps
	DifferentHashDeps   bool
	Tools               Tools
	Out                 []OutFile
	Cache               Cache
	RestoreCache        RestoreCache
	HasSupportFiles     bool
	Sandbox             bool
	OutInSandbox        bool
	Codegen             string
	Labels              []string
	Env                 map[string]string
	PassEnv             []string
	RuntimePassEnv      []string
	RunInCwd            bool
	Gen                 []Matcher
	Sources             []Source
	RuntimeEnv          map[string]string
	SrcEnv              SrcEnv
	OutEnv              string
	HashFile            string
	Transitive          Transitive
	Timeout             time.Duration
	GenDepsMeta         bool
	Annotations         map[string]interface{}
}

type Specer interface {
	Spec() Target
}

func (t Target) Spec() Target {
	return t
}

func AsSpecers[T Specer](a []T) []Specer {
	return ads.Map(a, func(t T) Specer {
		return t
	})
}

type Source struct {
	CallFrames []SourceCallFrame
}

func (s Source) SourceFile() string {
	if len(s.CallFrames) < 1 {
		return ""
	}

	return s.CallFrames[0].Pos.String()
}

type SourceCallFrame struct {
	Name string
	Pos  SourceCallFramePosition
}

func (s SourceCallFrame) String() string {
	return s.Name + " " + s.Pos.String()
}

type SourceCallFramePosition struct {
	syntax.Position
}

func (u *SourceCallFramePosition) MarshalJSON() ([]byte, error) {
	return json.Marshal(&struct {
		File string
		Line int32
	}{
		File: u.Filename(),
		Line: u.Line,
	})
}

type Platform struct {
	Labels  map[string]string
	Options map[string]interface{}
	Default bool
}

type Transitive struct {
	Deps           Deps
	Tools          Tools
	Env            map[string]string
	PassEnv        []string
	RuntimePassEnv []string
	RuntimeEnv     map[string]string
	Platforms      []Platform
}

func (t Target) IsPrivate() bool {
	return strings.HasPrefix(t.Name, "_")
}

func (t Target) AddrStruct() TargetAddr {
	return TargetAddr{
		Package: t.Package.Path,
		Name:    t.Name,
	}
}

func (t Target) IsGroup() bool {
	return len(t.Run) == 1 && t.Run[0] == "group"
}

func (t Target) IsTool() bool {
	return len(t.Run) >= 1 && t.Run[0] == "heph_tool"
}

func (t Target) IsGen() bool {
	return len(t.Gen) > 0
}

func (t Target) IsTextFile() bool {
	return len(t.Run) == 2 && t.Run[0] == "text_file"
}

func (t Target) IsExecutable() bool {
	return !t.IsGroup() && !t.IsTextFile()
}

func (t Target) HasDefaultPlatforms() bool {
	return len(t.Platforms) == 1 && t.Platforms[0].Default
}

func (t Target) Json() []byte {
	t.Package = nil
	t.Sources = nil

	b, err := json.Marshal(t)
	if err != nil {
		panic(err)
	}

	return b
}

func (t Target) Equal(spec Target) bool {
	return t.equalStruct(spec)
}

func (t Target) equalJson(spec Target) bool {
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

func (t Target) SourceFile() string {
	if len(t.Sources) < 1 {
		return ""
	}

	return t.Sources[0].SourceFile()
}

type TargetTool struct {
	Name   string
	Target string
	Output string
}

type ExprTool struct {
	Name   string
	Expr   exprs.Expr
	Output string
}

type HostTool struct {
	Name    string
	BinName string
	Path    string
}

var binCache = maps.Map[string, xsync.Once[string]]{}

func (t HostTool) ResolvedPath() (string, error) {
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

type Deps struct {
	Targets []DepTarget
	Files   []DepFile
	Exprs   []DepExpr
}

type Tools struct {
	Targets []TargetTool
	Hosts   []HostTool
	Exprs   []ExprTool
}

type DepMode string

const (
	DepModeCopy DepMode = "copy"
	DepModeLink DepMode = "link"
)

var DepModes = []DepMode{
	DepModeCopy,
	DepModeLink,
}

type DepTarget struct {
	Name   string
	Output string
	Target string
	Mode   DepMode
}

type DepExpr struct {
	Name string
	Expr exprs.Expr
}

type DepFile struct {
	Name string
	Path string
}

type OutFile struct {
	Name string
	Path string
}

type Cache struct {
	Enabled bool
	Named   []string
	History int
}

func (c Cache) NamedEnabled(name string) bool {
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

type RestoreCache struct {
	Enabled bool
	Key     string
	Paths   []string
	Env     string
}
