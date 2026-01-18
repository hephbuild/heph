package engine

import (
	"cmp"
	"context"
	"encoding/hex"
	"errors"
	"fmt"
	"maps"
	"path/filepath"
	"slices"
	"strings"
	"sync"

	"github.com/heimdalr/dag"
	"github.com/hephbuild/heph/internal/hdag"
	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/internal/hproto"
	"github.com/hephbuild/heph/internal/hproto/hashpb"
	"github.com/hephbuild/heph/internal/hslices"
	"github.com/hephbuild/heph/internal/htypes"
	"github.com/hephbuild/heph/internal/tmatch"
	"github.com/hephbuild/heph/plugin/pluginbin"
	"github.com/lithammer/fuzzysearch/fuzzy"
	sync_map "github.com/zolstein/sync-map"

	"github.com/hephbuild/heph/internal/hdebug"
	"github.com/hephbuild/heph/internal/herrgroup"
	"github.com/hephbuild/heph/lib/tref"

	"github.com/hephbuild/heph/internal/hcore/hlog"
	"github.com/hephbuild/heph/internal/hcore/hstep"
	"github.com/hephbuild/heph/lib/pluginsdk"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/plugingroup"
	"github.com/zeebo/xxh3"
	"go.opentelemetry.io/otel/attribute"
	"go.opentelemetry.io/otel/trace"
	"google.golang.org/protobuf/proto"
	"google.golang.org/protobuf/types/known/structpb"
)

type SpecContainer struct {
	Ref  *pluginv1.TargetRef
	Spec *pluginv1.TargetSpec
}

func (c SpecContainer) GetRef() *pluginv1.TargetRef {
	if c.Spec != nil {
		return c.Spec.GetRef()
	}
	if c.Ref != nil {
		return c.Ref
	}

	panic("ref or spec must be specified")
}

func (e *Engine) resolveProvider(
	ctx context.Context,
	rs *RequestState,
	states []*pluginv1.ProviderState,
	c SpecContainer,
	p EngineProvider,
) (*pluginv1.TargetSpec, error) {
	res, err := e.Get(ctx, rs, p, c.GetRef(), states)
	if err != nil {
		return nil, err
	}

	// TODO: what do we do if the spec.ref is different from the request ?

	return res.GetSpec(), nil
}

func (e *Engine) resolveSpec(ctx context.Context, rs *RequestState, states []*pluginv1.ProviderState, c SpecContainer) (*pluginv1.TargetSpec, error) {
	ctx, span := tracer.Start(ctx, "ResolveSpec", trace.WithAttributes(attribute.String("target", tref.Format(c.GetRef()))))
	defer span.End()

	spec, err, computed := rs.memSpec.Do(ctx, refKey(c.GetRef()), func(ctx context.Context) (*pluginv1.TargetSpec, error) {
		ref := c.GetRef()
		switch {
		case ref.GetPackage() == tref.QueryPackage:
			return e.resolveSpecQuery(ctx, rs, ref)
		case ref.GetPackage() == tref.BinPackage:
			return e.resolveHostBin(ctx, rs, ref)
		}

		for _, p := range e.Providers {
			var providerStates []*pluginv1.ProviderState
			for _, state := range states {
				if state.GetProvider() == p.Name {
					providerStates = append(providerStates, state)
				}
			}

			spec, err := e.resolveProvider(ctx, rs, providerStates, c, p)
			if err != nil {
				if errors.Is(err, pluginsdk.ErrNotFound) {
					continue
				}

				return nil, err
			}

			if spec == nil {
				return nil, fmt.Errorf("invalid nil spec: %v", tref.Format(c.GetRef()))
			}

			return spec, nil
		}

		return nil, errors.New("target not found")
	})
	if err != nil {
		return nil, fmt.Errorf("resolve spec: %v: %w", tref.Format(c.GetRef()), err)
	}

	if computed && !tref.Equal(spec.GetRef(), c.GetRef()) {
		hlog.From(ctx).Warn(fmt.Sprintf("%v resolved as %v", tref.Format(c.GetRef()), tref.Format(spec.GetRef())))
	}

	return spec, nil
}

