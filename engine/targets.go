package engine

import (
	"fmt"
	"github.com/heimdalr/dag"
	log "github.com/sirupsen/logrus"
	"heph/exprs"
	"heph/upgrade"
	"heph/utils"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"time"
)

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
	return filepath.Join(tt.Target.OutRoot.Abs(), tt.Target.Package.FullName, tt.RelPath)
}

type TargetDeps struct {
	Targets []TargetWithOutput
	Files   []PackagePath
}

type TargetNamedPackagePath struct {
	names []string
	named map[string]PackagePaths
	all   PackagePaths
}

func (tp *TargetNamedPackagePath) Named() map[string]PackagePaths {
	return tp.named
}

func (tp *TargetNamedPackagePath) All() PackagePaths {
	return tp.all
}

func (tp *TargetNamedPackagePath) Name(name string) PackagePaths {
	if tp.named == nil {
		return nil
	}

	return tp.named[name]
}

func (tp *TargetNamedPackagePath) Add(name string, p PackagePath) {
	if tp.named == nil {
		tp.named = map[string]PackagePaths{}
	}

	if _, ok := tp.named[name]; !ok {
		tp.names = append(tp.names, name)
		tp.named[name] = make(PackagePaths, 0)
	}

	tp.named[name] = append(tp.named[name], p)
	tp.all = append(tp.all, p)
}

func (tp *TargetNamedPackagePath) Sort() {
	sort.SliceStable(tp.all, func(i, j int) bool {
		return tp.all[i].RelRoot() < tp.all[j].RelRoot()
	})

	for name := range tp.named {
		sort.SliceStable(tp.named[name], func(i, j int) bool {
			return tp.named[name][i].RelRoot() < tp.named[name][j].RelRoot()
		})
	}
}

func (tp TargetNamedPackagePath) WithRoot(root string) *TargetNamedPackagePath {
	ntp := &TargetNamedPackagePath{
		names: tp.names,
		named: map[string]PackagePaths{},
		all:   tp.all.WithRoot(root),
	}

	for name, paths := range tp.named {
		ntp.named[name] = paths.WithRoot(root)
	}

	return ntp
}

type TargetNamedDeps struct {
	named map[string]TargetDeps
	all   TargetDeps
}

func (tp *TargetNamedDeps) Set(name string, p TargetDeps) {
	if tp.named == nil {
		tp.named = map[string]TargetDeps{}
	}

	tp.named[name] = p
	tp.all.Targets = append(tp.all.Targets, p.Targets...)
	tp.all.Files = append(tp.all.Files, p.Files...)
}

func (tp *TargetNamedDeps) Named() map[string]TargetDeps {
	return tp.named
}

func (tp *TargetNamedDeps) All() TargetDeps {
	return tp.all
}

func (tp *TargetNamedDeps) Name(name string) TargetDeps {
	if tp.named == nil {
		return TargetDeps{}
	}

	return tp.named[name]
}

func (tp *TargetNamedDeps) Names() []string {
	names := make([]string, 0, len(tp.named))
	for name := range tp.named {
		names = append(names, name)
	}
	sort.Strings(names)
	return names
}

func (tp *TargetNamedDeps) Map(fn func(deps TargetDeps) TargetDeps) {
	for name, deps := range tp.named {
		tp.named[name] = fn(deps)
	}

	tp.all = fn(tp.all)
}

func (tp *TargetNamedDeps) Sort() {
	tp.Map(func(deps TargetDeps) TargetDeps {
		sort.SliceStable(deps.Targets, func(i, j int) bool {
			return deps.Targets[i].Target.FQN < deps.Targets[j].Target.FQN
		})

		sort.SliceStable(deps.Files, func(i, j int) bool {
			return deps.Files[i].Path < deps.Files[j].Path
		})

		return deps
	})
}

