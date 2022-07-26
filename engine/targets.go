package engine

import (
	"errors"
	"fmt"
	"github.com/heimdalr/dag"
	log "github.com/sirupsen/logrus"
	"heph/exprs"
	"heph/packages"
	"heph/targetspec"
	"heph/upgrade"
	"heph/utils"
	"heph/utils/flock"
	"heph/utils/fs"
	"heph/utils/sets"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"time"
)

type TargetTools struct {
	// Holds targets references that do not have output (for transitive for ex)
	TargetReferences []*Target
	Targets          []TargetTool
	Hosts            []targetspec.TargetSpecHostTool
}

func (t TargetTools) Merge(tools TargetTools) TargetTools {
	return TargetTools{
		TargetReferences: append(t.TargetReferences, tools.TargetReferences...),
		Targets:          append(t.Targets, tools.Targets...),
		Hosts:            append(t.Hosts, tools.Hosts...),
	}
}

func (t TargetTools) Empty() bool {
	return len(t.Targets) == 0 && len(t.Hosts) == 0
}

func (t TargetTools) Sort() {
	sort.SliceStable(t.Hosts, func(i, j int) bool {
		return t.Hosts[i].Name < t.Hosts[j].Name
	})

	sort.SliceStable(t.Targets, func(i, j int) bool {
		return t.Targets[i].Name < t.Targets[j].Name
	})

	sort.SliceStable(t.TargetReferences, func(i, j int) bool {
		return t.TargetReferences[i].Name < t.TargetReferences[j].Name
	})
}

type TargetTool struct {
	Target *Target
	Output string
	Name   string
	File   fs.RelPath
}

func (tt TargetTool) AbsPath() string {
	return tt.File.WithRoot(tt.Target.OutExpansionRoot.Abs()).Abs()
}

type TargetDeps struct {
	Targets []TargetWithOutput
	Files   []fs.Path
}

func (d TargetDeps) Merge(deps TargetDeps) TargetDeps {
	nd := TargetDeps{}
	nd.Targets = append(d.Targets, deps.Targets...)
	nd.Files = append(d.Files, deps.Files...)

	return nd
}

func (d TargetDeps) Sort() {
	sort.SliceStable(d.Targets, func(i, j int) bool {
		if d.Targets[i].Target.FQN == d.Targets[j].Target.FQN {
			return d.Targets[i].Output < d.Targets[j].Output
		}

		return d.Targets[i].Target.FQN < d.Targets[j].Target.FQN
	})
	sort.SliceStable(d.Files, func(i, j int) bool {
		return d.Files[i].RelRoot() < d.Files[j].RelRoot()
	})
}

type NamedPaths[TS ~[]T, T fs.RelablePath] struct {
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

func (tp *NamedPaths[TS, T]) Names() []string {
	return tp.names
}

func (tp *NamedPaths[TS, T]) ProvisonName(name string) {
	if tp.named == nil {
		tp.named = map[string]TS{}
	}

	if _, ok := tp.named[name]; !ok {
		tp.names = append(tp.names, name)
		tp.named[name] = make(TS, 0)
	}
}

func (tp *NamedPaths[TS, T]) AddAll(name string, ps []T) {
	tp.ProvisonName(name)
	for _, p := range ps {
		tp.Add(name, p)
	}
}

func (tp *NamedPaths[TS, T]) Add(name string, p T) {
	tp.ProvisonName(name)
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
		named := tp.named[name]

		sort.SliceStable(tp.named[name], func(i, j int) bool {
			return named[i].RelRoot() < named[j].RelRoot()
		})
	}
}

func (tp NamedPaths[TS, T]) withRoot(paths []T, root string) fs.Paths {
	ps := make(fs.Paths, 0, len(paths))
	for _, path := range paths {
		ps = append(ps, path.WithRoot(root))
	}

	return ps
}