const querySrcTargetArg = "src_target"

func (e *Engine) resolveSpecQuery(ctx context.Context, rs *RequestState, ref *pluginv1.TargetRef) (*pluginv1.TargetSpec, error) {
	if ref.GetArgs()[querySrcTargetArg] == "" {
		return nil, fmt.Errorf("%v required", querySrcTargetArg)
	}

	var items []*pluginv1.TargetMatcher

	qo, err := tref.ParseQuery(ref)
	if err != nil {
		return nil, err
	}

	if label := qo.Label; label != "" {
		items = append(items, tmatch.Label(label))
	}

	if pkg := qo.Package; pkg != "" {
		items = append(items, tmatch.Package(pkg))
	}

	if pkg := qo.PackagePrefix; pkg != "" {
		items = append(items, tmatch.PackagePrefix(pkg))
	}

	if treeOutputTo := qo.TreeOutputTo; treeOutputTo != "" {
		items = append(items, pluginv1.TargetMatcher_builder{CodegenPackage: proto.String(treeOutputTo)}.Build())
	}

	skipProvider := qo.SkipProvider // TODO: can be automated by annotating the RequestState for List/Get

	if len(items) == 0 {
		return nil, errors.New("invalid query: empty selection")
	}

	var deps []*structpb.Value //nolint:prealloc
	for qref, err := range e.query(ctx, rs, tmatch.And(items...), queryOptions{
		filterProvider: func(p EngineProvider) bool {
			if skipProvider == "" {
				return true
			}

			return p.Name != skipProvider
		},
	}) {
		if err != nil {
			return nil, err
		}

		deps = append(deps, structpb.NewStringValue(tref.Format(qref)))
	}

	return pluginv1.TargetSpec_builder{
		Ref:    ref,
		Driver: htypes.Ptr(plugingroup.Name),
		Config: map[string]*structpb.Value{
			"deps": structpb.NewListValue(&structpb.ListValue{
				Values: deps,
			}),
		},
	}.Build(), nil
}

func (e *Engine) resolveHostBin(ctx context.Context, rs *RequestState, ref *pluginv1.TargetRef) (*pluginv1.TargetSpec, error) {
	return pluginv1.TargetSpec_builder{
		Ref:    tref.New(ref.GetPackage(), ref.GetName(), nil),
		Driver: htypes.Ptr(pluginbin.Name),
		Config: map[string]*structpb.Value{
			"name": structpb.NewStringValue(ref.GetName()),
		},
	}.Build(), nil
}

func dagAddParent(rs *RequestState, ref *pluginv1.TargetRef) (*RequestState, error) {
	if rs.parent != nil {
		err := rs.dag.AddVertex(rs.parent)
		if err != nil && !hdag.IsDuplicateVertexError(err) {
			return rs, err
		}

		err = rs.dag.AddVertex(ref)
		if err != nil && !hdag.IsDuplicateVertexError(err) {
			return rs, err
		}

		err = rs.dag.AddEdgeMeta(ref, rs.parent, "resolve")
		if err != nil && !hdag.IsDuplicateEdgeError(err) {
			if errors.As(err, &dag.EdgeLoopError{}) {
				return rs, NewStackRecursionError(func() string {
					return err.Error()
				})
			}
			if errors.As(err, &dag.SrcDstEqualError{}) {
				return rs, NewStackRecursionError(func() string {
					return err.Error()
				})
			}

			return rs, err
		}
	}

	rs = rs.WithParent(ref)

	return rs, nil
}

func (e *Engine) GetSpec(ctx context.Context, rs *RequestState, c SpecContainer) (*pluginv1.TargetSpec, error) {
	rs, err := dagAddParent(rs, c.GetRef())
	if err != nil {
		return nil, err
	}

	clean := e.StoreRequestState(rs)
	defer clean()

	return e.getSpec(ctx, rs, c)
}

