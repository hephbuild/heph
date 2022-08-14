package engine

import (
	"context"
	"fmt"
	"github.com/heimdalr/dag"
	log "github.com/sirupsen/logrus"
	"go.starlark.net/starlark"
	"heph/upgrade"
	"heph/utils"
	"heph/worker"
	"os"
	"os/exec"
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
	PassEnv     []string
	RunInCwd    bool
	Gen         bool
	Source      []string
	Provide     map[string]string
}

type TargetTool struct {
	Target  *Target
	Name    string
	RelPath string
}

type HostTool struct {
	Name    string
	BinPath string
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

	Tools          []TargetTool
	HostTools      []HostTool
	Deps           TargetDeps
	HashDeps       TargetDeps
	FilesOut       []PackagePath
	actualFilesOut []PackagePath
	Env            map[string]string

	CodegenLink bool

	WorkdirRoot       Path
	OutRoot           *Path
	CachedFiles       []PackagePath
	actualcachedFiles []PackagePath
	LogFile           string

	processed bool
	linked    bool
	linking   bool
	linkingCh chan struct{}
	ran       bool
	ranCh     chan struct{}
	m         sync.Mutex

	runLock   *utils.Flock
	cacheLock *utils.Flock
}

func (t *Target) ActualFilesOut() PackagePaths {
	if t.actualFilesOut == nil {
		panic("actualFilesOut is nil for " + t.FQN)
	}

	return t.actualFilesOut
}

func (t *Target) String() string {
	return t.FQN
}

func (t *Target) IsGroup() bool {
	return len(t.Runnable.Cmds) == 0 && len(t.FilesOut) == 1 && t.FilesOut[0].Path == "*"
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
	return filepath.Join(fp.PkgRootAbs(), fp.Path)
}

// PkgRootAbs is the absolute path to the package root
func (fp PackagePath) PkgRootAbs() string {
	if fp.Root != "" {
		return filepath.Join(fp.Root, fp.Package.Root.RelRoot)
	}

	return fp.Package.Root.Abs
}

func (fp PackagePath) RelRoot() string {
	return filepath.Join(fp.Package.Root.RelRoot, fp.Path)
}

func (fp PackagePath) WithRoot(root string) PackagePath {
	fp.Root = root

	return fp
}

type PackagePaths []PackagePath

func (p PackagePaths) Find(s string) (PackagePath, bool) {
	for _, path := range p {
		if path.Path == s {
			return path, true
		}
	}

	return PackagePath{}, false
}

func (t *Target) OutFilesInOutRoot() []PackagePath {
	out := make([]PackagePath, 0)
	for _, file := range t.FilesOut {
		out = append(out, file.WithRoot(t.OutRoot.Abs))
	}

	return out
}