func (tp NamedPaths[TS, T]) WithRoot(root string) *NamedPaths[fs.Paths, fs.Path] {
	ntp := &NamedPaths[fs.Paths, fs.Path]{
		names: tp.names,
		named: map[string]fs.Paths{},
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
			if deps.Targets[i].Target.FQN == deps.Targets[j].Target.FQN {
				return deps.Targets[i].Output < deps.Targets[j].Output
			}

			return deps.Targets[i].Target.FQN < deps.Targets[j].Target.FQN
		})

		sort.SliceStable(deps.Files, func(i, j int) bool {
			return deps.Files[i].RelRoot() < deps.Files[j].RelRoot()
		})

		return deps
	})
}

func (tp *TargetNamedDeps) Merge(deps TargetNamedDeps) TargetNamedDeps {
	ntp := TargetNamedDeps{}
	for name, deps := range tp.Named() {
		ntp.Set(name, deps)
	}

	for name, deps := range deps.Named() {
		ntp.Set(name, ntp.Name(name).Merge(deps))
	}

	return ntp
}

func (tp *TargetNamedDeps) Empty() bool {
	return len(tp.All().Targets) == 0 && len(tp.All().Files) == 0
}

type OutNamedPaths = NamedPaths[fs.RelPaths, fs.RelPath]
type ActualOutNamedPaths = NamedPaths[fs.Paths, fs.Path]

type Target struct {
	targetspec.TargetSpec

	Tools          TargetTools
	Deps           TargetNamedDeps
	HashDeps       TargetDeps
	OutWithSupport *OutNamedPaths
	Out            *OutNamedPaths
	actualOutFiles *ActualOutNamedPaths
	Env            map[string]string
	RuntimeEnv     map[string]TargetRuntimeEnv
	Transitive     TargetTransitive

	RequireTransitive     TargetTransitive
	DeepRequireTransitive TargetTransitive

	WorkdirRoot        fs.Path
	SandboxRoot        fs.Path
	actualSupportFiles fs.Paths
	LogFile            string
	OutExpansionRoot   *fs.Path

	processed  bool
	linked     bool
	deeplinked bool
	linking    bool
	linkingCh  chan struct{}
	linkingErr error
	// Deps + HashDeps + TargetTools
	linkingDeps *Targets
	m           sync.Mutex

	runLock         flock.Locker
	postRunWarmLock flock.Locker
	cacheLocks      map[string]flock.Locker
}

type TargetRuntimeEnv struct {
	Value  string
	Target *Target
}

type TargetTransitive struct {
	Tools      TargetTools
	Deps       TargetNamedDeps
	Env        map[string]string
	RuntimeEnv map[string]TargetRuntimeEnv
	PassEnv    []string
}

func (tr TargetTransitive) Merge(otr TargetTransitive) TargetTransitive {
	ntr := TargetTransitive{
		Env:        map[string]string{},
		RuntimeEnv: map[string]TargetRuntimeEnv{},
	}

	if otr.Tools.Empty() {
		ntr.Tools = tr.Tools
	} else {
		ntr.Tools = tr.Tools.Merge(otr.Tools)
	}

	if otr.Deps.Empty() {
		ntr.Deps = tr.Deps
	} else {
		ntr.Deps = tr.Deps.Merge(otr.Deps)
	}

	for k, v := range tr.Env {
		ntr.Env[k] = v
	}
	for k, v := range otr.Env {
		ntr.Env[k] = v
	}

	for k, v := range tr.RuntimeEnv {
		ntr.RuntimeEnv[k] = v
	}
	for k, v := range otr.RuntimeEnv {
		ntr.RuntimeEnv[k] = v
	}

	ntr.PassEnv = append(tr.PassEnv, otr.PassEnv...)

	return ntr
}

func (tr TargetTransitive) Empty() bool {
	return tr.Tools.Empty() && tr.Deps.Empty() && len(tr.Env) == 0 && len(tr.RuntimeEnv) == 0 && len(tr.PassEnv) == 0
}