func (e *Engine) getSpec(ctx context.Context, rs *RequestState, c SpecContainer) (*pluginv1.TargetSpec, error) {
	ctx, cleanLabels := hdebug.SetLabels(ctx, func() []string {
		return []string{"where", "GetSpec " + tref.Format(c.GetRef())}
	})
	defer cleanLabels()

	ctx, span := tracer.Start(ctx, "GetSpec", trace.WithAttributes(attribute.String("target", tref.Format(c.GetRef()))))
	defer span.End()

	if c.Spec != nil {
		return c.Spec, nil
	}

	var pkg string
	if c.GetRef() != nil {
		pkg = c.GetRef().GetPackage()
	} else {
		return nil, errors.New("spec or ref must be specified")
	}

	states, err := e.ProbeSegments(ctx, rs, c, pkg)
	if err != nil {
		return nil, err
	}

	return e.resolveSpec(ctx, rs, states, c)
}

func (e *Engine) ProbeSegments(ctx context.Context, rs *RequestState, c SpecContainer, pkg string) ([]*pluginv1.ProviderState, error) {
	return nil, nil

	// ctx, span := tracer.Start(ctx, "ProbeSegments", trace.WithAttributes(attribute.String("target", tref.Format(c.GetRef()))))
	// defer span.End()
	//
	//// TODO: errgroup to parallelize probing
	//
	// var states []*pluginv1.ProviderState
	// segments := strings.Split(pkg, string(filepath.Separator))
	// if len(segments) == 0 || len(segments) > 1 && segments[0] != "" {
	//	// make sure to always probe root
	//	segments = slices.Insert(segments, 0, "")
	//}
	// for i := range segments {
	//	probePkg := path.Join(segments[:i]...)
	//
	//	for ip, p := range e.Providers {
	//		probeStates, err, _ := rs.memProbe.Do(ctx, fmt.Sprintf("%v %v", ip, probePkg), func(ctx context.Context) ([]*pluginv1.ProviderState, error) {
	//			res, err := p.Probe(ctx, &pluginv1.ProbeRequest{
	//				RequestId: rs.ID,
	//				Package:   probePkg,
	//			})
	//			if err != nil {
	//				return nil, err
	//			}
	//
	//			return res.GetStates(), nil
	//		})
	//		if err != nil {
	//			return nil, err
	//		}
	//
	//		states = append(states, probeStates...)
	//	}
	//}
	//
	// return states, nil
}

type Refish interface {
	GetPackage() string
	GetName() string
}

type DefContainer struct {
	Ref  *pluginv1.TargetRef
	Spec *pluginv1.TargetSpec
	Def  *pluginv1.TargetDef
}

func (c DefContainer) GetRef() *pluginv1.TargetRef {
	if c.Def != nil {
		return c.Def.GetRef()
	}
	if c.Spec != nil {
		return c.Spec.GetRef()
	}
	if c.Ref != nil {
		return c.Ref
	}

	panic("ref, spec or def must be specified")
}

type TargetDef struct {
	*pluginv1.TargetDef
	*pluginv1.TargetSpec

	AppliedTransitive *pluginv1.Sandbox
}

func refKey(ref *pluginv1.TargetRef) string {
	return tref.Format(ref)
}

func (t TargetDef) GetRef() *pluginv1.TargetRef {
	return t.TargetSpec.GetRef()
}
func (t TargetDef) OutputNames() []string {
	outputs := make([]string, 0, len(t.GetOutputs()))
	for _, output := range t.GetOutputs() {
		outputs = append(outputs, output.GetGroup())
	}

	return outputs
}

func (e *Engine) GetDef(ctx context.Context, rs *RequestState, c DefContainer) (*TargetDef, error) {
	rs, err := dagAddParent(rs, c.GetRef())
	if err != nil {
		return nil, err
	}

	clean := e.StoreRequestState(rs)
	defer clean()

	return e.getDef(ctx, rs, c)
}

