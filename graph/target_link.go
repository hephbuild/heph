package graph

import (
	"context"
	"errors"
	"fmt"
	"github.com/heimdalr/dag"
	"github.com/hephbuild/heph/exprs"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/targetspec"
	"github.com/hephbuild/heph/tgt"
	"github.com/hephbuild/heph/utils/sets"
	"github.com/hephbuild/heph/utils/xfs"
	"os"
	"path/filepath"
	"strings"
)

func (e *State) Register(spec targetspec.TargetSpec) error {
	err := e.processTargetSpec(&spec)
	if err != nil {
		return err
	}

	l := e.targetsLock.Get(spec.FQN)
	l.Lock()
	defer l.Unlock()

	if t := e.targets.Find(spec.FQN); t != nil {
		if !t.TargetSpec.Equal(spec) {
			return fmt.Errorf("%v is already declared and does not equal the one defined in %v\n%s\n\n%s", spec.FQN, t.Source, t.Json(), spec.Json())
		}

		return nil
	}

	t := &Target{
		Target: &tgt.Target{
			TargetSpec: spec,
		},
	}

	err = e.processTarget(t)
	if err != nil {
		return err
	}

	e.targets.Add(t)

	return nil
}

func (e *State) processTargetSpec(t *targetspec.TargetSpec) error {
	// Validate FQN
	_, err := targetspec.TargetParse("", t.FQN)
	if err != nil {
		return fmt.Errorf("%v: %w", t.FQN, err)
	}

	if t.Cache.History == 0 {
		t.Cache.History = e.Config.CacheHistory
	}

	return nil
}

