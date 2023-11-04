package graph

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"github.com/heimdalr/dag"
	"github.com/hephbuild/heph/exprs"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/status"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/sets"
	"github.com/hephbuild/heph/utils/xfs"
	"github.com/hephbuild/heph/utils/xmath"
	"math"
	"os"
	"path/filepath"
	"strings"
)

func (e *State) Register(spec specs.Target) error {
	err := e.processTargetSpec(&spec)
	if err != nil {
		return err
	}

	err = spec.Validate()
	if err != nil {
		return err
	}

	l := e.targetsLock.Get(spec.Addr)
	l.Lock()
	defer l.Unlock()

	if t := e.targets.FindT(spec); t != nil {
		if !t.Spec().Equal(spec) {
			return fmt.Errorf("%v is already declared and does not equal the one defined in %v\n%s\n\n%s", spec.Addr, t.Source, t.Json(), spec.Json())
		}

		return nil
	}

	t := &Target{
		Target: spec,
	}

	err = e.processTarget(t)
	if err != nil {
		return err
	}

	e.targets.Add(t)

	return nil
}

func (e *State) processTargetSpec(t *specs.Target) error {
	// Validate Addr
	_, err := specs.ParseTargetAddr("", t.Addr)
	if err != nil {
		return fmt.Errorf("%v: %w", t.Addr, err)
	}

	if t.Cache.History == 0 {
		t.Cache.History = e.Config.CacheHistory
	}

	return nil
}

func (e *State) processTarget(t *Target) error {
	if t.processed {
		panic(fmt.Errorf("%v has already been processed", t.Addr))
	}

	switch t.Codegen {
	case specs.CodegenCopy, specs.CodegenLink:
		for _, file := range t.Spec().Out {
			p := t.Package.Root.Join(file.Path).RelRoot()

			if ct, ok := e.codegenPaths[p]; ok && ct != t {
				return fmt.Errorf("%v: target %v codegen already outputs %v", t.Addr, ct.Addr, p)
			}

			e.codegenPaths[p] = t
		}
	}

	if t.IsTool() {
		e.Tools.Add(t)
	}

	e.registerLabels(t.Labels)

	t.processed = true

	return nil
}

func (e *State) LinkTargets(ctx context.Context, ignoreNotFoundError bool, targets []*Target, emit bool) error {
	linkDone := log.TraceTiming("link targets")
	defer linkDone()

	if targets == nil {
		targets = e.Targets().Slice()
	}

	for _, target := range targets {
		target.resetLinking()
	}

	if emit {
		status.Emit(ctx, status.String("Linking targets..."))
	}

	for i, target := range targets {
		if err := ctx.Err(); err != nil {
			return err
		}

		if emit {
			percent := math.Round(xmath.Percent(i, len(targets)))
			status.EmitInteractive(ctx, status.String(xmath.FormatPercent("Linking targets [P]...", percent)))
		}

		//log.Tracef("# Linking target %v %v/%v", target.Addr, i+1, len(targets))
		err := e.LinkTarget(target, nil)
		if err != nil {
			if !ignoreNotFoundError || (ignoreNotFoundError && !errors.Is(err, specs.TargetNotFoundErr{})) {
				return fmt.Errorf("%v: %w", target.Addr, err)
			}
		}
	}

	return nil
}

func (e *State) filterOutCodegenFromDeps(t *Target, td TargetDeps) TargetDeps {
	td.Files = ads.Filter(td.Files, func(file xfs.Path) bool {
		if dep, ok := e.GetCodegenOrigin(file.RelRoot()); ok {
			log.Tracef("%v: %v removed from deps, as %v outputs it", t.Addr, file.RelRoot(), dep.Addr)
			return false
		}

		return true
	})

	return td
}

func (e *State) preventDepOnTool(t *Target, td TargetDeps) error {
	for _, target := range td.Targets {
		if target.Target.IsTool() {
			return fmt.Errorf("cannot depend on %v because it is a tool", target.Target.Addr)
		}
	}

	return nil
}

func (e *State) RequiredMatchers(ts []*Target) (specs.Matcher, error) {
	a := newRequiredMatchersAnalyzer(e.Targets(), e.exprFunctions())
	return a.requiredMatchers(ts...)
}

