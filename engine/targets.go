package engine

import (
	"errors"
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
	Target *Target
	Name   string
	File   RelPath
}

func (tt TargetTool) AbsPath() string {
	return tt.File.WithRoot(tt.Target.OutRoot.Abs()).Abs()
}

type TargetDeps struct {
	Targets []TargetWithOutput
	Files   []Path
}

type NamedPaths[TS ~[]T, T RelablePath] struct {
	names []string
	named map[string]TS
	all   TS
	allm  map[string]struct{}
}

func (tp *NamedPaths[TS, T]) Named() map[string]TS {
	return tp.named
}

func (tp *NamedPaths[TS, T]) All() TS {
	return tp.all
}

func (tp *NamedPaths[TS, T]) HasName(name string) bool {
	for _, n := range tp.names {
		if n == name {
			return true
		}
	}

	return false
}

func (tp *NamedPaths[TS, T]) Name(name string) TS {
	if tp.named == nil {
		return nil
	}

	return tp.named[name]
}

func (tp *NamedPaths[TS, T]) Add(name string, p T) {
	if tp.named == nil {
		tp.named = map[string]TS{}
	}

	if _, ok := tp.named[name]; !ok {
		tp.names = append(tp.names, name)
		tp.named[name] = make(TS, 0)
	}
	tp.named[name] = append(tp.named[name], p)

	if tp.allm == nil {
		tp.allm = map[string]struct{}{}
	}

	prr := p.RelRoot()
	if _, ok := tp.allm[prr]; !ok {
		tp.all = append(tp.all, p)
		tp.allm[prr] = struct{}{}
	}
}

func (tp *NamedPaths[TS, T]) Sort() {
	sort.SliceStable(tp.all, func(i, j int) bool {
		return tp.all[i].RelRoot() < tp.all[j].RelRoot()
	})

	for name := range tp.named {
		sort.SliceStable(tp.named[name], func(i, j int) bool {
			return tp.named[name][i].RelRoot() < tp.named[name][j].RelRoot()
		})
	}
}

func (tp NamedPaths[TS, T]) withRoot(paths []T, root string) Paths {
	ps := make(Paths, 0, len(paths))
	for _, path := range paths {
		ps = append(ps, path.WithRoot(root))
	}

	return ps
}

func (tp NamedPaths[TS, T]) WithRoot(root string) *NamedPaths[Paths, Path] {
	ntp := &NamedPaths[Paths, Path]{
		names: tp.names,
		named: map[string]Paths{},
		all:   tp.withRoot(tp.all, root),
	}

	for name, paths := range tp.named {
		ntp.named[name] = tp.withRoot(paths, root)
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
			return deps.Files[i].RelRoot() < deps.Files[j].RelRoot()
		})

		return deps
	})
}

type OutNamedPaths = NamedPaths[RelPaths, RelPath]
type ActualOutNamedPaths = NamedPaths[Paths, Path]

type Target struct {
	TargetSpec

	Tools          []TargetTool
	Deps           TargetNamedDeps
	HashDeps       TargetDeps
	Out            *OutNamedPaths
	actualFilesOut *ActualOutNamedPaths
	Env            map[string]string
	Transitive     TargetTransitive

	CodegenLink bool

	WorkdirRoot       Path
	SandboxRoot       Path
	OutRoot           *Path
	CacheFiles        RelPaths
	actualcachedFiles Paths
	LogFile           string

	processed  bool
	linked     bool
	deeplinked bool
	linking    bool
	linkingCh  chan struct{}
	linkingErr error
	deps       *Targets // keep track of all target deps for linking
	m          sync.Mutex

	runLock   utils.Locker
	cacheLock utils.Locker
}

type TargetTransitive struct {
	Tools []TargetTool
	Deps  TargetNamedDeps
}

var ErrStopWalk = errors.New("stop walk")

func (t *Target) transitivelyWalk(m map[string]struct{}, f func(t *Target) error) error {
	targets := append([]*Target{t}, t.deps.Slice()...)

	for _, t := range targets {
		if _, ok := m[t.FQN]; ok {
			continue
		}
		m[t.FQN] = struct{}{}

		err := f(t)
		if err != nil {
			return err
		}

		err = t.transitivelyWalk(m, f)
		if err != nil {
			return err
		}
	}

	return nil
}

func (t *Target) TransitivelyWalk(f func(t *Target) error) error {
	m := map[string]struct{}{}

	err := t.transitivelyWalk(m, f)
	if err != nil && !errors.Is(err, ErrStopWalk) {
		return err
	}

	return nil
}

func (t *Target) resetLinking() {
	t.deeplinked = false

	spec := t.TargetSpec

	if t.linkingErr != nil || len(spec.Deps.Exprs) > 0 || len(spec.HashDeps.Exprs) > 0 || len(spec.Tools.Exprs) > 0 {
		depsCap := 0
		if t.deps != nil {
			depsCap = len(t.deps.Slice())
		}
		t.deps = NewTargets(depsCap)
		t.linked = false
		t.linkingErr = nil
	}
}

