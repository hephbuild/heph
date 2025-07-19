package engine

import (
	"context"
	"errors"
	"fmt"
	"github.com/google/uuid"
	"github.com/hephbuild/heph/hdebug"
	"github.com/hephbuild/heph/herrgroup"
	"github.com/hephbuild/heph/internal/hcore/hlog"
	"github.com/hephbuild/heph/internal/hcore/hstep"
	"github.com/hephbuild/heph/internal/hproto/hstructpb"
	"github.com/hephbuild/heph/internal/hsingleflight"
	engine2 "github.com/hephbuild/heph/lib/engine"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/plugingroup"
	"github.com/hephbuild/heph/plugin/tref"
	"github.com/zeebo/xxh3"
	"go.opentelemetry.io/otel/attribute"
	"go.opentelemetry.io/otel/trace"
	"google.golang.org/protobuf/types/known/structpb"
	"maps"
	"os"
	"path"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"sync"
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

func (e *Engine) resolveProvider(ctx context.Context, rs *RequestState, states []*pluginv1.ProviderState, c SpecContainer, p EngineProvider) (*pluginv1.TargetSpec, error) {
	providerKey := p.Name

	rs, err := rs.TraceResolveProvider(tref.Format(c.GetRef()), p.Name)
	if err != nil {
		return nil, err
	}

	clean := e.StoreRequestState(rs)
	defer clean()

	spec, err, _ := rs.memSpecGet.Do(ctx, memSpecGetKey{providerName: providerKey, refKey: refKey(c.GetRef())}, func(ctx context.Context) (*pluginv1.TargetSpec, error) {
		res, err := e.Get(ctx, rs, p, c.GetRef(), states)
		if err != nil {
			return nil, err
		}

		return res.GetSpec(), nil
	})
	if err != nil {
		return nil, err
	}

	// TODO: what do we do if the spec.ref is different from the request ?

	return spec, nil

}

func (e *Engine) resolveSpec(ctx context.Context, rs *RequestState, states []*pluginv1.ProviderState, c SpecContainer) (*pluginv1.TargetSpec, error) {
	ctx, span := tracer.Start(ctx, "ResolveSpec", trace.WithAttributes(attribute.String("target", tref.Format(c.GetRef()))))
	defer span.End()

	rs, err := rs.Trace("ResolveSpec", tref.Format(c.GetRef()))
	if err != nil {
		return nil, err
	}

	clean := e.StoreRequestState(rs)
	defer clean()

	spec, err, computed := rs.memSpec.Do(ctx, refKey(c.GetRef()), func(ctx context.Context) (*pluginv1.TargetSpec, error) {
		if ref := c.GetRef(); ref.Package == "@heph/query" {
			items := []*pluginv1.TargetMatcher{}

			if label, ok := ref.Args["label"]; ok {
				items = append(items, &pluginv1.TargetMatcher{Item: &pluginv1.TargetMatcher_Label{Label: label}})
			}

			if treeOutputTo, ok := ref.Args["tree_output_to"]; ok {
				items = append(items, &pluginv1.TargetMatcher{Item: &pluginv1.TargetMatcher_CodegenPackage{CodegenPackage: treeOutputTo}})
			}

			if len(items) == 0 {
				return nil, errors.New("invalid query: empty selection")
			}

			var deps []string
			for ref, err := range e.Query(ctx, rs, &pluginv1.TargetMatcher{Item: &pluginv1.TargetMatcher_And{And: &pluginv1.TargetMatchers{Items: items}}}) {
				if err != nil {
					if errors.Is(err, ErrStackRecursion{}) {
						//hlog.From(ctx).Error("resolve specs query", "err", err)

						continue
					}

					return nil, err
				}

				deps = append(deps, tref.Format(ref))
			}

			return &pluginv1.TargetSpec{
				Ref:    ref,
				Driver: plugingroup.Name,
				Config: map[string]*structpb.Value{
					"deps": hstructpb.NewStringsValue(deps),
				},
			}, nil
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
				if errors.Is(err, engine2.ErrNotFound) {
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

	if computed && !tref.Equal(spec.Ref, c.GetRef()) {
		hlog.From(ctx).Warn(fmt.Sprintf("%v resolved as %v", tref.Format(c.GetRef()), tref.Format(spec.Ref)))
	}

	return spec, nil
}

func (e *Engine) GetSpec(ctx context.Context, rs *RequestState, c SpecContainer) (*pluginv1.TargetSpec, error) {
	ctx, span := tracer.Start(ctx, "GetSpec", trace.WithAttributes(attribute.String("target", tref.Format(c.GetRef()))))
	defer span.End()

	rs, err := rs.Trace("GetSpec", tref.Format(c.GetRef()))
	if err != nil {
		return nil, err
	}

	clean := e.StoreRequestState(rs)
	defer clean()

	if c.Spec != nil {
		return c.Spec, nil
	}

	var pkg string
	if c.Ref != nil {
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

	ctx, span := tracer.Start(ctx, "ProbeSegments", trace.WithAttributes(attribute.String("target", tref.Format(c.GetRef()))))
	defer span.End()

	// TODO: errgroup to parallelize probing

	var states []*pluginv1.ProviderState
	segments := strings.Split(pkg, string(filepath.Separator))
	if len(segments) == 0 || len(segments) > 1 && segments[0] != "" {
		// make sure to always probe root
		segments = slices.Insert(segments, 0, "")
	}
	for i := range segments {
		probePkg := path.Join(segments[:i]...)

		for ip, p := range e.Providers {
			probeStates, err, _ := rs.memProbe.Do(ctx, fmt.Sprintf("%v %v", ip, probePkg), func(ctx context.Context) ([]*pluginv1.ProviderState, error) {
				res, err := p.Probe(ctx, &pluginv1.ProbeRequest{
					RequestId: rs.ID,
					Package:   probePkg,
				})
				if err != nil {
					return nil, err
				}

				return res.States, nil
			})
			if err != nil {
				return nil, err
			}

			states = append(states, probeStates...)
		}
	}

	return states, nil
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

type memSpecGetKey struct {
	providerName, refKey string
}

type RequestStateData struct {
	InteractiveExec func(context.Context, InteractiveExecOptions) error
	Shell           *pluginv1.TargetRef
	Force           *pluginv1.TargetMatcher
	Interactive     *pluginv1.TargetRef

	memSpecGet hsingleflight.GroupMemContext[memSpecGetKey, *pluginv1.TargetSpec]
	memSpec    hsingleflight.GroupMemContext[string, *pluginv1.TargetSpec]
	memProbe   hsingleflight.GroupMemContext[string, []*pluginv1.ProviderState]

	memLink hsingleflight.GroupMemContext[string, *LightLinkedTarget]
	memDef  hsingleflight.GroupMemContext[string, *TargetDef]

	memResult  hsingleflight.GroupMemContext[string, *ExecuteResultLocks]
	memExecute hsingleflight.GroupMemContext[string, *ExecuteResultLocks]
}

type traceStackEntry struct {
	fun string
	id1 string
	id2 string
}

type RequestState struct {
	ID string

	*RequestStateData

	traceStack Stack[traceStackEntry]
}

func (s *RequestState) Trace(fun, id string) (*RequestState, error) {
	return s.traceStackPush(traceStackEntry{fun: fun, id1: id})
}

func (s *RequestState) HasTrace(fun, id string) bool {
	return s.traceStack.Has(traceStackEntry{fun: fun, id1: id})
}

func (s *RequestState) TraceList(name string, pkg string) (*RequestState, error) {
	return s.traceStackPush(traceStackEntry{fun: "List", id1: name, id2: pkg})
}

func (s *RequestState) TraceResolveProvider(format string, name string) (*RequestState, error) {
	return s.traceStackPush(traceStackEntry{fun: "ResolveProvider", id1: format, id2: name})
}

func (s *RequestState) TraceQueryListProvider(format string, name string) (*RequestState, error) {
	return s.traceStackPush(traceStackEntry{fun: "QueryListProvider", id1: format, id2: name})
}

func (s *RequestState) traceStackPush(e traceStackEntry) (*RequestState, error) {
	stack, err := s.traceStack.Push(e)
	if err != nil {
		return nil, err
	}

	return &RequestState{
		ID:               uuid.New().String(),
		RequestStateData: s.RequestStateData,
		traceStack:       stack,
	}, nil
}

type Stack[K comparable] struct {
	m           map[K]*K
	first       *K
	last        *K
	debugString string
}

type ErrStackRecursion struct {
	printer func() string
}

func (e ErrStackRecursion) Error() string {
	return fmt.Sprintf("stack recursion detected: %v", e.printer())
}

func (e ErrStackRecursion) Print() string {
	return e.printer()
}

func (e ErrStackRecursion) Is(err error) bool {
	_, ok := err.(ErrStackRecursion)

	return ok
}

func (s Stack[K]) Has(k K) bool {
	_, ok := s.m[k]

	return ok
}

var enableStackDebug = sync.OnceValue(func() bool {
	v, _ := strconv.ParseBool(os.Getenv("HEPH_DEBUG_STACK"))

	return v
})

func (s Stack[K]) Push(k K) (Stack[K], error) {
	if _, ok := s.m[k]; ok {
		return s, ErrStackRecursion{
			printer: func() string {
				return s.StringWith(" -> ", k)
			},
		}
	}

	s.m = maps.Clone(s.m)
	if s.m == nil {
		s.m = map[K]*K{}
		s.first = &k
	}
	if s.last != nil {
		s.m[*s.last] = &k
	}
	s.m[k] = nil
	s.last = &k

	if enableStackDebug() {
		s.debugString = s.StringWith("\n")
	}

	return s, nil
}

func (s Stack[K]) String() string {
	return s.StringWith(" -> ")
}

func (s Stack[K]) StringWith(sep string, v ...K) string {
	var buf strings.Builder
	next := s.first
	for next != nil {
		current := *next

		if buf.Len() > 0 {
			buf.WriteString(sep)
		}

		fmt.Fprintf(&buf, "%v", current)

		next = s.m[current]
	}

	if len(v) > 0 {
		buf.WriteString(sep)
		fmt.Fprintf(&buf, "%v", v[0])
	}

	return buf.String()
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
	ctx, span := tracer.Start(ctx, "GetDef", trace.WithAttributes(attribute.String("target", tref.Format(c.GetRef()))))
	defer span.End()

	rs, err := rs.Trace("GetDef", tref.Format(c.GetRef()))
	if err != nil {
		return nil, err
	}

	clean := e.StoreRequestState(rs)
	defer clean()

	// put back when we have custom ids
	// step, ctx := hstep.New(ctx, "Getting definition...")
	// defer step.Done()

	if c.Def != nil && c.Spec != nil {
		return &TargetDef{
			TargetDef:  c.Def,
			TargetSpec: c.Spec,
		}, nil
	}

	spec, err := e.GetSpec(ctx, rs, SpecContainer{
		Ref:  c.Ref,
		Spec: c.Spec,
	})
	if err != nil {
		return nil, err
	}

	res, err, _ := rs.memDef.Do(ctx, refKey(c.GetRef()), func(ctx context.Context) (*TargetDef, error) {
		driver, ok := e.DriversByName[spec.GetDriver()]
		if !ok {
			return nil, fmt.Errorf("driver %q doesnt exist", spec.GetDriver())
		}

		res, err := driver.Parse(ctx, &pluginv1.ParseRequest{
			RequestId: rs.ID,
			Spec:      spec,
		})
		if err != nil {
			return nil, err
		}

		def := res.GetTarget()

		if !tref.Equal(def.Ref, spec.Ref) {
			return nil, fmt.Errorf("mismatch def ref %v %v", tref.Format(def.Ref), tref.Format(spec.Ref))
		}

		for _, output := range def.CollectOutputs {
			if !slices.Contains(def.Outputs, output.Group) {
				def.Outputs = append(def.Outputs, output.Group)
			}
		}

		if len(def.Hash) == 0 {
			h := xxh3.New()
			def.HashPB(h, nil)

			def.Hash = h.Sum(nil)
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
	ctx, cleanLabels := hdebug.SetLabels(ctx, func() []string {
		return []string{
			"where", fmt.Sprintf("Link %v", tref.Format(c.GetRef())),
		}
	})
	defer cleanLabels()

	rs, err := rs.Trace("Link", tref.Format(c.GetRef()))
	if err != nil {
		return nil, err
	}

	clean := e.StoreRequestState(rs)
	defer clean()

	def, err := e.GetDef(ctx, rs, c)
	if err != nil {
		return nil, err
	}

	ldef, err, _ := rs.memLink.Do(ctx, refKey(c.GetRef()), func(ctx context.Context) (*LightLinkedTarget, error) {
		return e.innerLink(ctx, rs, def)
	})

	return ldef, err
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

	var sf hsingleflight.GroupMem[string, *TargetDef]
	var g herrgroup.Group
	for i, input := range inputs {
		depRef := tref.WithoutOut(input.GetRef())

		g.Go(func() error {
			linkedDep, err, _ := sf.Do(refKey(depRef), func() (*TargetDef, error) {
				return e.GetDef(ctx, rs, DefContainer{Ref: depRef})
			})
			if err != nil {
				return err
			}

			outputs := linkedDep.Outputs

			if input.GetRef().Output != nil {
				if !slices.Contains(linkedDep.Outputs, input.GetRef().GetOutput()) {
					return fmt.Errorf("%v doesnt have a named output %q", tref.Format(input.GetRef()), input.GetRef().GetOutput())
				}

				outputs = []string{input.GetRef().GetOutput()}
			}

			lt.Inputs[i] = &LightLinkedTargetInput{
				TargetDef: linkedDep,
				Outputs:   outputs,
				Origin:    input.Origin,
			}

			return nil
		})
	}

	if err := g.Wait(); err != nil {
		return nil, err
	}

	slices.SortFunc(lt.Inputs, func(a, b *LightLinkedTargetInput) int {
		if v := tref.Compare(a.TargetDef.GetRef(), a.TargetDef.GetRef()); v != 0 {
			return v
		}

		return strings.Compare(a.Origin.Id, a.Origin.Id)
	})

	return lt, nil
}