func (e *State) LinkTarget(t *Target, breadcrumb *sets.StringSet) (rerr error) {
	if !t.processed {
		panic(fmt.Sprintf("%v has not been processed", t.Addr))
	}

	if breadcrumb != nil {
		if breadcrumb.Has(t.Addr) {
			addrs := append(breadcrumb.Slice(), t.Addr)
			return fmt.Errorf("linking cycle: %v", addrs)
		}
		breadcrumb = breadcrumb.Copy()
	} else {
		breadcrumb = sets.NewStringSet(1)
	}
	breadcrumb.Add(t.Addr)

	//logPrefix := strings.Repeat("|", breadcrumb.Len()-1)

	t.m.Lock()
	if t.AllTargetDeps == nil {
		t.AllTargetDeps = NewTargets(0)
	}

	if t.deeplinked {
		t.m.Unlock()
		return nil
	} else if t.linked {
		for _, dep := range t.AllTargetDeps.Slice() {
			err := e.LinkTarget(dep, breadcrumb)
			if err != nil {
				t.linkingErr = err
				t.linked = false
				return err
			}
		}

		t.deeplinked = true

		t.m.Unlock()

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

	defer func() {
		t.linkingErr = rerr
		t.linking = false
		if rerr == nil {
			t.linked = true
			t.deeplinked = true
		}
		close(t.linkingCh)
		t.m.Unlock()
	}()

	var err error

	//log.Tracef(logPrefix+"Linking %v", t.Addr)
	//defer func() {
	//	log.Tracef(logPrefix+"Linking %v done", t.Addr)
	//}()

	//log.Tracef(logPrefix + "Linking tools")

	t.Tools, err = e.linkTargetTools(t, t.Spec().Tools, breadcrumb)
	if err != nil {
		return err
	}

	//log.Tracef(logPrefix + "Linking deps")
	t.Deps, err = e.linkTargetNamedDeps(t, t.Spec().Deps, breadcrumb)
	if err != nil {
		return fmt.Errorf("%v: deps: %w", t.Addr, err)
	}

	// Resolve hash deps specs
	if t.Spec().DifferentHashDeps {
		//log.Tracef(logPrefix + "Linking hashdeps")
		t.HashDeps, err = e.linkTargetDeps(t, t.Spec().HashDeps, breadcrumb)
		if err != nil {
			return fmt.Errorf("%v: hashdeps: %w", t.Addr, err)
		}
		t.HashDeps = e.filterOutCodegenFromDeps(t, t.HashDeps)
		err = e.preventDepOnTool(t, t.HashDeps)
		if err != nil {
			return err
		}
	} else {
		t.HashDeps = t.Deps.All()
	}

	if t.IsTool() {
		ts := t.Spec().Tools
		if len(ts.Targets) != 1 || len(ts.Hosts) > 0 {
			return fmt.Errorf("is a tool, must have a single `tool` with a single output")
		}
	}

	// Resolve transitive spec
	t.OwnTransitive = TargetTransitive{}
	t.OwnTransitive.Tools, err = e.linkTargetTools(t, t.Spec().Transitive.Tools, breadcrumb)
	if err != nil {
		return err
	}
	t.OwnTransitive.Deps, err = e.linkTargetNamedDeps(t, t.Spec().Transitive.Deps, breadcrumb)
	if err != nil {
		return err
	}
	t.OwnTransitive.Env = t.Spec().Transitive.Env
	t.OwnTransitive.RuntimeEnv = map[string]TargetRuntimeEnv{}
	for k, v := range t.Spec().Transitive.RuntimeEnv {
		t.OwnTransitive.RuntimeEnv[k] = TargetRuntimeEnv{
			Value:  v,
			Target: t,
		}
	}
	t.OwnTransitive.PassEnv = t.Spec().Transitive.PassEnv
	t.OwnTransitive.RuntimePassEnv = t.Spec().Transitive.RuntimePassEnv

	t.DeepOwnTransitive, err = e.collectDeepTransitive(t.OwnTransitive, breadcrumb)
	if err != nil {
		return err
	}

	relPathFactory := func(p string) xfs.RelPath {
		abs := strings.HasPrefix(p, "/")

		var relRoot string
		if abs {
			relRoot = strings.TrimPrefix(p, "/")
		} else {
			relRoot = filepath.Join(t.Package.Path, p)
		}

		return xfs.NewRelPath(relRoot)
	}

	t.Out = &OutNamedPaths{}
	t.OutWithSupport = &OutNamedPaths{}
	for _, file := range t.Spec().Out {
		// SupportFilesOutput should be excluded from normal outputs
		if file.Name != specs.SupportFilesOutput {
			t.Out.Add(file.Name, relPathFactory(file.Path))
		}

		t.OutWithSupport.Add(file.Name, relPathFactory(file.Path))
	}
	t.Out.Sort()
	t.OutWithSupport.Sort()

	t.RestoreCachePaths = nil
	if t.Spec().RestoreCache.Enabled {
		t.RestoreCachePaths = ads.Map(t.Spec().RestoreCache.Paths, func(p string) xfs.RelPath {
			return relPathFactory(p)
		})
		t.RestoreCachePaths.Sort()
	}

	t.RuntimePassEnv = []string{}
	t.RuntimePassEnv = append(t.RuntimePassEnv, t.Spec().RuntimePassEnv...)

	t.Env = map[string]string{}
	e.applyEnv(t, t.Spec().PassEnv, t.Spec().Env)

	t.RuntimeEnv = map[string]TargetRuntimeEnv{}
	for k, v := range t.Spec().RuntimeEnv {
		t.RuntimeEnv[k] = TargetRuntimeEnv{
			Value:  v,
			Target: t,
		}
	}

	// Apply transitive deps
	//log.Tracef(logPrefix + "Linking transitive")
	t.TransitiveDeps, t.Platforms, err = e.collectTransitiveFromDeps(t, breadcrumb)
	if err != nil {
		return err
	}

	if !t.TransitiveDeps.Tools.Empty() {
		t.Tools = t.Tools.Merge(t.TransitiveDeps.Tools)
		t.Tools.Dedup()
		t.Tools.Sort()
	}

	if !t.TransitiveDeps.Deps.Empty() {
		t.Deps = t.Deps.Merge(t.TransitiveDeps.Deps)
		t.Deps.Dedup()
		t.Deps.Sort()

		if t.DifferentHashDeps {
			t.HashDeps = t.HashDeps.Merge(t.TransitiveDeps.Deps.All())
			t.HashDeps.Dedup()
			t.HashDeps.Sort()
		} else {
			t.HashDeps = t.Deps.All()
		}
	}

	for k, v := range t.TransitiveDeps.RuntimeEnv {
		t.RuntimeEnv[k] = v
	}
	t.RuntimePassEnv = append(t.RuntimePassEnv, t.TransitiveDeps.RuntimePassEnv...)
	e.applyEnv(t, t.TransitiveDeps.PassEnv, t.TransitiveDeps.Env)

	err = e.registerDag(t)
	if err != nil {
		return err
	}

	t.Artifacts = e.newArtifactRegistry(t)

	parents, err := e.DAG().GetParents(t)
	if err != nil {
		return err
	}

	t.AllTargetDeps.AddAll(parents)
	t.AllTargetDeps.Sort()

	return nil
}

func (e *State) registerDag(t *Target) error {
	_, err := e.dag.AddVertex(t)
	if err != nil && !errors.As(err, &dag.VertexDuplicateError{}) {
		return err
	}

	addEdge := func(src *Target, dst *Target) error {
		ok, err := e.dag.IsEdge(src.Addr, dst.Addr)
		if ok || err != nil {
			return err
		}

		return e.dag.AddEdge(src.Addr, dst.Addr)
	}

	for _, dep := range t.Deps.All().Targets {
		err := addEdge(dep.Target, t)
		if err != nil {
			return fmt.Errorf("dep: %v to %v: %w", dep.Target.Addr, t.Addr, err)
		}
	}

	if t.DifferentHashDeps {
		for _, dep := range t.HashDeps.Targets {
			err := addEdge(dep.Target, t)
			if err != nil {
				return fmt.Errorf("hashdep: %v to %v: %w", dep.Target.Addr, t.Addr, err)
			}
		}
	}

	for _, target := range t.Tools.TargetReferences {
		err := addEdge(target, t)
		if err != nil {
			return fmt.Errorf("tool: %v to %v: %w", target.Addr, t.Addr, err)
		}
	}

	return nil
}

func (e *State) linkTargetNamedDeps(t *Target, deps specs.Deps, breadcrumb *sets.StringSet) (TargetNamedDeps, error) {
	m := map[string]specs.Deps{}
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

		err = e.preventDepOnTool(t, ldeps)
		if err != nil {
			return TargetNamedDeps{}, err
		}

		td.Set(name, ldeps)
	}

	td.Map(func(deps TargetDeps) TargetDeps {
		return e.filterOutCodegenFromDeps(t, deps)
	})

	td.Dedup()
	td.Sort()

	return td, nil
}