type Target struct {
	TargetSpec

	Tools          []TargetTool
	Deps           TargetNamedDeps
	HashDeps       TargetDeps
	Out            *TargetNamedPackagePath
	actualFilesOut *TargetNamedPackagePath
	Env            map[string]string

	CodegenLink bool

	WorkdirRoot       Path
	SandboxRoot       Path
	OutRoot           *Path
	CacheFiles        PackagePaths
	actualcachedFiles PackagePaths
	LogFile           string

	processed bool
	linked    bool
	linking   bool
	linkingCh chan struct{}
	m         sync.Mutex

	runLock   utils.Locker
	cacheLock utils.Locker
}

func (t *Target) ID() string {
	return t.FQN
}

func (t *Target) ActualFilesOut() PackagePaths {
	if t.actualFilesOut == nil {
		panic("actualFilesOut is nil for " + t.FQN)
	}

	return t.actualFilesOut.all
}

func (t *Target) NamedActualFilesOut() *TargetNamedPackagePath {
	if t.actualFilesOut == nil {
		panic("actualFilesOut is nil for " + t.FQN)
	}

	return t.actualFilesOut
}

func (t *Target) String() string {
	return t.FQN
}

func (t *Target) IsGroup() bool {
	return len(t.Run) == 0 && len(t.Out.All()) == 1 && (t.Out.All()[0].Path == "*" || t.Out.All()[0].Path == "/")
}

func Contains(ts []*Target, fqn string) bool {
	for _, t := range ts {
		if t.FQN == fqn {
			return true
		}
	}

	return false
}

type PackagePath struct {
	Package *Package
	// Use Package.Fullname instead of Package.Root.RelRoot
	PackagePath bool
	Path        string
	Root        string
}

func (fp PackagePath) Abs() string {
	var repoRoot string
	if fp.Root != "" {
		repoRoot = fp.Root
	} else {
		repoRoot = fp.Package.Root.Root
	}

	var pkgRoot string
	if fp.PackagePath {
		pkgRoot = filepath.Join(repoRoot, fp.Package.FullName)
	} else {
		pkgRoot = filepath.Join(repoRoot, fp.Package.Root.RelRoot)
	}

	return filepath.Join(pkgRoot, fp.Path)
}

func (fp PackagePath) RelRoot() string {
	base := fp.Package.Root.RelRoot
	if fp.PackagePath {
		base = fp.Package.FullName
	}
	return filepath.Join(base, fp.Path)
}

func (fp PackagePath) WithRoot(root string) PackagePath {
	fp.Root = root

	return fp
}