func (e *State) processTarget(t *Target) error {
	if t.processed {
		panic(fmt.Errorf("%v has already been processed", t.FQN))
	}

	switch t.Codegen {
	case targetspec.CodegenCopy, targetspec.CodegenLink:
		for _, file := range t.TargetSpec.Out {
			p := t.Package.Root.Join(file.Path).RelRoot()

			if ct, ok := e.codegenPaths[p]; ok && ct != t {
				return fmt.Errorf("%v: target %v codegen already outputs %v", t.FQN, ct.FQN, p)
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

func (e *State) LinkTargets(ctx context.Context, ignoreNotFoundError bool, targets []*Target) error {
	linkDone := log.TraceTiming("link targets")
	defer linkDone()

	for _, target := range e.Targets().Slice() {
		target.resetLinking()
	}

	if targets == nil {
		targets = e.Targets().Slice()
	}

	for _, target := range targets {
		if err := ctx.Err(); err != nil {
			return err
		}

		//log.Tracef("# Linking target %v %v/%v", target.FQN, i+1, len(targets))
		err := e.LinkTarget(target, nil)
		if err != nil {
			if !ignoreNotFoundError || (ignoreNotFoundError && !errors.Is(err, TargetNotFoundErr{})) {
				return fmt.Errorf("%v: %w", target.FQN, err)
			}
		}
	}

	return nil
}

func (e *State) filterOutCodegenFromDeps(t *Target, td tgt.TargetDeps) tgt.TargetDeps {
	files := make(xfs.Paths, 0, len(td.Files))
	for _, file := range td.Files {
		if dep, ok := e.GetCodegenOrigin(file.RelRoot()); ok {
			log.Tracef("%v: %v removed from deps, and %v outputs it", t.FQN, file.RelRoot(), dep.FQN)
		} else {
			files = append(files, file)
		}
	}
	td.Files = files

	return td
}

func (e *State) preventDepOnTool(t *Target, td tgt.TargetDeps) error {
	for _, target := range td.Targets {
		if target.Target.IsTool() {
			return fmt.Errorf("cannot depend on %v because it is a tool", target.Target.FQN)
		}
	}

	return nil
}

func (e *State) LinkTarget(t *Target, breadcrumb *sets.StringSet) (rerr error) {
	if !t.processed {
		panic(fmt.Sprintf("%v has not been processed", t.FQN))
	}

	if breadcrumb != nil {
		if breadcrumb.Has(t.FQN) {
			fqns := append(breadcrumb.Slice(), t.FQN)
			return fmt.Errorf("linking cycle: %v", fqns)
		}
		breadcrumb = breadcrumb.Copy()
	} else {
		breadcrumb = sets.NewStringSet(1)
	}
	breadcrumb.Add(t.FQN)

	//logPrefix := strings.Repeat("|", breadcrumb.Len()-1)

	t.m.Lock()
	if t.LinkingDeps == nil {
		t.LinkingDeps = NewTargets(0)
	}

	if t.deeplinked {
		t.m.Unlock()
		return nil
	} else if t.linked {
		for _, dep := range t.LinkingDeps.Slice() {
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
		close(t.linkingCh)
		t.linking = false
		if rerr == nil {
			t.linked = true
			t.deeplinked = true
		}
	}()

	defer t.m.Unlock()

	var err error

	//log.Tracef(logPrefix+"Linking %v", t.FQN)
	//defer func() {
	//	log.Tracef(logPrefix+"Linking %v done", t.FQN)
	//}()

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
		err = e.preventDepOnTool(t, t.HashDeps)
		if err != nil {
			return err
		}
	} else {
		t.HashDeps = t.Deps.All()
	}

	if t.IsTool() {
		ts := t.TargetSpec.Tools
		if len(ts.Targets) != 1 || len(ts.Hosts) > 0 {
			return fmt.Errorf("is a tool, mut have a single `tool` with a single output")
		}
	}

	// Resolve transitive spec
	t.OwnTransitive = tgt.TargetTransitive{}
	t.OwnTransitive.Tools, err = e.linkTargetTools(t, t.TargetSpec.Transitive.Tools, breadcrumb)
	if err != nil {
		return err
	}
	t.OwnTransitive.Deps, err = e.linkTargetNamedDeps(t, t.TargetSpec.Transitive.Deps, breadcrumb)
	if err != nil {
		return err
	}
	t.OwnTransitive.Env = t.TargetSpec.Transitive.Env
	t.OwnTransitive.RuntimeEnv = map[string]tgt.TargetRuntimeEnv{}
	for k, v := range t.TargetSpec.Transitive.RuntimeEnv {
		t.OwnTransitive.RuntimeEnv[k] = tgt.TargetRuntimeEnv{
			Value:  v,
			Target: t.Target,
		}
	}
	t.OwnTransitive.PassEnv = t.TargetSpec.Transitive.PassEnv
	t.OwnTransitive.RuntimePassEnv = t.TargetSpec.Transitive.RuntimePassEnv

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

	t.Out = &tgt.OutNamedPaths{}
	t.OutWithSupport = &tgt.OutNamedPaths{}
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
			return fmt.Errorf("%v cannot cache target which isn't sandboxed", t.FQN)
		}
	}

	t.RuntimePassEnv = []string{}
	t.RuntimePassEnv = append(t.RuntimePassEnv, t.TargetSpec.RuntimePassEnv...)

	t.Env = map[string]string{}
	e.applyEnv(t, t.TargetSpec.PassEnv, t.TargetSpec.Env)

	t.RuntimeEnv = map[string]tgt.TargetRuntimeEnv{}
	for k, v := range t.TargetSpec.RuntimeEnv {
		t.RuntimeEnv[k] = tgt.TargetRuntimeEnv{
			Value:  v,
			Target: t.Target,
		}
	}

	// Apply transitive deps
	//log.Tracef(logPrefix + "Linking transitive")
	t.TransitiveDeps, err = e.collectTransitiveFromDeps(t, breadcrumb)
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

	t.Artifacts = e.newArtifactOrchestrator(t)

	parents, err := e.DAG().GetParents(t)
	if err != nil {
		return err
	}
	t.LinkingDeps.AddAll(parents)
	t.LinkingDeps.Sort()

	return nil
}

func (e *State) registerDag(t *Target) error {
	_, err := e.dag.AddVertex(t)
	if err != nil && !errors.As(err, &dag.VertexDuplicateError{}) {
		return err
	}

	addEdge := func(src *tgt.Target, dst *Target) error {
		ok, err := e.dag.IsEdge(src.FQN, dst.FQN)
		if ok || err != nil {
			return err
		}

		return e.dag.AddEdge(src.FQN, dst.FQN)
	}

	for _, dep := range t.Deps.All().Targets {
		err := addEdge(dep.Target, t)
		if err != nil {
			return fmt.Errorf("dep: %v to %v: %w", dep.Target.FQN, t.FQN, err)
		}
	}

	if t.DifferentHashDeps {
		for _, dep := range t.HashDeps.Targets {
			err := addEdge(dep.Target, t)
			if err != nil {
				return fmt.Errorf("hashdep: %v to %v: %w", dep.Target.FQN, t.FQN, err)
			}
		}
	}

	for _, target := range t.Tools.TargetReferences {
		err := addEdge(target, t)
		if err != nil {
			return fmt.Errorf("tool: %v to %v: %w", target.FQN, t.FQN, err)
		}
	}

	return nil
}

func (e *State) linkTargetNamedDeps(t *Target, deps targetspec.TargetSpecDeps, breadcrumb *sets.StringSet) (tgt.TargetNamedDeps, error) {
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

	td := tgt.TargetNamedDeps{}
	for name, deps := range m {
		ldeps, err := e.linkTargetDeps(t, deps, breadcrumb)
		if err != nil {
			return tgt.TargetNamedDeps{}, err
		}

		err = e.preventDepOnTool(t, ldeps)
		if err != nil {
			return tgt.TargetNamedDeps{}, err
		}

		td.Set(name, ldeps)
	}

	td.Map(func(deps tgt.TargetDeps) tgt.TargetDeps {
		return e.filterOutCodegenFromDeps(t, deps)
	})

	td.Dedup()
	td.Sort()

	return td, nil
}

func (e *State) linkTargetTools(t *Target, toolsSpecs targetspec.TargetSpecTools, breadcrumb *sets.StringSet) (tgt.TargetTools, error) {
	type targetTool struct {
		Target *Target
		Output string
		Name   string
	}

	refs := make([]*tgt.Target, 0, len(toolsSpecs.Targets))
	targetTools := make([]targetTool, 0)
	for _, tool := range toolsSpecs.Targets {
		tt := e.Targets().Find(tool.Target)
		if tt == nil {
			return tgt.TargetTools{}, NewTargetNotFoundError(tool.Target)
		}

		err := e.LinkTarget(tt, breadcrumb)
		if err != nil {
			return tgt.TargetTools{}, fmt.Errorf("tool: %v: %w", tool, err)
		}

		refs = append(refs, tt.Target)

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
			return tgt.TargetTools{}, err
		}

		for _, target := range targets {
			targetTools = append(targetTools, targetTool{
				Target: target,
				Name:   tool.Name,
			})
			refs = append(refs, target.Target)
		}
	}

	tools := make([]tgt.TargetTool, 0, len(toolsSpecs.Targets))

	for _, tool := range targetTools {
		tt := tool.Target

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
				return tgt.TargetTools{}, fmt.Errorf("%v|%v has no output", tt.FQN, tool.Output)
			}

			paths = map[string]xfs.RelPaths{
				name: npaths,
			}
		} else {
			paths = tt.Out.Named()

			if len(paths) == 0 && tool.Target.DeepOwnTransitive.Empty() {
				return tgt.TargetTools{}, fmt.Errorf("%v has no output", tt.FQN)
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
				return tgt.TargetTools{}, fmt.Errorf("%v: each named output can only output one file to be used as a tool", tt.FQN)
			}

			path := paths[0]

			if name == "" {
				name = filepath.Base(path.RelRoot())
			}

			tools = append(tools, tgt.TargetTool{
				Target: tt.Target,
				Output: tool.Output,
				Name:   name,
				File:   path,
			})
		}
	}

	tt := tgt.TargetTools{
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

func (e *State) collectDeepTransitive(tr tgt.TargetTransitive, breadcrumb *sets.StringSet) (tgt.TargetTransitive, error) {
	targets := sets.NewSet(func(t *Target) string {
		return t.FQN
	}, 0)
	for _, dep := range tr.Deps.All().Targets {
		targets.Add(e.Targets().Find(dep.Target.FQN))
	}
	for _, dep := range tr.Tools.Targets {
		targets.Add(e.Targets().Find(dep.Target.FQN))
	}
	for _, t := range tr.Tools.TargetReferences {
		targets.Add(e.Targets().Find(t.FQN))
	}

	dtr, err := e.collectTransitive(targets.Slice(), breadcrumb)
	if err != nil {
		return tgt.TargetTransitive{}, err
	}
	dtr = dtr.Merge(tr)

	return dtr, nil
}

func (e *State) collectTransitiveFromDeps(t *Target, breadcrumb *sets.StringSet) (tgt.TargetTransitive, error) {
	targets := sets.NewSet(func(t *Target) string {
		return t.FQN
	}, 0)
	for _, dep := range t.Deps.All().Targets {
		targets.Add(e.Targets().Find(dep.Target.FQN))
	}
	for _, ref := range t.Tools.TargetReferences {
		targets.Add(e.Targets().Find(ref.FQN))
	}

	return e.collectTransitive(targets.Slice(), breadcrumb)
}

func (e *State) collectTransitive(deps []*Target, breadcrumb *sets.StringSet) (tgt.TargetTransitive, error) {
	tt := tgt.TargetTransitive{}

	for _, dep := range deps {
		tt = tt.Merge(dep.DeepOwnTransitive)
	}

	for _, dep := range tt.Deps.All().Targets {
		err := e.LinkTarget(e.Targets().Find(dep.Target.FQN), breadcrumb)
		if err != nil {
			return tgt.TargetTransitive{}, err
		}
	}

	for _, t := range tt.Tools.TargetReferences {
		err := e.LinkTarget(e.Targets().Find(t.FQN), breadcrumb)
		if err != nil {
			return tgt.TargetTransitive{}, err
		}
	}

	return tt, nil
}

func (e *State) targetExpr(t *Target, expr exprs.Expr, breadcrumb *sets.StringSet) ([]*Target, error) {
	switch expr.Function {
	case "collect":
		targets, err := e.collect(t, expr)
		if err != nil {
			return nil, fmt.Errorf("`%v`: %w", expr.String, err)
		}

		for _, target := range targets {
			err := e.LinkTarget(target, breadcrumb)
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
			err = e.LinkTarget(target, breadcrumb)
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

func (e *State) linkTargetDeps(t *Target, deps targetspec.TargetSpecDeps, breadcrumb *sets.StringSet) (tgt.TargetDeps, error) {
	td := tgt.TargetDeps{}

	for _, expr := range deps.Exprs {
		expr := expr.Expr

		targets, err := e.targetExpr(t, expr, breadcrumb)
		if err != nil {
			return tgt.TargetDeps{}, err
		}

		for _, target := range targets {
			if len(target.Out.Names()) == 0 {
				td.Targets = append(td.Targets, tgt.TargetWithOutput{
					Target: target.Target,
				})
			} else {
				for _, name := range target.Out.Names() {
					td.Targets = append(td.Targets, tgt.TargetWithOutput{
						Target: target.Target,
						Output: name,
					})
				}
			}
		}
	}

	for _, spec := range deps.Targets {
		dt := e.Targets().Find(spec.Target)
		if dt == nil {
			return tgt.TargetDeps{}, NewTargetNotFoundError(spec.Target)
		}

		err := e.LinkTarget(dt, breadcrumb)
		if err != nil {
			return tgt.TargetDeps{}, err
		}

		if spec.Output == "" {
			if len(dt.Out.Names()) == 0 {
				td.Targets = append(td.Targets, tgt.TargetWithOutput{
					Target: dt.Target,
					Mode:   spec.Mode,
				})
			} else {
				for _, name := range dt.Out.Names() {
					td.Targets = append(td.Targets, tgt.TargetWithOutput{
						Target: dt.Target,
						Output: name,
						Mode:   spec.Mode,
					})
				}
			}
		} else {
			if !dt.Out.HasName(spec.Output) {
				return tgt.TargetDeps{}, fmt.Errorf("%v does not have named output `%v`", dt.FQN, spec.Output)
			}

			td.Targets = append(td.Targets, tgt.TargetWithOutput{
				Target:     dt.Target,
				Output:     spec.Output,
				SpecOutput: spec.Output,
				Mode:       spec.Mode,
			})
		}
	}

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
			return tgt.TargetDeps{}, fmt.Errorf("%v: %w", p.Abs(), os.ErrNotExist)
		}

		td.Files = append(td.Files, p)
	}

	if InlineGroups {
		targets := make([]tgt.TargetWithOutput, 0, len(td.Targets))
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

	td.Dedup()
	td.Sort()

	return td, nil
}
