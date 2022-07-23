package engine

import (
	"fmt"
	"github.com/heimdalr/dag"
	log "github.com/sirupsen/logrus"
	"go.starlark.net/starlark"
	"heph/utils"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"time"
)

type Package struct {
	Name        string
	FullName    string
	Root        Path
	Thread      *starlark.Thread
	Globals     starlark.StringDict
	SourceFiles SourceFiles
}

func (p Package) TargetPath(name string) string {
	return "//" + p.FullName + ":" + name
}

type Runnable struct {
	Cmds     []string
	Callable starlark.Callable
}

func (r *Runnable) Unpack(v starlark.Value) error {
	switch v := v.(type) {
	case starlark.Callable:
		r.Callable = v
		return nil
	case starlark.String:
		r.Cmds = []string{string(v)}
		return nil
	case *starlark.List:
		var a ArrayMap
		err := a.Unpack(v)
		if err != nil {
			return err
		}

		r.Cmds = a.Array
		return nil
	}

	return fmt.Errorf("must be string or []string, is %v", v.Type())
}

type TargetSpec struct {
	Name    string
	FQN     string
	Package *Package

	Runnable    Runnable `json:"-"`
	Quiet       bool
	Dir         string
	PassArgs    bool
	Deps        ArrayMap
	HashDeps    ArrayMap
	Tools       ArrayMap
	Out         StringArrayMap
	ShouldCache bool
	CachedFiles []string
	Sandbox     bool
	Codegen     string
	Labels      []string
	Env         map[string]string
}

type TargetTool struct {
	Target  *Target
	Name    string
	RelPath string
}

func (tt TargetTool) AbsPath() string {
	return filepath.Join(tt.Target.OutRoot.Abs, tt.Target.Package.Root.RelRoot, tt.RelPath)
}

type TargetDeps struct {
	Targets Targets
	Files   []PackagePath
}

type Target struct {
	TargetSpec

	Tools    []TargetTool
	Deps     TargetDeps
	HashDeps TargetDeps
	FilesOut []PackagePath
	// Files that have been cached
	actualFilesOut []PackagePath

	CodegenLink bool

	WorkdirRoot Path
	OutRoot     *Path
	CachedFiles []PackagePath
	LogFile     string

	linked bool
	ran    bool
	ranCh  chan struct{}
	m      sync.Mutex
}

func (t *Target) ActualFilesOut() []PackagePath {
	if t.actualFilesOut == nil {
		panic("actualFilesOut is nil for " + t.FQN)
	}

	return t.actualFilesOut
}

func (t *Target) String() string {
	return t.FQN
}

type Path struct {
	Abs     string
	RelRoot string
}

type PackagePath struct {
	Package *Package
	Path    string
	Root    string
}

func (fp PackagePath) Abs() string {
	if fp.Root != "" {
		return filepath.Join(fp.Root, fp.Package.Root.RelRoot, fp.Path)
	}

	return filepath.Join(fp.Package.Root.Abs, fp.Path)
}

func (fp PackagePath) RelRoot() string {
	return filepath.Join(fp.Package.Root.RelRoot, fp.Path)
}

func (fp PackagePath) WithRoot(root string) PackagePath {
	fp.Root = root

	return fp
}

func (t *Target) OutFilesInOutRoot() []PackagePath {
	out := make([]PackagePath, 0)
	for _, file := range t.FilesOut {
		out = append(out, file.WithRoot(t.OutRoot.Abs))
	}

	return out
}

func (t *Target) CachedFilesInOutRoot() []PackagePath {
	out := make([]PackagePath, 0)
	for _, file := range t.CachedFiles {
		out = append(out, file.WithRoot(t.OutRoot.Abs))
	}

	return out
}

func (t *Target) Private() bool {
	return strings.HasPrefix(t.Name, "_")
}

func (t *Target) WaitRan() <-chan struct{} {
	return t.ranCh
}

func (t *Target) HasAnyLabel(labels []string) bool {
	for _, clabel := range labels {
		for _, tlabel := range t.Labels {
			if clabel == tlabel {
				return true
			}
		}
	}

	return false
}