func (t *Target) ID() string {
	return t.FQN
}

func (t *Target) ActualFilesOut() Paths {
	if t.actualFilesOut == nil {
		panic("actualFilesOut is nil for " + t.FQN)
	}

	return t.actualFilesOut.all
}

func (t *Target) NamedActualFilesOut() *NamedPaths[Paths, Path] {
	if t.actualFilesOut == nil {
		panic("actualFilesOut is nil for " + t.FQN)
	}

	return t.actualFilesOut
}

func (t *Target) String() string {
	return t.FQN
}

func Contains(ts []*Target, fqn string) bool {
	for _, t := range ts {
		if t.FQN == fqn {
			return true
		}
	}

	return false
}

func (t *Target) OutFilesInOutRoot() Paths {
	out := make(Paths, 0)
	for _, file := range t.Out.All() {
		out = append(out, file.WithRoot(t.OutRoot.Abs()))
	}

	return out
}

func (t *Target) ActualCachedFiles() Paths {
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

func (e *Engine) Init() error {
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

	return nil
}

func (e *Engine) Parse() error {
	for name, cfg := range e.Config.BuildFiles.Roots {
		err := e.runRootBuildFiles(name, cfg)
		if err != nil {
			return fmt.Errorf("root %v: %w", name, err)
		}
	}

	runStartTime := time.Now()
	err := e.runBuildFiles(e.Root.Abs(), func(dir string) *Package {
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
	err = e.linkTargets(true, nil)
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

type targetNotFoundError struct {
	target string
}

func (e targetNotFoundError) Error() string {
	return fmt.Sprintf("target %v not found", e.target)
}

func (e targetNotFoundError) Is(err error) bool {
	_, ok := err.(targetNotFoundError)
	return ok
}

func TargetNotFoundError(target string) error {
	return targetNotFoundError{
		target: target,
	}
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

			p := file.Package.Root.Join(file.Path).RelRoot()

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

func (e *Engine) linkTargets(ignoreNotFoundError bool, targets []*Target) error {
	for _, target := range e.Targets.Slice() {
		target.resetLinking()
	}

	if targets == nil {
		targets = e.Targets.Slice()
	}

	for i, target := range targets {
		log.Tracef("# Linking target %v %v/%v", target.FQN, i+1, len(targets))
		err := e.linkTarget(target, nil)
		if err != nil {
			if !ignoreNotFoundError || (ignoreNotFoundError && !errors.Is(err, targetNotFoundError{})) {
				return fmt.Errorf("%v: %w", target.FQN, err)
			}
		}
	}

	return nil
}

func (e *Engine) filterOutCodegenFromDeps(t *Target, td TargetDeps) TargetDeps {
	files := make(Paths, 0, len(td.Files))
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

func (e *Engine) linkTarget(t *Target, breadcrumb *Targets) (rerr error) {
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
	if t.deps == nil {
		t.deps = NewTargets(0)
	}

	if t.deeplinked {
		t.m.Unlock()
		return nil
	} else if t.linked {
		t.m.Unlock()

		for _, dep := range t.deps.Slice() {
			err := e.linkTarget(dep, breadcrumb)
			if err != nil {
				t.linkingErr = err
				t.linked = false
				return err
			}
		}

		t.deeplinked = true

		return nil
	} else if t.linkingErr != nil {
		t.m.Unlock()
		return t.linkingErr
	} else if t.linking {
		t.m.Unlock()
		<-t.linkingCh
		return t.linkingErr
	}
	t.linking = true
	t.linkingCh = make(chan struct{})
	t.m.Unlock()

	defer func() {
		t.linkingErr = rerr
		close(t.linkingCh)
		t.linking = false
		if rerr == nil {
			t.linked = true
			t.deeplinked = true
		}
	}()

	var err error

	log.Tracef(logPrefix+"Linking %v", t.FQN)
	defer func() {
		log.Tracef(logPrefix+"Linking %v done", t.FQN)
	}()

	t.SandboxRoot = e.sandboxRoot(t).Join("_dir")

	t.WorkdirRoot = e.Root

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
	for _, tool := range t.TargetSpec.Tools.Targets {
		tt := e.Targets.Find(tool.Target)
		if tt == nil {
			return TargetNotFoundError(tool.Target)
		}

		err = e.linkTarget(tt, breadcrumb)
		if err != nil {
			return fmt.Errorf("tool: %v: %w", tool, err)
		}

		targetTools = append(targetTools, targetTool{
			Target: tt,
			Output: tool.Output,
		})
	}

	for _, tool := range t.TargetSpec.Tools.Exprs {
		expr := tool.Expr

		targets, err := e.targetExpr(t, expr, breadcrumb)
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

		var paths map[string]RelPaths
		if tool.Output != "" {
			npaths := tt.Out.Name(tool.Output)

			if len(npaths) == 0 {
				return fmt.Errorf("%v|%v has no output", tt.FQN, tool.Output)
			}

			paths = map[string]RelPaths{
				tool.Output: npaths,
			}
		} else {
			paths = tt.Out.Named()

			if len(paths) == 0 {
				return fmt.Errorf("%v has no output", tt.FQN)
			}
		}

		for name, paths := range paths {
			if len(paths) > 1 {
				return fmt.Errorf("%v: each named output can only output one file to be used as a tool", tt.FQN)
			}

			path := paths[0]

			if name == "" {
				name = filepath.Base(path.RelRoot())
			}

			t.Tools = append(t.Tools, TargetTool{
				Target: tt,
				Name:   name,
				File:   path,
			})
		}
	}

	sort.SliceStable(t.TargetSpec.Tools.Hosts, func(i, j int) bool {
		ts := t.TargetSpec.Tools.Hosts
		return ts[i].Name < ts[j].Name
	})

	sort.SliceStable(t.Tools, func(i, j int) bool {
		return t.Tools[i].Name < t.Tools[j].Name
	})

	log.Tracef(logPrefix + "Linking deps")
	t.Deps, err = e.linkTargetNamedDeps(t, t.TargetSpec.Deps, breadcrumb)
	if err != nil {
		return fmt.Errorf("%v: deps: %w", t.FQN, err)
	}
	t.Deps.Map(func(deps TargetDeps) TargetDeps {
		return e.filterOutCodegenFromDeps(t, deps)
	})

	if t.TargetSpec.DifferentHashDeps {
		log.Tracef(logPrefix + "Linking hashdeps")
		t.HashDeps, err = e.linkTargetDeps(t, t.TargetSpec.HashDeps, breadcrumb)
		if err != nil {
			return fmt.Errorf("%v: hashdeps: %w", t.FQN, err)
		}
		t.HashDeps = e.filterOutCodegenFromDeps(t, t.HashDeps)
	} else {
		t.HashDeps = t.Deps.All()
	}

	relPathFactory := func(p string) RelPath {
		abs := strings.HasPrefix(p, "/")

		var relRoot string
		if abs {
			relRoot = strings.TrimPrefix(p, "/")
		} else {
			relRoot = filepath.Join(t.Package.FullName, p)
		}

		return RelPath{
			relRoot: relRoot,
		}
	}

	t.Out = &OutNamedPaths{}
	for _, file := range t.TargetSpec.Out {
		t.Out.Add(file.Name, relPathFactory(file.Path))
	}

	t.Out.Sort()

	if t.TargetSpec.Cache.Enabled {
		if !t.TargetSpec.Sandbox && !t.TargetSpec.OutInSandbox {
			return fmt.Errorf("%v cannot cache target which isnt sandboxed", t.FQN)
		}

		if t.TargetSpec.Cache.Files == nil {
			t.CacheFiles = t.Out.All()
		} else {
			t.CacheFiles = RelPaths{}
			for _, p := range t.TargetSpec.Cache.Files {
				t.CacheFiles = append(t.CacheFiles, relPathFactory(p))
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

	for _, dep := range t.Deps.All().Targets {
		t.deps.Add(dep.Target)
	}

	if t.DifferentHashDeps {
		for _, dep := range t.HashDeps.Targets {
			t.deps.Add(dep.Target)
		}
	}

	for _, dep := range t.Tools {
		t.deps.Add(dep.Target)
	}

	t.deps.Sort()

	return nil
}

func (e *Engine) linkTargetNamedDeps(t *Target, deps TargetSpecDeps, breadcrumb *Targets) (TargetNamedDeps, error) {
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
		ldeps, err := e.linkTargetDeps(t, deps, breadcrumb)
		if err != nil {
			return TargetNamedDeps{}, err
		}

		td.Set(name, ldeps)
	}

	td.Sort()

	return td, nil
}

func (e *Engine) targetExpr(t *Target, expr exprs.Expr, breadcrumb *Targets) ([]*Target, error) {
	switch expr.Function {
	case "collect":
		targets, err := e.collect(t, expr)
		if err != nil {
			return nil, fmt.Errorf("`%v`: %w", expr.String, err)
		}

		for _, target := range targets {
			err := e.linkTarget(target, breadcrumb)
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
			err = e.linkTarget(target, breadcrumb)
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

const InlineGroups = true

func (e *Engine) linkTargetDeps(t *Target, deps TargetSpecDeps, breadcrumb *Targets) (TargetDeps, error) {
	td := TargetDeps{}

	for _, expr := range deps.Exprs {
		expr := expr.Expr

		targets, err := e.targetExpr(t, expr, breadcrumb)
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

		err := e.linkTarget(dt, breadcrumb)
		if err != nil {
			return TargetDeps{}, err
		}

		td.Targets = append(td.Targets, TargetWithOutput{
			Target: dt,
			Output: target.Output,
		})
	}

	for _, file := range deps.Files {
		td.Files = append(td.Files, Path{
			root:    t.Package.Root.root,
			relRoot: filepath.Join(t.Package.FullName, file.Path),
			abs:     t.Package.Root.Join(file.Path).Abs(),
		})
	}

	if InlineGroups {
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
	td.Files = utils.DedupKeepLast(td.Files, func(t Path) string {
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