func (fp PackagePath) WithPackagePath(v bool) PackagePath {
	fp.PackagePath = v

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

func (p PackagePaths) WithRoot(root string) PackagePaths {
	np := make(PackagePaths, 0, len(p))

	for _, path := range p {
		np = append(np, path.WithRoot(root))
	}

	return np
}

func (t *Target) OutFilesInOutRoot() []PackagePath {
	out := make([]PackagePath, 0)
	for _, file := range t.Out.All() {
		out = append(out, file.WithRoot(t.OutRoot.Abs()))
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

type TargetWithOutput struct {
	Target *Target
	Output string
}

type Targets struct {
	mu sync.RWMutex
	m  map[string]*Target
	a  []*Target
}

func NewTargets(cap int) *Targets {
	t := &Targets{}
	t.m = make(map[string]*Target, cap)
	t.a = make([]*Target, 0, cap)

	return t
}

func (ts *Targets) Add(t *Target) {
	ts.mu.Lock()
	defer ts.mu.Unlock()

	if _, ok := ts.m[t.FQN]; ok {
		return
	}

	ts.m[t.FQN] = t
	ts.a = append(ts.a, t)
}

func (ts *Targets) Find(fqn string) *Target {
	if ts == nil {
		return nil
	}

	ts.mu.RLock()
	defer ts.mu.RUnlock()

	return ts.m[fqn]
}

func (ts *Targets) FQNs() []string {
	if ts == nil {
		return nil
	}

	fqns := make([]string, 0)
	for _, target := range ts.a {
		fqns = append(fqns, target.FQN)
	}

	return fqns
}

func (ts *Targets) Sort() {
	if ts == nil {
		return
	}

	sort.SliceStable(ts.a, func(i, j int) bool {
		return ts.a[i].FQN < ts.a[j].FQN
	})
}

func (ts *Targets) Slice() []*Target {
	if ts == nil {
		return nil
	}

	return ts.a[:]
}

func (ts *Targets) Len() int {
	if ts == nil {
		return 0
	}

	return len(ts.a)
}

func (ts *Targets) Copy() *Targets {
	t := NewTargets(ts.Len())

	for _, target := range ts.Slice() {
		t.Add(target)
	}

	return t
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
	log.Debugf("ParseConfigs took %v", time.Since(configStartTime))

	err = upgrade.CheckAndUpdate(e.Config.Config)
	if err != nil {
		return fmt.Errorf("upgrade: %w", err)
	}

	for name, cfg := range e.Config.BuildFiles.Roots {
		err := e.runRootBuildFiles(name, cfg)
		if err != nil {
			return fmt.Errorf("root %v: %w", name, err)
		}
	}

	runStartTime := time.Now()
	err = e.runBuildFiles(e.Root.Abs(), func(dir string) *Package {
		return e.createPkg(dir)
	})
	if err != nil {
		return err
	}
	log.Debugf("RunBuildFiles took %v", time.Since(runStartTime))

	e.Targets.Sort()

	processStartTime := time.Now()
	for _, target := range e.Targets.Slice() {
		err := e.processTarget(target)
		if err != nil {
			return fmt.Errorf("%v: %w", target.FQN, err)
		}
	}
	log.Debugf("ProcessTargets took %v", time.Since(processStartTime))

	linkStartTime := time.Now()
	err = e.linkTargets(false, e.noRequireGenTargets())
	if err != nil {
		return err
	}
	log.Debugf("ProcessTargets took %v", time.Since(linkStartTime))

	err = e.createDag()
	if err != nil {
		return err
	}

	return nil
}

func (e *Engine) Simplify() error {
	return e.linkTargets(true, nil)
}

func TargetNotFoundError(target string) error {
	return fmt.Errorf("target %v not found", target)
}

func (e *Engine) createDag() error {
	targets := e.linkedTargets()

	e.dag = &DAG{dag.NewDAG()}

	dagStartTime := time.Now()
	for _, target := range targets.Slice() {
		_, err := e.dag.AddVertex(target)
		if err != nil {
			return err
		}
	}

	addEdge := func(src, dst *Target) error {
		ok, err := e.dag.IsEdge(src.FQN, dst.FQN)
		if ok || err != nil {
			return err
		}

		return e.dag.AddEdge(src.FQN, dst.FQN)
	}

	for _, target := range targets.Slice() {
		for _, dep := range target.Deps.All().Targets {
			err := addEdge(dep.Target, target)
			if err != nil {
				return fmt.Errorf("dep: %v to %v: %w", dep.Target.FQN, target.FQN, err)
			}
		}

		if target.DifferentHashDeps {
			for _, dep := range target.HashDeps.Targets {
				err := addEdge(dep.Target, target)
				if err != nil {
					return fmt.Errorf("hashdep: %v to %v: %w", dep.Target.FQN, target.FQN, err)
				}
			}
		}

		for _, tool := range target.Tools {
			err := addEdge(tool.Target, target)
			if err != nil {
				return fmt.Errorf("tool: %v to %v: %w", tool.Target.FQN, target.FQN, err)
			}
		}
	}
	log.Debugf("DAG took %v", time.Since(dagStartTime))

	return nil
}

func (e *Engine) lockFactory(t *Target, resource string) utils.Locker {
	p := e.lockPath(t, resource)

	return utils.NewFlock(p)
}

func (e *Engine) processTarget(t *Target) error {
	if t.processed {
		panic(fmt.Errorf("%v has already been processed", t.FQN))
	}

	// Validate FQN
	_, err := utils.TargetParse("", t.FQN)
	if err != nil {
		return fmt.Errorf("%v: %w", t.FQN, err)
	}

	t.runLock = e.lockFactory(t, "run")
	t.cacheLock = e.lockFactory(t, "cache")

	if t.Codegen != "" {
		if t.Codegen != "link" {
			return fmt.Errorf("codegen must be omitted or be equal to `link`")
		}

		if !t.Sandbox {
			return fmt.Errorf("codegen is only suported in sandbox mode")
		}

		t.CodegenLink = true

		for _, file := range t.TargetSpec.Out {
			if strings.Contains(file.Path, "*") {
				return fmt.Errorf("codegen must not have glob outputs")
			}

			p := filepath.Join(file.Package.Root.RelRoot, file.Path)

			if ct, ok := e.codegenPaths[p]; ok && ct != t {
				return fmt.Errorf("%v: target %v codegen already outputs %v", t.FQN, ct.FQN, p)
			}

			e.codegenPaths[p] = t
		}
	}

	t.processed = true

	return nil
}

func (e *Engine) linkedTargets() *Targets {
	targets := NewTargets(e.Targets.Len())

	for _, target := range e.Targets.Slice() {
		if !target.linked {
			continue
		}

		targets.Add(target)
	}

	return targets
}

func (e *Engine) noRequireGenTargets() []*Target {
	targets := make([]*Target, 0)

	for _, target := range e.Targets.Slice() {
		if target.RequireGen {
			continue
		}

		targets = append(targets, target)
	}

	return targets
}

func (e *Engine) linkTargets(simplify bool, targets []*Target) error {
	for _, target := range e.Targets.Slice() {
		target.linked = false
	}

	if targets == nil {
		targets = e.Targets.Slice()
	}

	for i, target := range targets {
		log.Tracef("# Linking target %v %v/%v", target.FQN, i+1, len(targets))
		err := e.linkTarget(target, simplify, nil)
		if err != nil {
			return fmt.Errorf("%v: %w", target.FQN, err)
		}
	}

	return nil
}

func (e *Engine) filterOutCodegenFromDeps(t *Target, td TargetDeps) TargetDeps {
	files := make(PackagePaths, 0, len(td.Files))
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

func (e *Engine) linkTarget(t *Target, simplify bool, breadcrumb *Targets) error {
	if !t.processed {
		panic(fmt.Sprintf("%v has not been processed", t.FQN))
	}

	if breadcrumb.Find(t.FQN) != nil {
		breadcrumb.Add(t)
		return fmt.Errorf("linking cycle: %v", breadcrumb.FQNs())
	}
	breadcrumb = breadcrumb.Copy()
	breadcrumb.Add(t)

	logPrefix := strings.Repeat("|", breadcrumb.Len()-1)

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

	t.SandboxRoot = e.sandboxRoot(t).Join("_dir")

	t.WorkdirRoot = Path{
		Root:    e.Root.Abs(),
		RelRoot: "",
	}

	if t.Sandbox {
		t.WorkdirRoot = t.SandboxRoot
	}

	log.Tracef(logPrefix + "Linking tools")

	t.Tools = []TargetTool{}

	type targetTool struct {
		Target *Target
		Output string
	}

	targetTools := make([]targetTool, 0)
	for _, tool := range t.TargetSpec.TargetTools {
		tt := e.Targets.Find(tool.Target)
		if tt == nil {
			return TargetNotFoundError(tool.Target)
		}

		err = e.linkTarget(tt, simplify, breadcrumb)
		if err != nil {
			return fmt.Errorf("tool: %v: %w", tool, err)
		}

		targetTools = append(targetTools, targetTool{
			Target: tt,
			Output: tool.Output,
		})
	}

	for _, tool := range t.TargetSpec.ExprTools {
		expr := tool.Expr

		targets, err := e.targetExpr(t, expr, simplify, breadcrumb)
		if err != nil {
			return err
		}

		for _, target := range targets {
			targetTools = append(targetTools, targetTool{
				Target: target,
			})
		}
	}

	for _, tool := range targetTools {
		tt := tool.Target

		if len(tt.Out.All()) == 0 {
			return fmt.Errorf("%v does not output anything", tt.FQN)
		}

		if tool.Output != "" {
			if !tt.IsNamedOutput() {
				return fmt.Errorf("%v must have named output", tt.FQN)
			}

			file := tt.FindNamedOutput(tool.Output)

			if file == nil {
				return fmt.Errorf("%v does not have any output named %v", tt.FQN, tool.Output)
			}

			t.Tools = append(t.Tools, TargetTool{
				Target:  tt,
				Name:    file.Name,
				RelPath: file.Path,
			})
		} else {
			for _, file := range tt.TargetSpec.Out {
				name := file.Name
				if name == "" {
					if len(tt.TargetSpec.Out) == 1 {
						name = tt.Name
					} else {
						name = filepath.Base(file.Path)
					}
				}
				t.Tools = append(t.Tools, TargetTool{
					Target:  tt,
					Name:    name,
					RelPath: file.Path,
				})
			}
		}
	}

	sort.SliceStable(t.HostTools, func(i, j int) bool {
		return t.HostTools[i].Name < t.HostTools[j].Name
	})

	sort.SliceStable(t.Tools, func(i, j int) bool {
		return t.Tools[i].Name < t.Tools[j].Name
	})

	log.Tracef(logPrefix + "Linking deps")
	t.Deps, err = e.linkTargetNamedDeps(t, t.TargetSpec.Deps, simplify, breadcrumb)
	if err != nil {
		return fmt.Errorf("%v: deps: %w", t.FQN, err)
	}
	t.Deps.Map(func(deps TargetDeps) TargetDeps {
		return e.filterOutCodegenFromDeps(t, deps)
	})

	if t.TargetSpec.DifferentHashDeps {
		log.Tracef(logPrefix + "Linking hashdeps")
		t.HashDeps, err = e.linkTargetDeps(t, t.TargetSpec.HashDeps, simplify, breadcrumb)
		if err != nil {
			return fmt.Errorf("%v: hashdeps: %w", t.FQN, err)
		}
		t.HashDeps = e.filterOutCodegenFromDeps(t, t.HashDeps)
	} else {
		t.HashDeps = t.Deps.All()
	}

	t.Out = &TargetNamedPackagePath{}
	for _, file := range t.TargetSpec.Out {
		t.Out.Add(file.Name, PackagePath{
			Package:     file.Package,
			Path:        file.Path,
			PackagePath: true,
		})
	}

	t.Out.Sort()

	if t.TargetSpec.ShouldCache {
		if !t.TargetSpec.Sandbox && !t.TargetSpec.OutInSandbox {
			return fmt.Errorf("%v cannot cache target which isnt sandboxed", t.FQN)
		}

		if t.TargetSpec.Cache == nil {
			t.CacheFiles = t.Out.All()
		} else {
			t.CacheFiles = []PackagePath{}
			for _, p := range t.TargetSpec.Cache {
				t.CacheFiles = append(t.CacheFiles, PackagePath{
					Package:     t.Package,
					Path:        p,
					PackagePath: true,
				})
			}

			sort.SliceStable(t.CacheFiles, func(i, j int) bool {
				return t.CacheFiles[i].RelRoot() < t.CacheFiles[j].RelRoot()
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

	return nil
}

func (e *Engine) linkTargetNamedDeps(t *Target, deps TargetSpecDeps, simplify bool, breadcrumb *Targets) (TargetNamedDeps, error) {
	m := map[string]TargetSpecDeps{}
	for _, itm := range deps.Targets {
		a := m[itm.Name]
		a.Targets = append(m[itm.Name].Targets, itm)
		m[itm.Name] = a
	}

	for _, itm := range deps.Exprs {
		a := m[itm.Name]
		a.Exprs = append(m[itm.Name].Exprs, itm)
		m[itm.Name] = a
	}

	for _, itm := range deps.Files {
		a := m[itm.Name]
		a.Files = append(m[itm.Name].Files, itm)
		m[itm.Name] = a
	}

	td := TargetNamedDeps{}
	for name, deps := range m {
		ldeps, err := e.linkTargetDeps(t, deps, simplify, breadcrumb)
		if err != nil {
			return TargetNamedDeps{}, err
		}

		td.Set(name, ldeps)
	}

	td.Sort()

	return td, nil
}

func (e *Engine) targetExpr(t *Target, expr exprs.Expr, simplify bool, breadcrumb *Targets) ([]*Target, error) {
	switch expr.Function {
	case "collect":
		targets, err := e.collect(t, expr)
		if err != nil {
			return nil, fmt.Errorf("`%v`: %w", expr.String, err)
		}

		for _, target := range targets {
			err := e.linkTarget(target, simplify, breadcrumb)
			if err != nil {
				return nil, fmt.Errorf("collect: %w", err)
			}
		}

		return targets, nil
	case "find_parent":
		target, err := e.findParent(t, expr)
		if err != nil {
			return nil, fmt.Errorf("`%v`: %w", expr.String, err)
		}

		if target != nil {
			err = e.linkTarget(target, simplify, breadcrumb)
			if err != nil {
				return nil, fmt.Errorf("find_parent: %w", err)
			}

			return []*Target{target}, nil
		}

		return []*Target{}, nil
	default:
		return nil, fmt.Errorf("unhandled function %v", expr.Function)
	}
}

func (e *Engine) linkTargetDeps(t *Target, deps TargetSpecDeps, simplify bool, breadcrumb *Targets) (TargetDeps, error) {
	td := TargetDeps{}

	for _, expr := range deps.Exprs {
		expr := expr.Expr

		targets, err := e.targetExpr(t, expr, simplify, breadcrumb)
		if err != nil {
			return TargetDeps{}, err
		}

		for _, target := range targets {
			td.Targets = append(td.Targets, TargetWithOutput{Target: target})
		}
	}

	for _, target := range deps.Targets {
		dt := e.Targets.Find(target.Target)
		if dt == nil {
			return TargetDeps{}, TargetNotFoundError(target.Target)
		}

		err := e.linkTarget(dt, simplify, breadcrumb)
		if err != nil {
			return TargetDeps{}, err
		}

		td.Targets = append(td.Targets, TargetWithOutput{
			Target: dt,
			Output: target.Output,
		})
	}

	for _, file := range deps.Files {
		td.Files = append(td.Files, PackagePath{
			Package: file.Package,
			Path:    file.Path,
		})
	}

	if simplify {
		targets := make([]TargetWithOutput, 0, len(td.Targets))
		for _, dep := range td.Targets {
			if dep.Target.IsGroup() {
				targets = append(targets, dep.Target.Deps.All().Targets...)
				td.Files = append(td.Files, dep.Target.Deps.All().Files...)
			} else {
				targets = append(targets, dep)
			}
		}
		td.Targets = targets
	}

	td.Targets = utils.DedupKeepLast(td.Targets, func(t TargetWithOutput) string {
		return t.Target.FQN
	})
	td.Files = utils.DedupKeepLast(td.Files, func(t PackagePath) string {
		return t.RelRoot()
	})

	sort.SliceStable(td.Targets, func(i, j int) bool {
		return td.Targets[i].Target.FQN < td.Targets[j].Target.FQN
	})
	sort.SliceStable(td.Files, func(i, j int) bool {
		return td.Files[i].RelRoot() < td.Files[j].RelRoot()
	})

	return td, nil
}