type Targets []*Target

func (t Targets) Find(fqn string) *Target {
	for _, target := range t {
		if target.FQN == fqn {
			return target
		}
	}

	return nil
}

func (t Targets) WaitAllRan() <-chan struct{} {
	doneCh := make(chan struct{})
	var wg sync.WaitGroup

	for _, target := range t {
		wg.Add(1)
		go func(target *Target) {
			<-target.WaitRan()
			wg.Done()
		}(target)
	}

	go func() {
		wg.Wait()
		close(doneCh)
	}()

	return doneCh
}

func (e *Engine) Parse() error {
	configStartTime := time.Now()
	err := e.parseConfigs()
	if err != nil {
		return err
	}
	log.Tracef("ParseConfigs took %v", time.Now().Sub(configStartTime))

	runStartTime := time.Now()
	err = e.runBuildFiles()
	if err != nil {
		return err
	}
	log.Tracef("RunBuildFiles took %v", time.Now().Sub(runStartTime))

	processStartTime := time.Now()
	for _, target := range e.Targets {
		err := e.processTarget(target)
		if err != nil {
			return fmt.Errorf("%v: %w", target.FQN, err)
		}
	}
	log.Tracef("ProcessTargets took %v", time.Now().Sub(processStartTime))

	e.dag = &DAG{dag.NewDAG()}

	linkStartTime := time.Now()
	for _, target := range e.Targets {
		err := e.linkTarget(target)
		if err != nil {
			return fmt.Errorf("linking %v: %w", target.FQN, err)
		}
	}
	log.Tracef("LinkTargets took %v", time.Now().Sub(linkStartTime))

	dagStartTime := time.Now()
	for _, target := range e.Targets {
		err = e.dag.AddVertexByID(target.FQN, target)
		if err != nil {
			return err
		}
	}

	for _, target := range e.Targets {
		for _, dep := range target.Deps.Targets {
			err := e.dag.AddEdge(dep.FQN, target.FQN)
			if err != nil && !isEdgeDuplicateError(err) {
				return fmt.Errorf("dep: %w", err)
			}
		}

		for _, tool := range target.Tools {
			err := e.dag.AddEdge(tool.Target.FQN, target.FQN)
			if err != nil && !isEdgeDuplicateError(err) {
				return fmt.Errorf("tool: %w", err)
			}
		}
	}
	log.Tracef("DAG took %v", time.Now().Sub(dagStartTime))

	return nil
}

func isEdgeDuplicateError(err error) bool {
	_, is := err.(dag.EdgeDuplicateError)
	return is
}

func TargetNotFoundError(target string) error {
	return fmt.Errorf("target %v not found", target)
}

func (e *Engine) processTarget(t *Target) error {
	// Validate FQN
	_, err := utils.TargetParse("", t.FQN)
	if err != nil {
		return fmt.Errorf("%v: %w", t.FQN, err)
	}

	t.ranCh = make(chan struct{})

	if t.Codegen != "" {
		if t.Codegen != "link" {
			return fmt.Errorf("codegen must be omitted or be equal to `link`")
		}

		if !t.Sandbox {
			return fmt.Errorf("codegen is only suported in sandbox mode")
		}

		t.CodegenLink = true
	}

	return nil
}

