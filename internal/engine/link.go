package engine

import (
	"context"
	"errors"
	"fmt"
	"github.com/google/uuid"
	"github.com/hephbuild/heph/internal/hcore/hlog"
	"path"
	"path/filepath"
	"slices"
	"strings"
	"sync"

	"connectrpc.com/connect"
	"github.com/hephbuild/heph/internal/hcore/hstep"
	"github.com/hephbuild/heph/internal/hsingleflight"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/tref"
	"go.opentelemetry.io/otel/attribute"
	"go.opentelemetry.io/otel/trace"
	"golang.org/x/sync/errgroup"
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

func (e *Engine) resolveProvider(ctx context.Context, states []*pluginv1.ProviderState, c SpecContainer, rc *ResolveCache, p Provider) (*pluginv1.TargetSpec, error) {
	providerKey := p.Name

	// TODO: caching must be smarter, probably package-based ?
	specs, err, _ := rc.memSpecs.Do(providerKey+tref.Format(c.GetRef()), func() ([]*pluginv1.TargetSpec, error) {
		strm, err := p.GetSpecs(ctx, connect.NewRequest(&pluginv1.GetSpecsRequest{
			Ref:    c.Ref,
			States: states,
		}))
		if err != nil {
			return nil, err
		}
		defer strm.Close()

		var specs []*pluginv1.TargetSpec
		for strm.Receive() {
			switch res := strm.Msg().GetOf().(type) {
			case *pluginv1.GetSpecsResponse_Spec:
				// TODO: what do we do if the spec.ref is different from the request ?

				if !tref.Equal(res.Spec.GetRef(), c.GetRef()) {
					rc.memRef.Set(tref.Format(res.Spec.GetRef()), res.Spec, nil)
				}

				specs = append(specs, res.Spec)
			}
		}
		if err = strm.Err(); err != nil {
			if connect.CodeOf(err) == connect.CodeUnimplemented {
				return nil, nil
			}

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

	spec, err, _ := rc.memSpecGet.Do(providerKey+tref.Format(c.GetRef()), func() (*pluginv1.TargetSpec, error) {
		res, err := p.Get(ctx, connect.NewRequest(&pluginv1.GetRequest{
			Ref:    c.GetRef(),
			States: states,
		}))
		if err != nil {
			return nil, err
		}

		return res.Msg.GetSpec(), nil
	})
	if err != nil {
		return nil, err
	}

	// TODO: what do we do if the spec.ref is different from the request ?

	return spec, nil

}

func (e *Engine) ResolveSpec(ctx context.Context, states []*pluginv1.ProviderState, c SpecContainer, rc *ResolveCache) (*pluginv1.TargetSpec, error) {
	ctx, span := tracer.Start(ctx, "ResolveSpec", trace.WithAttributes(attribute.String("target", tref.Format(c.GetRef()))))
	defer span.End()

	spec, err, _ := rc.memRef.Do(tref.Format(c.GetRef()), func() (*pluginv1.TargetSpec, error) {
		for _, p := range e.Providers {
			var providerStates []*pluginv1.ProviderState
			for _, state := range states {
				if state.GetProvider() == p.Name {
					providerStates = append(providerStates, state)
				}
			}

			spec, err := e.resolveProvider(ctx, providerStates, c, rc, p)
			if err != nil {
				if connect.CodeOf(err) == connect.CodeNotFound {
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

func (e *Engine) GetSpec(ctx context.Context, c SpecContainer, rc *ResolveCache) (*pluginv1.TargetSpec, error) {
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

	states, err := e.ProbeSegments(ctx, c, pkg, rc)
	if err != nil {
		return nil, err
	}

	return e.ResolveSpec(ctx, states, c, rc)
}

func (e *Engine) ProbeSegments(ctx context.Context, c SpecContainer, pkg string, rc *ResolveCache) ([]*pluginv1.ProviderState, error) {
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
			probeStates, err, _ := rc.memProbe.Do(fmt.Sprintf("%v %v", ip, probePkg), func() ([]*pluginv1.ProviderState, error) {
				res, err := p.Probe(ctx, connect.NewRequest(&pluginv1.ProbeRequest{
					Package: probePkg,
				}))
				if err != nil {
					return nil, err
				}

				return res.Msg.States, nil
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

type ResolveCache struct {
	memSpecs   hsingleflight.GroupMem[[]*pluginv1.TargetSpec]
	memSpecGet hsingleflight.GroupMem[*pluginv1.TargetSpec]
	memRef     hsingleflight.GroupMem[*pluginv1.TargetSpec]
	memProbe   hsingleflight.GroupMem[[]*pluginv1.ProviderState]

	memLink hsingleflight.GroupMem[*LightLinkedTarget]
	memDef  hsingleflight.GroupMem[*TargetDef]

	memResult  hsingleflight.GroupMem[*ExecuteResultLocks]
	memExecute hsingleflight.GroupMem[*ExecuteResultLocks]

	correlationId  string
	correlationIdm sync.Mutex
}

func (c *ResolveCache) GetCorrelationId() string {
	c.correlationIdm.Lock()
	defer c.correlationIdm.Unlock()

	if c.correlationId == "" {
		c.correlationId = uuid.New().String()
	}

	return c.correlationId
}

type TargetDef struct {
	*pluginv1.TargetDef
	*pluginv1.TargetSpec
}

func (t TargetDef) GetRef() *pluginv1.TargetRef {
	return t.TargetSpec.GetRef()
}

func (e *Engine) GetDef(ctx context.Context, c DefContainer, rc *ResolveCache) (*TargetDef, error) {
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

	res, err, _ := rc.memDef.Do(tref.Format(c.GetRef()), func() (*TargetDef, error) {
		spec, err := e.GetSpec(ctx, SpecContainer{
			Ref:  c.Ref,
			Spec: c.Spec,
		}, rc)
		if err != nil {
			return nil, err
		}

		driver, ok := e.DriversByName[spec.GetDriver()]
		if !ok {
			return nil, fmt.Errorf("driver %q doesnt exist", spec.GetDriver())
		}

		res, err := driver.Parse(ctx, connect.NewRequest(&pluginv1.ParseRequest{
			Spec: spec,
		}))
		if err != nil {
			return nil, err
		}

		def := res.Msg.GetTarget()

		if !tref.Equal(def.Ref, spec.Ref) {
			return nil, fmt.Errorf("mismatch def ref %v %v", tref.Format(def.Ref), tref.Format(spec.Ref))
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

func (e *Engine) LightLink(ctx context.Context, c DefContainer) (*LightLinkedTarget, error) {
	ctx = trace.ContextWithSpan(ctx, e.RootSpan)
	ctx = hstep.WithoutParent(ctx)

	ctx, span := tracer.Start(ctx, "LightLink", trace.WithAttributes(attribute.String("target", tref.Format(c.GetRef()))))
	defer span.End()

	step, ctx := hstep.New(ctx, fmt.Sprintf("Linking %v...", tref.Format(c.GetRef())))
	defer step.Done()

	rc := GlobalResolveCache

	def, err := e.GetDef(ctx, c, rc)
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
		refStr := tref.Format(depRef)

		g.Go(func() error {
			linkedDep, err, _ := sf.Do(refStr, func() (*TargetDef, error) {
				return e.GetDef(ctx, DefContainer{Ref: depRef}, rc)
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