var ErrStopWalk = errors.New("stop walk")

func (t *Target) transitivelyWalk(m map[string]struct{}, f func(t *Target) error) error {
	targets := append([]*Target{t}, t.linkingDeps.Slice()...)

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
		if t.linkingDeps != nil {
			depsCap = len(t.linkingDeps.Slice())
		}
		t.linkingDeps = NewTargets(depsCap)
		t.linked = false
		t.linkingErr = nil
	}
}

func (t *Target) ID() string {
	return t.FQN
}

func (t *Target) ActualOutFiles() *ActualOutNamedPaths {
	if t.actualOutFiles == nil {
		panic("actualOutFiles is nil for " + t.FQN)
	}

	return t.actualOutFiles
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

func (t *Target) ActualSupportFiles() fs.Paths {
	if t.actualSupportFiles == nil {
		panic("actualSupportFiles is nil for " + t.FQN)
	}

	return t.actualSupportFiles
}

func (t *Target) IsPrivate() bool {
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
	Target     *Target
	Output     string
	SpecOutput string
}

func (t TargetWithOutput) Full() string {
	if t.Output == "" {
		return t.Target.FQN
	}

	return t.Target.FQN + "|" + t.Output
}

type Targets struct {
	*sets.Set[string, *Target]
}

func NewTargets(cap int) *Targets {
	s := sets.NewSet(func(t *Target) string {
		if t == nil {
			panic("target must not be nil")
		}

		return t.FQN
	}, cap)

	return &Targets{
		Set: s,
	}
}

func (ts *Targets) FQNs() []string {
	if ts == nil {
		return nil
	}

	fqns := make([]string, 0)
	for _, target := range ts.Slice() {
		fqns = append(fqns, target.FQN)
	}

	return fqns
}

func (ts *Targets) Sort() {
	if ts == nil {
		return
	}

	a := ts.Slice()

	sort.SliceStable(a, func(i, j int) bool {
		return a[i].FQN < a[j].FQN
	})
}

func (ts *Targets) Copy() *Targets {
	if ts == nil {
		return NewTargets(0)
	}

	return &Targets{
		Set: ts.Set.Copy(),
	}
}

func (ts *Targets) Find(fqn string) *Target {
	if ts == nil {
		return nil
	}

	if !ts.HasKey(fqn) {
		return nil
	}

	for _, target := range ts.Slice() {
		if target.FQN == fqn {
			return target
		}
	}

	panic("should not happen")
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
	err := e.runBuildFiles(e.Root.Abs(), func(dir string) *packages.Package {
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

	e.autocompleteHash, err = e.computeAutocompleteHash()
	if err != nil {
		return err
	}

	return nil
}

type TargetNotFoundErr struct {
	String string
}

func (e TargetNotFoundErr) Error() string {
	return fmt.Sprintf("target %v not found", e.String)
}

func (e TargetNotFoundErr) Is(err error) bool {
	_, ok := err.(TargetNotFoundErr)
	return ok
}

func NewTargetNotFoundError(target string) error {
	return TargetNotFoundErr{
		String: target,
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

		for _, tool := range target.Tools.Targets {
			err := addEdge(tool.Target, target)
			if err != nil {
				return fmt.Errorf("tool: %v to %v: %w", tool.Target.FQN, target.FQN, err)
			}
		}
	}
	log.Debugf("DAG took %v", time.Since(dagStartTime))

	return nil
}

func (e *Engine) lockFactory(t *Target, resource string) flock.Locker {
	p := e.lockPath(t, resource)

	return flock.NewFlock(p)
}

func (e *Engine) processTarget(t *Target) error {
	if t.processed {
		panic(fmt.Errorf("%v has already been processed", t.FQN))
	}

	// Validate FQN
	_, err := targetspec.TargetParse("", t.FQN)
	if err != nil {
		return fmt.Errorf("%v: %w", t.FQN, err)
	}

	if t.Cache.Enabled {
		t.runLock = e.lockFactory(t, "run")
	} else {
		t.runLock = flock.NewMutex(t.FQN)
	}
	t.postRunWarmLock = e.lockFactory(t, "postrunwarm")
	t.cacheLocks = map[string]flock.Locker{}
	for _, o := range t.TargetSpec.Out {
		t.cacheLocks[o.Name] = e.lockFactory(t, "cache_"+o.Name)
	}
	t.cacheLocks[inputHashName] = e.lockFactory(t, "cache_"+inputHashName)

	if t.Cache.History == 0 {
		t.Cache.History = e.Config.CacheHistory
	}

	if t.Codegen != "" {
		for _, file := range t.TargetSpec.Out {
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

	for _, target := range targets {
		//log.Tracef("# Linking target %v %v/%v", target.FQN, i+1, len(targets))
		err := e.linkTarget(target, nil)
		if err != nil {
			if !ignoreNotFoundError || (ignoreNotFoundError && !errors.Is(err, TargetNotFoundErr{})) {
				return fmt.Errorf("%v: %w", target.FQN, err)
			}
		}
	}

	return nil
}

func (e *Engine) filterOutCodegenFromDeps(t *Target, td TargetDeps) TargetDeps {
	files := make(fs.Paths, 0, len(td.Files))
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

	//logPrefix := strings.Repeat("|", breadcrumb.Len()-1)

	t.m.Lock()
	if t.linkingDeps == nil {
		t.linkingDeps = NewTargets(0)
	}

	if t.deeplinked {
		t.m.Unlock()
		return nil
	} else if t.linked {
		t.m.Unlock()

		for _, dep := range t.linkingDeps.Slice() {
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

	//log.Tracef(logPrefix+"Linking %v", t.FQN)
	//defer func() {
	//	log.Tracef(logPrefix+"Linking %v done", t.FQN)
	//}()

	t.SandboxRoot = e.sandboxRoot(t).Join("_dir")

	t.WorkdirRoot = e.Root

	if t.Sandbox {
		t.WorkdirRoot = t.SandboxRoot
	}

	//log.Tracef(logPrefix + "Linking tools")

	t.Tools, err = e.linkTargetTools(t, t.TargetSpec.Tools, breadcrumb)
	if err != nil {
		return err
	}

	//log.Tracef(logPrefix + "Linking deps")
	t.Deps, err = e.linkTargetNamedDeps(t, t.TargetSpec.Deps, breadcrumb)
	if err != nil {
		return fmt.Errorf("%v: deps: %w", t.FQN, err)
	}

	// Resolve hash deps specs
	if t.TargetSpec.DifferentHashDeps {
		//log.Tracef(logPrefix + "Linking hashdeps")
		t.HashDeps, err = e.linkTargetDeps(t, t.TargetSpec.HashDeps, breadcrumb)
		if err != nil {
			return fmt.Errorf("%v: hashdeps: %w", t.FQN, err)
		}
		t.HashDeps = e.filterOutCodegenFromDeps(t, t.HashDeps)
	} else {
		t.HashDeps = t.Deps.All()
	}

	// Resolve transitive specs
	t.RequireTransitive = TargetTransitive{}
	t.RequireTransitive.Tools, err = e.linkTargetTools(t, t.TargetSpec.Transitive.Tools, breadcrumb)
	if err != nil {
		return err
	}
	t.RequireTransitive.Deps, err = e.linkTargetNamedDeps(t, t.TargetSpec.Transitive.Deps, breadcrumb)
	if err != nil {
		return err
	}
	t.RequireTransitive.Env = t.TargetSpec.Transitive.Env
	t.RequireTransitive.RuntimeEnv = map[string]TargetRuntimeEnv{}
	for k, v := range t.TargetSpec.Transitive.RuntimeEnv {
		t.RequireTransitive.RuntimeEnv[k] = TargetRuntimeEnv{
			Value:  v,
			Target: t,
		}
	}
	t.RequireTransitive.PassEnv = t.TargetSpec.Transitive.PassEnv

	relPathFactory := func(p string) fs.RelPath {
		abs := strings.HasPrefix(p, "/")

		var relRoot string
		if abs {
			relRoot = strings.TrimPrefix(p, "/")
		} else {
			relRoot = filepath.Join(t.Package.FullName, p)
		}

		return fs.NewRelPath(relRoot)
	}

	t.Out = &OutNamedPaths{}
	t.OutWithSupport = &OutNamedPaths{}
	for _, file := range t.TargetSpec.Out {
		if file.Name != targetspec.SupportFilesOutput {
			t.Out.Add(file.Name, relPathFactory(file.Path))
		}
		t.OutWithSupport.Add(file.Name, relPathFactory(file.Path))
	}
	t.Out.Sort()
	t.OutWithSupport.Sort()

	if t.TargetSpec.Cache.Enabled {
		if !t.TargetSpec.Sandbox && !t.TargetSpec.OutInSandbox {
			return fmt.Errorf("%v cannot cache target which isnt sandboxed", t.FQN)
		}
	}

	t.Env = map[string]string{}
	e.applyEnv(t, t.TargetSpec.PassEnv, t.TargetSpec.Env)

	t.DeepRequireTransitive, err = e.computeDeepTransitive(t, breadcrumb)
	if err != nil {
		return err
	}

	// Apply transitive deps
	//log.Tracef(logPrefix + "Linking transitive")
	t.Transitive, err = e.collectDepsTransitive(t, breadcrumb)
	if err != nil {
		return err
	}

	if !t.Transitive.Tools.Empty() {
		t.Tools = t.Tools.Merge(t.Transitive.Tools)
		t.Tools.Sort()
	}

	if !t.Transitive.Deps.Empty() {
		t.Deps = t.Deps.Merge(t.Transitive.Deps)
		t.Deps.Sort()

		if t.DifferentHashDeps {
			t.HashDeps = t.HashDeps.Merge(t.Transitive.Deps.All())
			t.HashDeps.Sort()
		} else {
			t.HashDeps = t.Deps.All()
		}
	}

	e.applyEnv(t, t.Transitive.PassEnv, t.Transitive.Env)
	if t.RuntimeEnv == nil {
		t.RuntimeEnv = map[string]TargetRuntimeEnv{}
	}
	for k, v := range t.Transitive.RuntimeEnv {
		t.RuntimeEnv[k] = v
	}

	e.registerLabels(t.Labels)

	for _, dep := range t.Deps.All().Targets {
		t.linkingDeps.Add(dep.Target)
	}

	if t.DifferentHashDeps {
		for _, dep := range t.HashDeps.Targets {
			t.linkingDeps.Add(dep.Target)
		}
	}

	for _, dep := range t.Tools.Targets {
		t.linkingDeps.Add(dep.Target)
	}

	t.linkingDeps.Sort()

	return nil
}

func (e *Engine) linkTargetNamedDeps(t *Target, deps targetspec.TargetSpecDeps, breadcrumb *Targets) (TargetNamedDeps, error) {
	m := map[string]targetspec.TargetSpecDeps{}
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

	td.Map(func(deps TargetDeps) TargetDeps {
		return e.filterOutCodegenFromDeps(t, deps)
	})

	td.Sort()

	return td, nil
}

func (e *Engine) linkTargetTools(t *Target, toolsSpecs targetspec.TargetSpecTools, breadcrumb *Targets) (TargetTools, error) {
	type targetTool struct {
		Target *Target
		Output string
		Name   string
	}

	refs := make([]*Target, 0, len(toolsSpecs.Targets))
	targetTools := make([]targetTool, 0)
	for _, tool := range toolsSpecs.Targets {
		tt := e.Targets.Find(tool.Target)
		if tt == nil {
			return TargetTools{}, NewTargetNotFoundError(tool.Target)
		}

		err := e.linkTarget(tt, breadcrumb)
		if err != nil {
			return TargetTools{}, fmt.Errorf("tool: %v: %w", tool, err)
		}

		refs = append(refs, tt)

		if tool.Output == "" {
			for _, name := range tt.Out.Names() {
				targetTools = append(targetTools, targetTool{
					Target: tt,
					Output: name,
					Name:   tool.Name,
				})
			}
		} else {
			targetTools = append(targetTools, targetTool{
				Target: tt,
				Output: tool.Output,
				Name:   tool.Name,
			})
		}
	}

	for _, tool := range toolsSpecs.Exprs {
		expr := tool.Expr

		targets, err := e.targetExpr(t, expr, breadcrumb)
		if err != nil {
			return TargetTools{}, err
		}

		for _, target := range targets {
			targetTools = append(targetTools, targetTool{
				Target: target,
				Name:   tool.Name,
			})
			refs = append(refs, target)
		}
	}

	tools := make([]TargetTool, 0, len(toolsSpecs.Targets))

	for _, tool := range targetTools {
		tt := tool.Target

		var name string
		if tool.Name != "" {
			name = tool.Name
		} else {
			name = tool.Output
		}

		var paths map[string]fs.RelPaths
		if tool.Output != "" {
			npaths := tt.Out.Name(tool.Output)

			if len(npaths) == 0 {
				return TargetTools{}, fmt.Errorf("%v|%v has no output", tt.FQN, tool.Output)
			}

			paths = map[string]fs.RelPaths{
				name: npaths,
			}
		} else {
			paths = tt.Out.Named()

			if len(paths) == 0 && tool.Target.DeepRequireTransitive.Empty() {
				return TargetTools{}, fmt.Errorf("%v has no output", tt.FQN)
			}

			if name != "" {
				npaths := map[string]fs.RelPaths{}
				for k, v := range paths {
					nk := name
					if k != "" {
						nk = name + "_" + k
					}
					npaths[nk] = v
				}
				paths = npaths
			}
		}

		for name, paths := range paths {
			if len(paths) != 1 {
				return TargetTools{}, fmt.Errorf("%v: each named output can only output one file to be used as a tool", tt.FQN)
			}

			path := paths[0]

			if name == "" {
				name = filepath.Base(path.RelRoot())
			}

			tools = append(tools, TargetTool{
				Target: tt,
				Output: tool.Output,
				Name:   name,
				File:   path,
			})
		}
	}

	tt := TargetTools{
		TargetReferences: refs,
		Targets:          tools,
		Hosts:            toolsSpecs.Hosts,
	}

	tt.Sort()

	return tt, nil
}

var allEnv = map[string]string{}

func init() {
	for _, e := range os.Environ() {
		parts := strings.SplitN(e, "=", 2)
		allEnv[parts[0]] = parts[1]
	}
}

func (e *Engine) applyEnv(t *Target, passEnv []string, env map[string]string) {
	if t.Env == nil {
		t.Env = map[string]string{}
	}

	for _, name := range passEnv {
		if name == "*" {
			for k, v := range allEnv {
				t.Env[k] = v
			}
			break
		}

		value, ok := os.LookupEnv(name)
		if !ok {
			continue
		}
		t.Env[name] = value
	}
	for k, v := range env {
		t.Env[k] = v
	}
}

func (e *Engine) computeDeepTransitive(t *Target, breadcrumb *Targets) (TargetTransitive, error) {
	tr := t.RequireTransitive
	targets := make([]*Target, 0, len(tr.Deps.All().Targets)+len(tr.Tools.Targets))
	for _, dep := range tr.Deps.All().Targets {
		targets = append(targets, dep.Target)
	}
	for _, dep := range tr.Tools.Targets {
		targets = append(targets, dep.Target)
	}

	dtr, err := e.collectTransitive(targets, breadcrumb)
	if err != nil {
		return TargetTransitive{}, err
	}
	dtr = dtr.Merge(tr)

	return dtr, nil
}

func (e *Engine) collectDepsTransitive(t *Target, breadcrumb *Targets) (TargetTransitive, error) {
	targets := make([]*Target, 0, len(t.Deps.All().Targets)+len(t.Tools.TargetReferences))
	for _, dep := range t.Deps.All().Targets {
		targets = append(targets, dep.Target)
	}
	for _, t := range t.Tools.TargetReferences {
		targets = append(targets, t)
	}

	return e.collectTransitive(targets, breadcrumb)
}

func (e *Engine) collectTransitive(deps []*Target, breadcrumb *Targets) (TargetTransitive, error) {
	tt := TargetTransitive{}

	for _, dep := range deps {
		tt = tt.Merge(dep.DeepRequireTransitive)
	}

	for _, dep := range tt.Deps.All().Targets {
		err := e.linkTarget(dep.Target, breadcrumb)
		if err != nil {
			return TargetTransitive{}, err
		}
	}

	for _, t := range tt.Tools.TargetReferences {
		err := e.linkTarget(t, breadcrumb)
		if err != nil {
			return TargetTransitive{}, err
		}
	}

	return tt, nil
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

func (e *Engine) linkTargetDeps(t *Target, deps targetspec.TargetSpecDeps, breadcrumb *Targets) (TargetDeps, error) {
	td := TargetDeps{}

	for _, expr := range deps.Exprs {
		expr := expr.Expr

		targets, err := e.targetExpr(t, expr, breadcrumb)
		if err != nil {
			return TargetDeps{}, err
		}

		for _, target := range targets {
			if len(target.Out.Names()) == 0 {
				td.Targets = append(td.Targets, TargetWithOutput{
					Target: target,
				})
			} else {
				for _, name := range target.Out.Names() {
					td.Targets = append(td.Targets, TargetWithOutput{
						Target: target,
						Output: name,
					})
				}
			}
		}
	}

	for _, spec := range deps.Targets {
		dt := e.Targets.Find(spec.Target)
		if dt == nil {
			return TargetDeps{}, NewTargetNotFoundError(spec.Target)
		}

		err := e.linkTarget(dt, breadcrumb)
		if err != nil {
			return TargetDeps{}, err
		}

		if spec.Output == "" {
			if len(dt.Out.Names()) == 0 {
				td.Targets = append(td.Targets, TargetWithOutput{
					Target: dt,
				})
			} else {
				for _, name := range dt.Out.Names() {
					td.Targets = append(td.Targets, TargetWithOutput{
						Target: dt,
						Output: name,
					})
				}
			}
		} else {
			if !dt.Out.HasName(spec.Output) {
				return TargetDeps{}, fmt.Errorf("%v does not have named output `%v`", dt.FQN, spec.Output)
			}

			td.Targets = append(td.Targets, TargetWithOutput{
				Target:     dt,
				Output:     spec.Output,
				SpecOutput: spec.Output,
			})
		}
	}

	for _, file := range deps.Files {
		if strings.HasPrefix(file.Path, "/") {
			td.Files = append(td.Files, fs.NewPathAbs(
				t.Package.Root.Root(),
				file.Path[1:],
				e.Root.Join(file.Path[1:]).Abs(),
			))
		} else {
			td.Files = append(td.Files, fs.NewPathAbs(
				t.Package.Root.Root(),
				filepath.Join(t.Package.FullName, file.Path),
				t.Package.Root.Join(file.Path).Abs(),
			))
		}
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
		return t.Target.FQN + t.Output
	})
	td.Files = utils.DedupKeepLast(td.Files, func(t fs.Path) string {
		return t.RelRoot()
	})

	td.Sort()

	return td, nil
}