func (t *Target) ActualCachedFiles() []PackagePath {
	if t.actualcachedFiles == nil {
		panic("actualcachedFiles is nil for " + t.FQN)
	}

	return t.actualcachedFiles
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

func (t Targets) FQNs() []string {
	fqns := make([]string, 0)
	for _, target := range t {
		fqns = append(fqns, target.FQN)
	}

	return fqns
}

func (e *Engine) Parse() error {
	cwd, err := os.Getwd()
	if err != nil {
		return err
	}

	e.Cwd = cwd

	configStartTime := time.Now()
	err = e.parseConfigs()
	if err != nil {
		return err
	}
	log.Tracef("ParseConfigs took %v", time.Since(configStartTime))

	upgrade.CheckAndUpdate(e.Config.Config)

	runStartTime := time.Now()
	err = e.runBuildFiles()
	if err != nil {
		return err
	}
	log.Tracef("RunBuildFiles took %v", time.Since(runStartTime))

	sort.SliceStable(e.Targets, func(i, j int) bool {
		return e.Targets[i].FQN < e.Targets[j].FQN
	})

	processStartTime := time.Now()
	for _, target := range e.Targets {
		err := e.processTarget(target)
		if err != nil {
			return fmt.Errorf("%v: %w", target.FQN, err)
		}
	}
	log.Tracef("ProcessTargets took %v", time.Since(processStartTime))

	return nil
}

func (e *Engine) Simplify() error {
	return e.linkTargets(true, nil)
}

func (e *Engine) ScheduleStaticAnalysis(ctx context.Context, pool *worker.Pool) error {
	if e.ranStatAn {
		return nil
	}

	genTargets := e.GeneratedTargets()

	if len(genTargets) == 0 {
		linkStartTime := time.Now()
		err := e.linkTargets(false, nil)
		if err != nil {
			return fmt.Errorf("linking %w", err)
		}
		log.Tracef("LinkTargets took %v", time.Since(linkStartTime))

		err = e.createDag(nil)
		if err != nil {
			return err
		}

		return nil
	}

	log.Tracef("Run static analysis")

	linkStartTime := time.Now()
	err := e.linkTargets(false, genTargets)
	if err != nil {
		return fmt.Errorf("linking %w", err)
	}
	log.Tracef("LinkTargets took %v", time.Since(linkStartTime))

	linkedTargets := make(Targets, 0)
	for _, target := range e.Targets {
		if target.linked {
			linkedTargets = append(linkedTargets, target)
		}
	}

	err = e.createDag(linkedTargets)
	if err != nil {
		return err
	}

	var wg utils.WaitGroupChan

	for _, target := range genTargets {
		_, err := e.ScheduleTargetDeps(ctx, pool, target)
		if err != nil {
			return err
		}

		err = e.ScheduleTarget(ctx, pool, target)
		if err != nil {
			return err
		}

		err = e.ScheduleRunGenerated(pool, target, &wg)
		if err != nil {
			return err
		}
	}

	pool.Schedule(&worker.Job{
		ID: "post static analysis",
		Wait: func(ctx context.Context) {
			select {
			case <-ctx.Done():
			case <-wg.Done():
			}
		},
		Do: func(w *worker.Worker, ctx context.Context) error {
			w.Status("Finalize static analysis...")

			err := e.Simplify()
			if err != nil {
				return err
			}

			err = e.createDag(nil)
			if err != nil {
				return err
			}

			return nil
		},
	})

	e.ranStatAn = true

	return nil
}

func (e *Engine) ScheduleRunGenerated(pool *worker.Pool, target *Target, wg *utils.WaitGroupChan) error {
	ancestors, err := e.DAG().GetAncestors(target)
	if err != nil {
		return err
	}

	deps := append(ancestors, target)

	log.Tracef("Scheduling rungen %v", target.FQN)

	pool.ScheduleWith(
		worker.ScheduleOptions{
			OnSchedule: func() {
				wg.Add()
			},
		},
		&worker.Job{
			ID: "rungen-" + target.FQN,
			Wait: func(ctx context.Context) {
				select {
				case <-ctx.Done():
				case <-deps.WaitAllRan():
				}
				return
			},
			Do: func(w *worker.Worker, ctx context.Context) (ferr error) {
				w.Status(fmt.Sprintf("Static analysis on %v...", target.FQN))
				defer func() {
					w.Status(fmt.Sprintf("Static analysis on %v done: %v", target.FQN, ferr))
				}()

				err := e.runGenerated(target)
				if err != nil {
					return TargetFailedError{
						Target: target,
						Err:    err,
					}
				}

				wg.Sub()

				return nil
			},
		})

	return nil
}

func isEdgeDuplicateError(err error) bool {
	_, is := err.(dag.EdgeDuplicateError)
	return is
}

func TargetNotFoundError(target string) error {
	return fmt.Errorf("target %v not found", target)
}

func (e *Engine) createDag(targets Targets) error {
	if targets == nil {
		targets = e.Targets
	}

	e.dag = &DAG{dag.NewDAG()}

	dagStartTime := time.Now()
	for _, target := range targets {
		err := e.dag.AddVertexByID(target.FQN, target)
		if err != nil {
			return err
		}
	}

	for _, target := range targets {
		for _, dep := range target.Deps.Targets {
			err := e.dag.AddEdge(dep.FQN, target.FQN)
			if err != nil && !isEdgeDuplicateError(err) {
				return fmt.Errorf("dep: %v to %v: %w", dep.FQN, target.FQN, err)
			}
		}

		for _, tool := range target.Tools {
			err := e.dag.AddEdge(tool.Target.FQN, target.FQN)
			if err != nil && !isEdgeDuplicateError(err) {
				return fmt.Errorf("tool: %v to %v: %w", tool.Target.FQN, target.FQN, err)
			}
		}
	}
	log.Tracef("DAG took %v", time.Since(dagStartTime))

	return nil
}

func (e *Engine) processTarget(t *Target) error {
	// Validate FQN
	_, err := utils.TargetParse("", t.FQN)
	if err != nil {
		return fmt.Errorf("%v: %w", t.FQN, err)
	}

	t.ranCh = make(chan struct{})
	t.runLock = utils.NewFlock(e.lockPath(t, "run"))
	t.cacheLock = utils.NewFlock(e.lockPath(t, "cache"))

	if t.Codegen != "" {
		if t.Codegen != "link" {
			return fmt.Errorf("codegen must be omitted or be equal to `link`")
		}

		if !t.Sandbox {
			return fmt.Errorf("codegen is only suported in sandbox mode")
		}

		t.CodegenLink = true
	}

	t.processed = true

	return nil
}

func (e *Engine) linkTargets(simplify bool, targets Targets) error {
	if targets == nil {
		targets = e.Targets
	}

	for _, target := range targets {
		target.linked = false
	}

	for i, target := range targets {
		log.Tracef("# Linking target %v %v/%v", target.FQN, i+1, len(targets))
		err := e.linkTarget(target, simplify, nil)
		if err != nil {
			return fmt.Errorf("%v: %w", target.FQN, err)
		}
	}

	for _, target := range targets {
		target.Deps = e.filterOutCodegenFromDeps(target, target.Deps)
		target.HashDeps = e.filterOutCodegenFromDeps(target, target.HashDeps)
	}

	return nil
}

func (e *Engine) filterOutCodegenFromDeps(t *Target, td TargetDeps) TargetDeps {
	files := make(PackagePaths, 0)
	for _, file := range td.Files {
		if dep, ok := e.codegenPaths[file.RelRoot()]; ok {
			log.Tracef("%v: %v removed from deps, and %v outputs it", t.FQN, file.RelRoot(), dep.FQN)
		} else {
			files = append(files, file)
		}
	}
	td.Files = files

	return td
}

func (e *Engine) linkTarget(t *Target, simplify bool, breadcrumb Targets) error {
	if !t.processed {
		panic(fmt.Sprintf("%v has not been processed", t.FQN))
	}

	for _, target := range breadcrumb {
		if target.FQN == t.FQN {
			breadcrumb = append(breadcrumb, t)
			return fmt.Errorf("linking cycle: %v", breadcrumb.FQNs())
		}
	}
	breadcrumb = append(breadcrumb, t)

	logPrefix := strings.Repeat("|", len(breadcrumb)-1)

	t.m.Lock()
	if t.linked {
		t.m.Unlock()
		return nil
	} else if t.linking {
		t.m.Unlock()
		<-t.linkingCh
		return nil
	}
	t.linking = true
	t.linkingCh = make(chan struct{})
	defer func() {
		t.linked = true
		t.linking = false
		close(t.linkingCh)
	}()
	t.m.Unlock()

	var err error

	log.Tracef(logPrefix+"Linking %v", t.FQN)
	defer func() {
		log.Tracef(logPrefix+"Linking %v done", t.FQN)
	}()

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

	log.Tracef(logPrefix + "Linking tools")

	for _, tool := range t.TargetSpec.Tools.Array {
		tp, err := utils.TargetOutputParse(t.Package.FullName, tool)
		if err != nil {
			binPath, err := exec.LookPath(tool)
			if err != nil {
				return fmt.Errorf("%v is not a target, and cannot be found in PATH", tool)
			}

			log.Tracef("%v Using tool %v from %v", t.FQN, tool, binPath)

			t.HostTools = append(t.HostTools, HostTool{
				Name:    tool,
				BinPath: binPath,
			})
			continue
		}

		tt := e.Targets.Find(tp.Full())
		if tt == nil {
			return TargetNotFoundError(tool)
		}

		err = e.linkTarget(tt, simplify, breadcrumb)
		if err != nil {
			return fmt.Errorf("tool: %v: %w", tool, err)
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
				Name:    tt.Name,
				RelPath: out,
			})
		} else {
			return fmt.Errorf("target %v output must be a string, map[string]string, got %#v", tt.Name, tt.Out)
		}
	}

	sort.SliceStable(t.HostTools, func(i, j int) bool {
		return t.HostTools[i].Name < t.HostTools[j].Name
	})

	sort.SliceStable(t.Tools, func(i, j int) bool {
		return t.Tools[i].Name < t.Tools[j].Name
	})

	log.Tracef(logPrefix + "Linking deps")

	t.Deps, err = e.linkTargetDeps(t, t.TargetSpec.Deps, simplify, breadcrumb)
	if err != nil {
		return fmt.Errorf("%v: deps: %w", t.FQN, err)
	}

	log.Tracef(logPrefix + "Linking hashdeps")

	if t.TargetSpec.HashDeps.Array != nil {
		t.HashDeps, err = e.linkTargetDeps(t, t.TargetSpec.HashDeps, simplify, breadcrumb)
		if err != nil {
			return fmt.Errorf("%v: hashdeps: %w", t.FQN, err)
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

	t.FilesOut = []PackagePath{}
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
			t.CachedFiles = []PackagePath{}
			for _, p := range t.TargetSpec.CachedFiles {
				t.CachedFiles = append(t.CachedFiles, PackagePath{
					Package: t.Package,
					Path:    p,
				})
			}

			sort.SliceStable(t.CachedFiles, func(i, j int) bool {
				return t.CachedFiles[i].RelRoot() < t.CachedFiles[j].RelRoot()
			})
		}
	}

	t.Env = map[string]string{}
	for _, name := range t.TargetSpec.PassEnv {
		value, ok := os.LookupEnv(name)
		if !ok {
			continue
		}
		t.Env[name] = value
	}
	for k, v := range t.TargetSpec.Env {
		t.Env[k] = v
	}

	e.registerLabels(t.Labels)

	if t.CodegenLink {
		for _, file := range t.FilesOut {
			if strings.Contains(file.Path, "*") {
				return fmt.Errorf("codegen must not have glob outputs")
			}

			if e.codegenPaths == nil {
				e.codegenPaths = map[string]*Target{}
			}

			p := file.RelRoot()

			if ct, ok := e.codegenPaths[p]; ok && ct != t {
				return fmt.Errorf("%v: target %v codegen alredy outputs %v", t.FQN, ct.FQN, p)
			}

			e.codegenPaths[p] = t
		}
	}

	return nil
}