func (e *Engine) getDef(ctx context.Context, rs *RequestState, c DefContainer) (*TargetDef, error) {
	ctx, span := tracer.Start(ctx, "GetDef", trace.WithAttributes(attribute.String("target", tref.Format(c.GetRef()))))
	defer span.End()

	// put back when we have custom ids
	// step, ctx := hstep.New(ctx, "Getting definition...")
	// defer step.Done()

	ref := c.Ref
	if c.Def != nil {
		ref = c.GetRef()
	}

	spec, err := e.getSpec(ctx, rs, SpecContainer{
		Ref:  ref,
		Spec: c.Spec,
	})
	if err != nil {
		return nil, err
	}

	res, err, _ := rs.memDef.Do(ctx, refKey(c.GetRef()), func(ctx context.Context) (*TargetDef, error) {
		driver, ok := e.DriversByName[spec.GetDriver()]
		if !ok {
			matches := fuzzy.RankFindNormalizedFold(spec.GetDriver(), slices.Collect(maps.Keys(e.DriversByName)))
			slices.SortFunc(matches, func(a, b fuzzy.Rank) int {
				return cmp.Compare(a.Distance, b.Distance)
			})

			if len(matches) > 0 {
				return nil, fmt.Errorf("driver %q doesnt exist, did you mean %q?", spec.GetDriver(), matches[0].Target)
			}

			return nil, fmt.Errorf("driver %q doesnt exist", spec.GetDriver())
		}

		res, err := driver.Parse(ctx, pluginv1.ParseRequest_builder{
			RequestId: htypes.Ptr(rs.ID),
			Spec:      spec,
		}.Build())
		if err != nil {
			return nil, fmt.Errorf("parse: %q: %w", spec.GetDriver(), err)
		}

		def := res.GetTarget()

		if spec.GetDriver() == plugingroup.Name {
			def, err = e.getDefGroup(ctx, rs, def)
			if err != nil {
				return nil, err
			}
		}

		if !tref.Equal(def.GetRef(), spec.GetRef()) {
			return nil, fmt.Errorf("mismatch def ref %v %v", tref.Format(def.GetRef()), tref.Format(spec.GetRef()))
		}

		currentTargetAddrHash := sync.OnceValue(func() string {
			h := xxh3.New()
			hashpb.Hash(h, def.GetRef(), tref.OmitHashPb)

			return hex.EncodeToString(h.Sum(nil))
		})

		addSrcTargetToInputs(def, currentTargetAddrHash)

		for _, output := range def.GetOutputs() {
			for _, path := range output.GetPaths() {
				var pathStr string
				switch path.WhichContent() {
				case pluginv1.TargetDef_Output_Path_FilePath_case:
					pathStr = path.GetFilePath()
				case pluginv1.TargetDef_Output_Path_DirPath_case:
					pathStr = path.GetDirPath()
				case pluginv1.TargetDef_Output_Path_Glob_case:
					pathStr = path.GetGlob() // TODO: may need more brain

					if path.GetCodegenTree() != pluginv1.TargetDef_Output_Path_CODEGEN_MODE_UNSPECIFIED {
						return nil, fmt.Errorf("codegen tree: %v: cannot be a glob", pathStr)
					}
				default:
					return nil, fmt.Errorf("unexpected path type: %v", path.WhichContent())
				}

				if pathStr == ".." || strings.Contains(pathStr, "../") {
					return nil, errors.New("output path cannot go to the parent folder")
				}
			}
		}

		allTransitive, err := e.collectTransitive(ctx, rs, def.GetInputs())
		if err != nil {
			return nil, fmt.Errorf("collect transitive: %w", err)
		}

		if spec.GetDriver() != plugingroup.Name && !sandboxSpecEmpty(allTransitive) {
			res, err := driver.ApplyTransitive(ctx, pluginv1.ApplyTransitiveRequest_builder{
				RequestId:  htypes.Ptr(rs.ID),
				Target:     def,
				Transitive: allTransitive,
			}.Build())
			if err != nil {
				return nil, fmt.Errorf("apply transitive: %w", err)
			}

			def = res.GetTarget()

			addSrcTargetToInputs(def, currentTargetAddrHash)
		}

		if len(def.GetHash()) == 0 {
			h := xxh3.New()
			hashpb.Hash(h, def, tref.OmitHashPb)

			def.SetHash(h.Sum(nil))
		}

		return &TargetDef{
			TargetDef:         def,
			TargetSpec:        spec,
			AppliedTransitive: allTransitive,
		}, nil
	})

	return res, err
}

