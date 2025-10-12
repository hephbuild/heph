package engine

import (
	"cmp"
	"context"
	"encoding/hex"
	"errors"
	"fmt"
	"maps"
	"slices"
	"strings"
	"sync"

	"github.com/heimdalr/dag"
	"github.com/hephbuild/heph/internal/hdag"
	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/internal/hproto/hashpb"
	"github.com/hephbuild/heph/internal/htypes"
	"github.com/hephbuild/heph/internal/tmatch"
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
		if ref := c.GetRef(); ref.GetPackage() == tref.QueryPackage {
			return e.resolveSpecQuery(ctx, rs, ref)
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
		return nil, errors.New(querySrcTargetArg + " required")
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
}

func refKey(ref *pluginv1.TargetRef) string {
	return tref.Format(ref)
}

func (t TargetDef) GetRef() *pluginv1.TargetRef {
	return t.TargetSpec.GetRef()
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

	if c.Def != nil && c.Spec != nil {
		return &TargetDef{
			TargetDef:  c.Def,
			TargetSpec: c.Spec,
		}, nil
	}

	spec, err := e.getSpec(ctx, rs, SpecContainer{
		Ref:  c.Ref,
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

		if !tref.Equal(def.GetRef(), spec.GetRef()) {
			return nil, fmt.Errorf("mismatch def ref %v %v", tref.Format(def.GetRef()), tref.Format(spec.GetRef()))
		}

		currentTargetAddrHash := sync.OnceValue(func() string {
			h := xxh3.New()
			hashpb.Hash(h, def.GetRef(), nil)

			return hex.EncodeToString(h.Sum(nil))
		})

		inputs := def.GetInputs()
		for _, input := range inputs {
			if input.GetRef().GetPackage() != tref.QueryPackage {
				continue
			}

			ref := input.GetRef()
			args := ref.GetArgs()
			if args == nil {
				args = map[string]string{}
			}
			args[querySrcTargetArg] = currentTargetAddrHash()
			ref.SetArgs(args)

			input.SetRef(ref)
		}
		def.SetInputs(inputs)

		for _, output := range def.GetCollectOutputs() {
			if !slices.Contains(def.GetOutputs(), output.GetGroup()) {
				def.SetOutputs(append(def.GetOutputs(), output.GetGroup()))
			}

			for _, path := range output.GetPaths() {
				if path == ".." || strings.Contains(path, "../") {
					return nil, errors.New("output path cannot go to the parent folder")
				}
			}
		}

		for i, gen := range def.GetCodegenTree() {
			if hfs.IsGlob(gen.GetPath()) {
				return nil, fmt.Errorf("codegen tree: %v: cannot be a glob", i)
			}

			if gen.GetPath() == ".." || strings.Contains(gen.GetPath(), "../") {
				return nil, errors.New("output path cannot go to the parent folder")
			}
		}

		if len(def.GetHash()) == 0 {
			h := xxh3.New()
			hashpb.Hash(h, def, nil)

			if x := h.Sum(nil); x != nil {
				def.SetHash(x)
			} else {
				def.ClearHash()
			}
		}

		return &TargetDef{
			TargetDef:  def,
			TargetSpec: spec,
		}, nil
	})

	return res, err
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

func (e *Engine) innerLink(ctx context.Context, rs *RequestState, def *TargetDef) (*LightLinkedTarget, error) {
	ctx = trace.ContextWithSpan(ctx, e.RootSpan)
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

			outputs := linkedDep.GetOutputs()

			if input.GetRef().HasOutput() {
				if !slices.Contains(linkedDep.GetOutputs(), input.GetRef().GetOutput()) {
					return fmt.Errorf("%v doesnt have a named output %q", tref.Format(input.GetRef()), input.GetRef().GetOutput())
				}

				outputs = []string{input.GetRef().GetOutput()}
			}

			lt.Inputs[i] = &LightLinkedTargetInput{
				TargetDef: linkedDep,
				Outputs:   outputs,
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