func (e *Engine) linkTargetDeps(t *Target, deps ArrayMap, simplify bool, breadcrumb Targets) (TargetDeps, error) {
	td := TargetDeps{}

	for _, dep := range deps.Array {
		if expr, err := utils.ExprParse(dep); err == nil {
			switch expr.Function {
			case "collect":
				targets, err := e.collect(t, expr)
				if err != nil {
					return TargetDeps{}, err
				}

				for _, target := range targets {
					err := e.linkTarget(target, simplify, breadcrumb)
					if err != nil {
						return TargetDeps{}, fmt.Errorf("collect: %w", err)
					}

					td.Targets = append(td.Targets, target)
				}
			case "find_parent":
				target, err := e.findParent(t, expr)
				if err != nil {
					return TargetDeps{}, err
				}

				err = e.linkTarget(target, simplify, breadcrumb)
				if err != nil {
					return TargetDeps{}, fmt.Errorf("find_parent: %w", err)
				}

				td.Targets = append(td.Targets, target)
			default:
				return TargetDeps{}, fmt.Errorf("unhandled function %v", expr.Function)
			}
			continue
		}

		// TODO: support named output
		if dtp, err := utils.TargetParse(t.Package.FullName, dep); err == nil {
			dt := e.Targets.Find(dtp.Full())
			if dt == nil {
				return TargetDeps{}, TargetNotFoundError(dtp.Full())
			}

			err = e.linkTarget(dt, simplify, breadcrumb)
			if err != nil {
				return TargetDeps{}, err
			}

			td.Targets = append(td.Targets, dt)
			continue
		}

		// Is probably file
		td.Files = append(td.Files, PackagePath{
			Package: t.Package,
			Path:    dep,
		})
	}

	if simplify {
		targets := make(Targets, 0)
		for _, dep := range td.Targets {
			if dep.IsGroup() {
				targets = append(targets, dep.Deps.Targets...)
				td.Files = append(td.Files, dep.Deps.Files...)
			} else {
				targets = append(targets, dep)
			}
		}
		td.Targets = targets
	}

	td.Targets = utils.DedupKeepLast(td.Targets, func(t *Target) string {
		return t.FQN
	})
	td.Files = utils.DedupKeepLast(td.Files, func(t PackagePath) string {
		return t.RelRoot()
	})

	sort.SliceStable(td.Targets, func(i, j int) bool {
		return td.Targets[i].FQN < td.Targets[j].FQN
	})
	sort.SliceStable(td.Files, func(i, j int) bool {
		return td.Files[i].RelRoot() < td.Files[j].RelRoot()
	})

	return td, nil
}