func addSrcTargetToInputs(def *pluginv1.TargetDef, currentTargetAddrHash func() string) {
	inputs := def.GetInputs()
	for _, input := range inputs {
		refo := input.GetRef()
		ref := refo.GetTarget()

		if ref.GetPackage() != tref.QueryPackage {
			continue
		}

		ref = tref.WithArg(ref, querySrcTargetArg, currentTargetAddrHash())
		refo.SetTarget(ref)
		refo.ClearHash()
	}
	def.SetInputs(inputs)
}

func sandboxSpecEmpty(sb *pluginv1.Sandbox) bool {
	return len(sb.GetTools()) == 0 && len(sb.GetDeps()) == 0 && len(sb.GetEnv()) == 0
}

func (e *Engine) collectTransitive(ctx context.Context, rs *RequestState, inputs []*pluginv1.TargetDef_Input) (*pluginv1.Sandbox, error) {
	sb := &pluginv1.Sandbox{}

	for i, input := range inputs {
		spec, err := e.getSpec(ctx, rs, SpecContainer{Ref: tref.WithoutOut(input.GetRef())})
		if err != nil {
			return nil, fmt.Errorf("get spec %v: %w", tref.FormatOut(input.GetRef()), err)
		}

		transitive := spec.GetTransitive()
		if spec.GetDriver() == plugingroup.Name {
			def, err := e.GetDef(ctx, rs, DefContainer{Spec: spec})
			if err != nil {
				return nil, fmt.Errorf("get def %v: %w", tref.FormatOut(input.GetRef()), err)
			}

			if !sandboxSpecEmpty(def.AppliedTransitive) {
				transitive = hproto.Clone(transitive)
				mergeSandbox(transitive, def.AppliedTransitive, "group")
			}
		}

		if sandboxSpecEmpty(transitive) {
			continue
		}

		h := xxh3.New()
		hashpb.Hash(h, spec.GetRef(), tref.OmitHashPb)

		id := fmt.Sprintf("_transitive_%s_%v", hex.EncodeToString(h.Sum(nil)), i)

		mergeSandbox(sb, transitive, id)
	}

	slices.SortFunc(sb.GetDeps(), func(a, b *pluginv1.Sandbox_Dep) int {
		return cmp.Compare(a.GetId(), b.GetId())
	})
	slices.SortFunc(sb.GetTools(), func(a, b *pluginv1.Sandbox_Tool) int {
		return cmp.Compare(a.GetId(), b.GetId())
	})

	return sb, nil
}

func mergeSandbox(dst, src *pluginv1.Sandbox, id string) {
	for _, tool := range src.GetTools() {
		tool = hproto.Clone(tool)
		tool.SetId(fmt.Sprintf("%v_tool_%v", id, tool.GetId()))
		tool.SetGroup(tool.GetId())

		dst.SetTools(append(dst.GetTools(), tool))
	}
	for _, dep := range src.GetDeps() {
		dep = hproto.Clone(dep)
		dep.SetId(fmt.Sprintf("%v_dep_%v", id, dep.GetId()))
		dep.SetGroup(dep.GetId())

		dst.SetDeps(append(dst.GetDeps(), dep))
	}

	allEnv := dst.GetEnv()
	if allEnv == nil {
		allEnv = map[string]*pluginv1.Sandbox_Env{}
	}
	for k, env := range src.GetEnv() {
		// TODO append
		allEnv[k] = env
	}
	dst.SetEnv(allEnv)
}

type LinkedTarget struct {
	*pluginv1.TargetDef
	Deps []*LinkedTarget
}

type LightLinkedTargetInput struct {
	*TargetDef
	Origin  *pluginv1.TargetDef_InputOrigin
	Outputs []string
}

type LightLinkedTarget struct {
	*TargetDef
	Inputs []*LightLinkedTargetInput
}

func (t LightLinkedTarget) Clone() *LightLinkedTarget {
	return &LightLinkedTarget{
		TargetDef: t.TargetDef,
		Inputs:    slices.Clone(t.Inputs),
	}
}