func (e *Engine) linkTarget(t *Target) error {
	if t.linked {
		return nil
	}
	defer func() {
		t.linked = true
	}()

	var err error

	log.Tracef("Linking %v", t.FQN)

	t.WorkdirRoot = Path{
		Abs:     e.Root,
		RelRoot: "",
	}
	if t.Sandbox {
		abs := filepath.Join(e.sandboxRoot(t), "_dir")
		rel, err := filepath.Rel(e.Root, abs)
		if err != nil {
			return err
		}

		t.WorkdirRoot = Path{
			Abs:     abs,
			RelRoot: rel,
		}
	}

	t.Tools = []TargetTool{}

	for _, tool := range t.TargetSpec.Tools.Array {
		tp, err := utils.TargetOutputParse(t.Package.FullName, tool)
		if err != nil {
			return err
		}

		tt := e.Targets.Find(tp.Full())
		if tt == nil {
			return TargetNotFoundError(tool)
		}

		err = e.linkTarget(tt)
		if err != nil {
			return err
		}

		if tp.Output != "" {
			if len(tt.Out.IMap) == 0 {
				return fmt.Errorf("target %v output must have named output", tt.Name)
			}
		}

		if len(tt.Out.IMap) > 0 {
			for name, path := range tt.Out.IMap {
				if tp.Output != "" {
					if name != tp.Output {
						continue
					}
				}

				t.Tools = append(t.Tools, TargetTool{
					Target:  tt,
					Name:    name,
					RelPath: path,
				})
			}
		} else if out := tt.Out.String; out != "" {
			t.Tools = append(t.Tools, TargetTool{
				Target:  tt,
				Name:    filepath.Base(out),
				RelPath: out,
			})
		} else {
			return fmt.Errorf("target %v output must be a string, map[string]string, got %#v", tt.Name, tt.Out)
		}
	}

	sort.SliceStable(t.Tools, func(i, j int) bool {
		return t.Tools[i].Name < t.Tools[j].Name
	})

	t.Deps, err = e.linkTargetDeps(t, t.TargetSpec.Deps)
	if err != nil {
		return err
	}

	if t.TargetSpec.HashDeps.Array != nil {
		t.HashDeps, err = e.linkTargetDeps(t, t.TargetSpec.HashDeps)
		if err != nil {
			return err
		}
	} else {
		t.HashDeps = t.Deps
	}

	createOutFile := func(t *Target, file string) PackagePath {
		return PackagePath{
			Package: t.Package,
			Path:    file,
		}
	}

	if s := t.TargetSpec.Out.String; s != "" {
		t.FilesOut = append(t.FilesOut, createOutFile(t, s))
	}

	for _, file := range t.TargetSpec.Out.Array {
		t.FilesOut = append(t.FilesOut, createOutFile(t, file))
	}

	sort.SliceStable(t.FilesOut, func(i, j int) bool {
		return t.FilesOut[i].RelRoot() < t.FilesOut[j].RelRoot()
	})

	if t.TargetSpec.ShouldCache {
		if !t.TargetSpec.Sandbox {
			return fmt.Errorf("%v cannot cache target which isnt sandboxed", t.FQN)
		}

		if len(t.TargetSpec.CachedFiles) == 0 {
			t.CachedFiles = t.FilesOut
		} else {
			for _, p := range t.TargetSpec.CachedFiles {
				t.CachedFiles = append(t.CachedFiles, PackagePath{
					Package: t.Package,
					Path:    p,
				})
			}
		}

		sort.SliceStable(t.CachedFiles, func(i, j int) bool {
			return t.CachedFiles[i].RelRoot() < t.CachedFiles[j].RelRoot()
		})
	}

	e.registerLabels(t.Labels)

	return nil
}

func (e *Engine) linkTargetDeps(t *Target, deps ArrayMap) (TargetDeps, error) {
	td := TargetDeps{}

	for _, dep := range deps.Array {
		// TODO: support named output
		dtp, err := utils.TargetParse(t.Package.FullName, dep)
		if err != nil { // Is probably file
			td.Files = append(td.Files, PackagePath{
				Package: t.Package,
				Path:    dep,
			})
			continue
		}

		dt := e.Targets.Find(dtp.Full())
		if dt == nil {
			return TargetDeps{}, TargetNotFoundError(dtp.Full())
		}

		err = e.linkTarget(dt)
		if err != nil {
			return TargetDeps{}, err
		}

		td.Targets = append(td.Targets, dt)
	}

	sort.SliceStable(td.Targets, func(i, j int) bool {
		return td.Targets[i].FQN < td.Targets[j].FQN
	})
	sort.SliceStable(td.Files, func(i, j int) bool {
		return td.Files[i].RelRoot() < td.Files[j].RelRoot()
	})

	return td, nil
}
