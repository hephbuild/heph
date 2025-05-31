package engine

import (
	"context"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/internal/hcore/hlog"
	"github.com/hephbuild/heph/internal/hcore/hstep"
	"github.com/hephbuild/heph/internal/hsingleflight"
	engine2 "github.com/hephbuild/heph/lib/engine"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/tref"
	"go.opentelemetry.io/otel/attribute"
	"go.opentelemetry.io/otel/trace"
	"golang.org/x/sync/errgroup"
	"path"
	"path/filepath"
	"slices"
	"strings"
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

func (e *Engine) resolveProvider(ctx context.Context, states []*pluginv1.ProviderState, c SpecContainer, rs *RequestState, p EngineProvider) (*pluginv1.TargetSpec, error) {
	providerKey := p.Name

	// TODO: caching must be smarter, probably package-based ?
	specs, err, _ := rs.memSpecs.Do(providerKey+refKey(c.GetRef()), func() ([]*pluginv1.TargetSpec, error) {
		strm, err := p.GetSpecs(ctx, &pluginv1.GetSpecsRequest{
			RequestId: rs.ID,
			Ref:       c.Ref,
			States:    states,
		})
		if err != nil {
			if errors.Is(err, engine2.ErrNotImplemented) {
				return nil, nil
			}

			return nil, err
		}
		defer strm.CloseReceive()

		var specs []*pluginv1.TargetSpec
		for strm.Receive() {
			switch res := strm.Msg().GetOf().(type) {
			case *pluginv1.GetSpecsResponse_Spec:
				// TODO: what do we do if the spec.ref is different from the request ?

				if !tref.Equal(res.Spec.GetRef(), c.GetRef()) {
					rs.memRef.Set(refKey(res.Spec.GetRef()), res.Spec, nil)
				}

				specs = append(specs, res.Spec)
			}
		}
		if err := strm.Err(); err != nil {
			return nil, err
		}

		return specs, nil
	})
	if err != nil {
		return nil, err
	}

	for _, spec := range specs {
		if tref.Equal(spec.GetRef(), c.GetRef()) {
			return spec, nil
		}
	}

	spec, err, _ := rs.memSpecGet.Do(providerKey+refKey(c.GetRef()), func() (*pluginv1.TargetSpec, error) {
		res, err := p.Get(ctx, &pluginv1.GetRequest{
			RequestId: rs.ID,
			Ref:       c.GetRef(),
			States:    states,
		})
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

func (e *Engine) ResolveSpec(ctx context.Context, states []*pluginv1.ProviderState, c SpecContainer, rs *RequestState) (*pluginv1.TargetSpec, error) {
	ctx, span := tracer.Start(ctx, "ResolveSpec", trace.WithAttributes(attribute.String("target", tref.Format(c.GetRef()))))
	defer span.End()

	spec, err, _ := rs.memRef.Do(refKey(c.GetRef()), func() (*pluginv1.TargetSpec, error) {
		for _, p := range e.Providers {
			var providerStates []*pluginv1.ProviderState
			for _, state := range states {
				if state.GetProvider() == p.Name {
					providerStates = append(providerStates, state)
				}
			}

			spec, err := e.resolveProvider(ctx, providerStates, c, rs, p)
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

	if !tref.Equal(spec.Ref, c.GetRef()) {
		hlog.From(ctx).Warn(fmt.Sprintf("%v resolved as %v", tref.Format(c.GetRef()), tref.Format(spec.Ref)))
	}

	return spec, nil
}

func (e *Engine) GetSpec(ctx context.Context, c SpecContainer, rs *RequestState) (*pluginv1.TargetSpec, error) {
	ctx, span := tracer.Start(ctx, "GetSpec", trace.WithAttributes(attribute.String("target", tref.Format(c.GetRef()))))
	defer span.End()

	if c.Spec != nil {
		return c.Spec, nil
	}

	var pkg string
	if c.Ref != nil {
		pkg = c.GetRef().GetPackage()
	} else {
		return nil, errors.New("spec or ref must be specified")
	}

	states, err := e.ProbeSegments(ctx, c, pkg, rs)
	if err != nil {
		return nil, err
	}

	return e.ResolveSpec(ctx, states, c, rs)
}

func (e *Engine) ProbeSegments(ctx context.Context, c SpecContainer, pkg string, rs *RequestState) ([]*pluginv1.ProviderState, error) {
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
			probeStates, err, _ := rs.memProbe.Do(fmt.Sprintf("%v %v", ip, probePkg), func() ([]*pluginv1.ProviderState, error) {
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

type RequestState struct {
	ID string

	memSpecs   hsingleflight.GroupMem[[]*pluginv1.TargetSpec]
	memSpecGet hsingleflight.GroupMem[*pluginv1.TargetSpec]
	memRef     hsingleflight.GroupMem[*pluginv1.TargetSpec]
	memProbe   hsingleflight.GroupMem[[]*pluginv1.ProviderState]

	memLink hsingleflight.GroupMem[*LightLinkedTarget]
	memDef  hsingleflight.GroupMem[*TargetDef]

	memResult  hsingleflight.GroupMem[*ExecuteResultLocks]
	memExecute hsingleflight.GroupMem[*ExecuteResultLocks]
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

func (e *Engine) GetDef(ctx context.Context, c DefContainer, rs *RequestState) (*TargetDef, error) {
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

	res, err, _ := rs.memDef.Do(refKey(c.GetRef()), func() (*TargetDef, error) {
		spec, err := e.GetSpec(ctx, SpecContainer{
			Ref:  c.Ref,
			Spec: c.Spec,
		}, rs)
		if err != nil {
			return nil, err
		}

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

func (e *Engine) Link(ctx context.Context, c DefContainer, rs *RequestState) (*LightLinkedTarget, error) {
	ctx = trace.ContextWithSpan(ctx, e.RootSpan)
	ctx = hstep.WithoutParent(ctx)

	ctx, span := tracer.Start(ctx, "Link", trace.WithAttributes(attribute.String("target", tref.Format(c.GetRef()))))
	defer span.End()

	step, ctx := hstep.New(ctx, fmt.Sprintf("Linking %v...", tref.Format(c.GetRef())))
	defer step.Done()

	def, err := e.GetDef(ctx, c, rs)
	if err != nil {
		return nil, err
	}

	lt := &LightLinkedTarget{
		TargetDef: def,
		Inputs:    make([]*LightLinkedTargetInput, len(def.GetInputs())),
	}

	var sf hsingleflight.GroupMem[*TargetDef]
	var g errgroup.Group
	for i, input := range def.GetInputs() {
		depRef := tref.WithoutOut(input.GetRef())

		g.Go(func() error {
			linkedDep, err, _ := sf.Do(refKey(depRef), func() (*TargetDef, error) {
				return e.GetDef(ctx, DefContainer{Ref: depRef}, rs)
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