func (e *State) linkTargetTools(t *Target, toolsSpecs specs.Tools, breadcrumb *sets.StringSet) (TargetTools, error) {
	refs := sets.NewSet(func(t *Target) string {
		return t.Addr
	}, len(toolsSpecs.Targets))

	targetTools := make([]TargetWithOutput, 0)
	for _, tool := range toolsSpecs.Targets {
		tt := e.Targets().Find(tool.Target)
		if tt == nil {
			return TargetTools{}, specs.NewTargetNotFoundError(tool.Target, e.Targets())
		}

		err := e.LinkTarget(tt, breadcrumb)
		if err != nil {
			return TargetTools{}, fmt.Errorf("tool: %v: %w", tool, err)
		}

		refs.Add(tt)

		if tool.Output == "" {
			for _, name := range tt.Out.Names() {
				targetTools = append(targetTools, TargetWithOutput{
					Target: tt,
					Output: name,
					Name:   tool.Name,
				})
			}
		} else {
			targetTools = append(targetTools, TargetWithOutput{
				Target: tt,
				Output: tool.Output,
				Name:   tool.Name,
			})
		}
	}

	for _, tool := range toolsSpecs.Exprs {
		targets, err := e.execExpr(t, tool.Expr, breadcrumb)
		if err != nil {
			return TargetTools{}, err
		}

		for _, target := range targets {
			targetTools = append(targetTools, TargetWithOutput{
				Target: target,
				Name:   tool.Name,
			})
		}
	}

	if InlineGroups {
		var err error
		targetTools, err = ads.MapFlatE(targetTools, func(tdep TargetWithOutput) ([]TargetWithOutput, error) {
			if !tdep.Target.IsGroup() {
				return []TargetWithOutput{tdep}, nil
			}

			return inlineGroups(tdep)
		})
		if err != nil {
			return TargetTools{}, err
		}
	}

	tools := make([]TargetTool, 0, len(targetTools))

	for _, tool := range targetTools {
		tt := tool.Target

		refs.Add(tt)

		var name string
		if tool.Name != "" {
			name = tool.Name
		} else {
			name = tool.Output
		}

		var paths map[string]xfs.RelPaths
		if tool.Output != "" {
			npaths := tt.Out.Name(tool.Output)

			if len(npaths) == 0 {
				return TargetTools{}, ErrDoesNotHaveOutput{tt.Addr, tool.Output}
			}

			paths = map[string]xfs.RelPaths{
				name: npaths,
			}
		} else {
			paths = tt.Out.Named()

			if len(paths) == 0 && tool.Target.DeepOwnTransitive.Empty() {
				return TargetTools{}, fmt.Errorf("%v has no output", tt.Addr)
			}

			if name != "" {
				npaths := map[string]xfs.RelPaths{}
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
				return TargetTools{}, fmt.Errorf("%v: each named output can only output one file to be used as a tool", tt.Addr)
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
		TargetReferences: refs.Slice(),
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

func (e *State) applyEnv(t *Target, passEnv []string, env map[string]string) {
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

func (e *State) collectDeepTransitive(tr TargetTransitive, breadcrumb *sets.StringSet) (TargetTransitive, error) {
	targets := sets.NewSet(func(t *Target) string {
		return t.Addr
	}, 0)

	for _, dep := range tr.Deps.All().Targets {
		targets.Add(dep.Target)
	}
	for _, dep := range tr.Deps.All().RawTargets {
		targets.Add(dep.Target)
	}
	for _, dep := range tr.Tools.Targets {
		targets.Add(dep.Target)
	}
	targets.AddAll(tr.Tools.TargetReferences)

	dtr, err := e.collectTransitive(breadcrumb, targets.Slice())
	if err != nil {
		return TargetTransitive{}, err
	}

	dtr = dtr.Merge(tr)

	return dtr, nil
}

func (e *State) collectTransitiveFromDeps(t *Target, breadcrumb *sets.StringSet) (TargetTransitive, []specs.Platform, error) {
	targets := sets.NewSet(func(t *Target) string {
		return t.Addr
	}, 0)

	for _, dep := range t.Deps.All().Targets {
		targets.Add(dep.Target)
	}
	// Include group targets
	for _, dep := range t.Deps.All().RawTargets {
		targets.Add(dep.Target)
	}
	for _, tool := range t.Tools.Targets {
		targets.Add(tool.Target)
	}
	// Include group targets
	targets.AddAll(t.Tools.TargetReferences)

	tr, err := e.collectTransitive(breadcrumb, targets.Slice())
	if err != nil {
		return TargetTransitive{}, nil, err
	}

	platforms, err := e.computePlatformsFromTransitiveTargets(t, targets.Slice())
	if err != nil {
		return tr, nil, err
	}

	return tr, platforms, nil
}

func (e *State) computePlatformsFromTransitiveTargets(t *Target, targets []*Target) ([]specs.Platform, error) {
	if !t.HasDefaultPlatforms() {
		return t.Spec().Platforms, nil
	}

	if len(targets) == 0 {
		return t.Spec().Platforms, nil
	}

	transitivePlatforms := sets.NewSet(func(plats []specs.Platform) string {
		b, err := json.Marshal(plats)
		if err != nil {
			panic(err)
		}

		return string(b)
	}, 0)

	for _, target := range targets {
		if len(target.Transitive.Platforms) == 0 {
			continue
		}

		transitivePlatforms.Add(target.Transitive.Platforms)
	}

	switch transitivePlatforms.Len() {
	case 0:
		return t.Spec().Platforms, nil
	case 1:
		return transitivePlatforms.Slice()[0], nil
	}

	configs := strings.Join(ads.Map(transitivePlatforms.Slice(), func(tr []specs.Platform) string {
		b, err := json.Marshal(tr)
		if err != nil {
			panic(err)
		}

		return " - " + string(b)
	}), "\n")

	return nil, fmt.Errorf("has conflicting transitive platform config, you must override `platforms`, got:\n%v", configs)
}

func (e *State) collectTransitive(breadcrumb *sets.StringSet, targets []*Target) (TargetTransitive, error) {
	tt := TargetTransitive{}

	for _, target := range targets {
		err := e.LinkTarget(target, breadcrumb)
		if err != nil {
			return TargetTransitive{}, err
		}
	}

	for _, target := range targets {
		tt = tt.Merge(target.DeepOwnTransitive)
	}

	for _, dep := range tt.Deps.All().Targets {
		err := e.LinkTarget(dep.Target, breadcrumb)
		if err != nil {
			return TargetTransitive{}, err
		}
	}

	for _, t := range tt.Tools.TargetReferences {
		err := e.LinkTarget(t, breadcrumb)
		if err != nil {
			return TargetTransitive{}, err
		}
	}

	return tt, nil
}

func (e *State) execExpr(t *Target, expr exprs.Expr, breadcrumb *sets.StringSet) ([]*Target, error) {
	m := e.exprFunctions()

	f, ok := m[expr.Function]
	if !ok {
		return nil, fmt.Errorf("unhandled function %v", expr.Function)
	}

	return f.Run(t, expr, breadcrumb)
}

const InlineGroups = true

type ErrDoesNotHaveOutput struct {
	Addr, Output string
}

func (e ErrDoesNotHaveOutput) Error() string {
	return fmt.Sprintf("%v does not have named output `%v`", e.Addr, e.Output)
}

func (e *State) linkTargetDeps(t *Target, deps specs.Deps, breadcrumb *sets.StringSet) (TargetDeps, error) {
	td := TargetDeps{}

	for _, expr := range deps.Exprs {
		targets, err := e.execExpr(t, expr.Expr, breadcrumb)
		if err != nil {
			return TargetDeps{}, err
		}

		for _, target := range targets {
			if len(target.Out.Names()) == 0 {
				td.Targets = append(td.Targets, TargetWithOutput{
					Target: target,
				})
			} else {
				td.Targets = ads.GrowExtra(td.Targets, len(target.Out.Names()))
				for _, name := range target.Out.Names() {
					td.Targets = append(td.Targets, TargetWithOutput{
						Target: target,
						Output: name,
					})
				}
			}
		}
	}

	td.Targets = ads.GrowExtra(td.Targets, len(deps.Targets))
	for _, spec := range deps.Targets {
		dt := e.Targets().Find(spec.Target)
		if dt == nil {
			return TargetDeps{}, specs.NewTargetNotFoundError(spec.Target, e.Targets())
		}

		err := e.LinkTarget(dt, breadcrumb)
		if err != nil {
			return TargetDeps{}, err
		}

		if spec.Output == "" {
			if len(dt.Out.Names()) == 0 {
				td.Targets = append(td.Targets, TargetWithOutput{
					Target: dt,
					Mode:   spec.Mode,
					Name:   "",
				})
			} else {
				td.Targets = ads.GrowExtra(td.Targets, len(dt.Out.Names()))

				for _, name := range dt.Out.Names() {
					td.Targets = append(td.Targets, TargetWithOutput{
						Target: dt,
						Output: name,
						Mode:   spec.Mode,
						Name:   name,
					})
				}
			}
		} else {
			if !dt.IsGroup() && !dt.Out.HasName(spec.Output) {
				return TargetDeps{}, ErrDoesNotHaveOutput{dt.Addr, spec.Output}
			}

			td.Targets = append(td.Targets, TargetWithOutput{
				Target: dt,
				Output: spec.Output,
				Mode:   spec.Mode,
				Name:   "",
			})
		}
	}

	td.Files = ads.GrowExtra(td.Files, len(deps.Files))
	for _, file := range deps.Files {
		var p xfs.Path
		if strings.HasPrefix(file.Path, "/") {
			p = xfs.NewPathAbs(
				t.Package.Root.Root(),
				file.Path[1:],
				e.Root.Root.Join(file.Path[1:]).Abs(),
			)
		} else {
			p = xfs.NewPathAbs(
				t.Package.Root.Root(),
				filepath.Join(t.Package.Path, file.Path),
				t.Package.Root.Join(file.Path).Abs(),
			)
		}

		if !xfs.PathExists(p.Abs()) {
			return TargetDeps{}, fmt.Errorf("%v: %w", p.Abs(), os.ErrNotExist)
		}

		td.Files = append(td.Files, p)
	}

	td.RawTargets = td.Targets

	if InlineGroups {
		var err error
		td.Targets, err = ads.MapFlatE(td.Targets, func(tdep TargetWithOutput) ([]TargetWithOutput, error) {
			if !tdep.Target.IsGroup() {
				return []TargetWithOutput{tdep}, nil
			}

			td.Files = append(td.Files, tdep.Target.Deps.All().Files...)
			td.RawTargets = append(td.RawTargets, tdep.Target.Deps.All().RawTargets...)

			return inlineGroups(tdep)
		})
		if err != nil {
			return TargetDeps{}, err
		}
	}

	td.Dedup()
	td.Sort()

	return td, nil
}

func inlineGroups(tdep TargetWithOutput) ([]TargetWithOutput, error) {
	dt := tdep.Target

	if tdep.Output == "" {
		if !dt.Deps.IsNamed() {
			return ads.Map(dt.Deps.All().Targets, func(dep TargetWithOutput) TargetWithOutput {
				dep.Name = ""
				return dep
			}), nil
		}

		deps := make([]TargetWithOutput, 0)
		for _, name := range dt.Deps.Names() {
			deps = append(deps, ads.Map(dt.Deps.Name(name).Targets, func(dep TargetWithOutput) TargetWithOutput {
				return TargetWithOutput{
					Name:   name,
					Target: dep.Target,
					Output: dep.Output,
					Mode:   tdep.Mode,
				}
			})...)
		}

		return deps, nil
	}

	if dt.Deps.IsNamed() {
		if !dt.Deps.HasName(tdep.Output) {
			return nil, ErrDoesNotHaveOutput{dt.Addr, tdep.Output}
		}

		return ads.Map(dt.Deps.Name(tdep.Output).Targets, func(dep TargetWithOutput) TargetWithOutput {
			return TargetWithOutput{
				Name:   tdep.Name,
				Target: dep.Target,
				Output: dep.Output,
				Mode:   tdep.Mode,
			}
		}), nil
	} else {
		deps := ads.Filter(dt.Deps.All().Targets, func(dep TargetWithOutput) bool {
			return dep.Output == tdep.Output
		})

		deps = ads.Map(deps, func(dep TargetWithOutput) TargetWithOutput {
			dep.Name = tdep.Name
			return dep
		})

		if len(deps) == 0 {
			return nil, ErrDoesNotHaveOutput{dt.Addr, tdep.Output}
		}

		return deps, nil
	}
}