func (e *Engine) Link(ctx context.Context, rs *RequestState, c DefContainer) (*LightLinkedTarget, error) {
	rs, err := dagAddParent(rs, c.GetRef())
	if err != nil {
		return nil, err
	}

	clean := e.StoreRequestState(rs)
	defer clean()

	return e.link(ctx, rs, c)
}

func (e *Engine) link(ctx context.Context, rs *RequestState, c DefContainer) (*LightLinkedTarget, error) {
	ctx, cleanLabels := hdebug.SetLabels(ctx, func() []string {
		return []string{
			"where", fmt.Sprintf("Link %v", tref.Format(c.GetRef())),
		}
	})
	defer cleanLabels()

	def, err := e.getDef(ctx, rs, c)
	if err != nil {
		return nil, err
	}

	ldef, err, _ := rs.memLink.Do(ctx, refKey(c.GetRef()), func(ctx context.Context) (*LightLinkedTarget, error) {
		return e.innerLink(ctx, rs, def)
	})

	return ldef, err
}

type DAGType int

const (
	DAGTypeAll DAGType = iota
	DAGTypeAncestors
	DAGTypeDescendants
)

func (e *Engine) DAG(ctx context.Context, rs *RequestState, ref *pluginv1.TargetRef, t DAGType, scope *pluginv1.TargetMatcher) (*DAG, error) {
	switch t {
	case DAGTypeAncestors:
		err := e.QueryLink(ctx, rs, tmatch.And(scope, tmatch.Ref(ref)), scope)
		if err != nil {
			return nil, err
		}

		return rs.dag.GetAncestorsGraph(ctx, ref)
	case DAGTypeAll:
		err := e.QueryLink(ctx, rs, tmatch.And(scope, tmatch.All()), scope)
		if err != nil {
			return nil, err
		}

		return rs.dag.GetGraph(ctx, ref)
	case DAGTypeDescendants:
		err := e.QueryLink(ctx, rs, tmatch.And(scope, tmatch.All()), scope)
		if err != nil {
			return nil, err
		}

		return rs.dag.GetDescendantsGraph(ctx, ref)
	default:
		return nil, fmt.Errorf("unknown dag type %v", t)
	}
}

func (e *Engine) QueryLink(ctx context.Context, rs *RequestState, m, deepMatcher *pluginv1.TargetMatcher) error {
	var dedup sync_map.Map[string, htypes.Void]
	var g herrgroup.Group

	var link func(ref *tref.Ref) error
	link = func(ref *tref.Ref) error {
		if _, loaded := dedup.LoadOrStore(tref.Format(ref), htypes.Void{}); loaded {
			return nil
		}

		lldef, err := e.Link(ctx, rs, DefContainer{Ref: ref})
		if err != nil {
			return err
		}

		if tmatch.MatchDef(lldef.TargetSpec, lldef.TargetDef.TargetDef, deepMatcher) == tmatch.MatchNo {
			return nil
		}

		for _, def := range lldef.Inputs {
			g.Go(func() error {
				return link(def.GetRef())
			})
		}

		return nil
	}

	for ref, err := range e.Query(ctx, rs, m) {
		if err != nil {
			return err
		}

		g.Go(func() error {
			return link(ref)
		})
	}

	return g.Wait()
}

func (e *Engine) Validate(ctx context.Context, rs *RequestState, m *pluginv1.TargetMatcher) error {
	step, ctx := hstep.New(ctx, "Validating...")
	defer step.Done()

	err := e.QueryLink(ctx, rs, m, tmatch.All())
	if err != nil {
		return err
	}

	codegenPkg := map[string]htypes.Void{}
	for _, ref := range rs.dag.GetVertices() {
		def, err := e.GetDef(ctx, rs, DefContainer{Ref: ref})
		if err != nil {
			return err
		}

		for _, output := range def.GetOutputs() {
			for _, path := range output.GetPaths() {
				if path.GetCodegenTree() == pluginv1.TargetDef_Output_Path_CODEGEN_MODE_UNSPECIFIED {
					continue
				}

				var pkg string
				switch path.WhichContent() {
				case pluginv1.TargetDef_Output_Path_FilePath_case:
					pkg = tref.JoinPackage(ref.GetPackage(), tref.ToPackage(filepath.Dir(path.GetFilePath())))
				case pluginv1.TargetDef_Output_Path_DirPath_case:
					pkg = tref.JoinPackage(ref.GetPackage(), tref.ToPackage(path.GetFilePath()))
				case pluginv1.TargetDef_Output_Path_Glob_case:
					return fmt.Errorf("glob codegen path %q not supported", path.GetGlob())
				default:
					panic("unreachable")
				}

				if _, ok := codegenPkg[pkg]; ok {
					continue
				}
				codegenPkg[pkg] = htypes.Void{}

				for p := range tref.ParentPackages(m.GetPackage()) {
					err := e.QueryLink(ctx, rs, tmatch.CodegenPackage(p), tmatch.None())
					if err != nil {
						return err
					}
				}
			}
		}
	}

	paths := map[string]*pluginv1.TargetRef{}
	dirs := map[string]*pluginv1.TargetRef{}
	for _, ref := range rs.dag.GetVertices() {
		def, err := e.GetDef(ctx, rs, DefContainer{Ref: ref})
		if err != nil {
			return err
		}
		for _, output := range def.GetOutputs() {
			for _, path := range output.GetPaths() {
				if path.GetCodegenTree() == pluginv1.TargetDef_Output_Path_CODEGEN_MODE_UNSPECIFIED {
					continue
				}

				var codegenPath string
				switch path.WhichContent() {
				case pluginv1.TargetDef_Output_Path_FilePath_case:
					codegenPath = filepath.Join(tref.ToOSPath(ref.GetPackage()), path.GetFilePath())
				case pluginv1.TargetDef_Output_Path_DirPath_case:
					codegenPath = filepath.Join(tref.ToOSPath(ref.GetPackage()), path.GetDirPath())
				case pluginv1.TargetDef_Output_Path_Glob_case:
					return fmt.Errorf("glob codegen path %q not supported", path.GetGlob())
				default:
					panic("unreachable")
				}

				if src, ok := paths[codegenPath]; ok {
					return fmt.Errorf("%v: %v already outputs %v", tref.Format(ref), tref.Format(src), codegenPath)
				}

				for p := range hfs.ParentPaths(codegenPath) {
					if src, ok := dirs[p]; ok {
						return fmt.Errorf("%v: %v already outputs %v, cannot output %v because it's in a generated directory", tref.Format(ref), tref.Format(src), p, codegenPath)
					}
				}

				if path.WhichContent() == pluginv1.TargetDef_Output_Path_DirPath_case {
					for p, src := range paths {
						if hfs.HasPathPrefix(p, codegenPath) {
							return fmt.Errorf("%v: %v already outputs %v, it would shadow be shadowed by %v", tref.Format(ref), tref.Format(src), p, codegenPath)
						}
					}

					dirs[codegenPath] = ref
				}

				paths[codegenPath] = ref
			}
		}

	}

	return nil
}

func (e *Engine) innerLink(ctx context.Context, rs *RequestState, def *TargetDef) (*LightLinkedTarget, error) {
	if e.RootSpan.IsRecording() {
		ctx = trace.ContextWithSpan(ctx, e.RootSpan)
	}
	ctx = hstep.WithoutParent(ctx)

	ctx, cleanLabels := hdebug.SetLabels(ctx, func() []string {
		return []string{
			"where", fmt.Sprintf("innerLink %v", tref.Format(def.GetRef())),
		}
	})
	defer cleanLabels()

	ctx, span := tracer.Start(ctx, "Link", trace.WithAttributes(attribute.String("target", tref.Format(def.GetRef()))))
	defer span.End()

	step, ctx := hstep.New(ctx, fmt.Sprintf("Linking %v...", tref.Format(def.GetRef())))
	defer step.Done()

	inputs := def.GetInputs()

	lt := &LightLinkedTarget{
		TargetDef: def,
		Inputs:    make([]*LightLinkedTargetInput, len(inputs)),
	}

	err := rs.dag.AddVertex(def.GetRef())
	if err != nil && !hdag.IsDuplicateVertexError(err) {
		return nil, err
	}

	var g herrgroup.Group
	for i, input := range inputs {
		g.Go(func() error {
			depRef := tref.WithoutOut(input.GetRef())

			err := rs.dag.AddVertex(depRef)
			if err != nil && !hdag.IsDuplicateVertexError(err) {
				return err
			}

			err = rs.dag.AddEdgeMeta(depRef, def.GetRef(), "dep")
			if err != nil && !hdag.IsDuplicateEdgeError(err) {
				if errors.As(err, &dag.EdgeLoopError{}) {
					return NewStackRecursionError(func() string {
						return err.Error()
					})
				}
				if errors.As(err, &dag.SrcDstEqualError{}) {
					return NewStackRecursionError(func() string {
						return err.Error()
					})
				}

				return fmt.Errorf("innerlink: %w", err)
			}

			linkedDep, err := e.GetDef(ctx, rs, DefContainer{Ref: depRef})
			if err != nil {
				return err
			}

			var outputNames []string
			if input.GetRef().HasOutput() {
				if !checkRefOutputExists(linkedDep, input.GetRef()) {
					return fmt.Errorf("%v doesnt have a named output %q", tref.FormatOut(input.GetRef()), input.GetRef().GetOutput())
				}

				outputNames = []string{input.GetRef().GetOutput()}
			} else {
				outputNames = make([]string, 0, len(linkedDep.GetOutputs()))
				for _, output := range linkedDep.GetOutputs() {
					outputNames = append(outputNames, output.GetGroup())
				}
			}

			lt.Inputs[i] = &LightLinkedTargetInput{
				TargetDef: linkedDep,
				Outputs:   outputNames,
				Origin:    input.GetOrigin(),
			}

			return nil
		})
	}

	if err := g.Wait(); err != nil {
		return nil, err
	}

	slices.SortFunc(lt.Inputs, func(a, b *LightLinkedTargetInput) int {
		if v := tref.Compare(a.GetRef(), a.GetRef()); v != 0 {
			return v
		}

		return strings.Compare(a.Origin.GetId(), a.Origin.GetId())
	})

	return lt, nil
}

func (e *Engine) getDefGroup(ctx context.Context, rs *RequestState, def *pluginv1.TargetDef) (*pluginv1.TargetDef, error) {
	type getDefContainer struct {
		input *pluginv1.TargetDef_Input
		def   *TargetDef
	}

	results := make([]getDefContainer, len(def.GetInputs()))

	var g herrgroup.Group
	for i, input := range def.GetInputs() {
		g.Go(func() error {
			linkedInput, err := e.GetDef(ctx, rs, DefContainer{Ref: tref.WithoutOut(input.GetRef())})
			if err != nil {
				return err
			}

			results[i] = getDefContainer{
				input: input,
				def:   linkedInput,
			}

			return nil
		})
	}

	err := g.Wait()
	if err != nil {
		return nil, err
	}

	def.SetOutputs(nil)

	for _, c := range results {
		input := c.input
		linkedInput := c.def

		inputRef := input.GetRef()

		if inputRef.HasOutput() {
			if !checkRefOutputExists(linkedInput, inputRef) {
				return nil, fmt.Errorf("%v doesnt have a named output %q", tref.FormatOut(inputRef), inputRef.GetOutput())
			}

			for _, output := range def.GetOutputs() {
				if output.GetGroup() != inputRef.GetOutput() {
					continue
				}

				def.SetOutputs(append(def.GetOutputs(), output))
			}
		} else {
			def.SetOutputs(append(def.GetOutputs(), linkedInput.GetOutputs()...))
		}
	}

	return def, nil
}

func checkRefOutputExists(linkedInput *TargetDef, inputRef *pluginv1.TargetRefWithOutput) bool {
	return hslices.Has(linkedInput.GetOutputs(), func(output *pluginv1.TargetDef_Output) bool {
		return output.GetGroup() == inputRef.GetOutput()
	})
}
