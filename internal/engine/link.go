package engine

import (
	"context"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/internal/hsingleflight"
	"github.com/hephbuild/heph/plugin/tref"
	"path/filepath"
	"slices"
	"strings"

	"connectrpc.com/connect"
	"github.com/hephbuild/heph/internal/hcore/hstep"
	"github.com/hephbuild/heph/internal/hmaps"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
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
	// TODO: caching must be smarter, probably package-based ?
	specs, err, _ := rc.memSpecs.Do(tref.Format(c.GetRef()), func() ([]*pluginv1.TargetSpec, error) {
		strm, err := p.GetSpecs(ctx, connect.NewRequest(&pluginv1.GetSpecsRequest{
			Ref:    c.Ref,
			States: states,
		}))
		if err != nil {
			return nil, err
		}

		var specs []*pluginv1.TargetSpec
		for strm.Receive() {
			switch res := strm.Msg().Of.(type) {
			case *pluginv1.GetSpecsResponse_Spec:
				if !tref.Equal(res.Spec.Ref, c.GetRef()) {
					rc.memRef.Set(tref.Format(res.Spec.Ref), res.Spec, nil)
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

	res, err := p.Get(ctx, connect.NewRequest(&pluginv1.GetRequest{
		Ref:    c.GetRef(),
		States: states,
	}))
	if err != nil {
		return nil, err
	}

	return res.Msg.GetSpec(), nil
}

func (e *Engine) ResolveSpec(ctx context.Context, states []*pluginv1.ProviderState, c SpecContainer, rc *ResolveCache) (*pluginv1.TargetSpec, error) {
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

	return spec, nil
}

func (e *Engine) GetSpec(ctx context.Context, c SpecContainer, rc *ResolveCache) (*pluginv1.TargetSpec, error) {
	if c.Spec != nil {
		return c.Spec, nil
	}

	var pkg string
	if c.Ref != nil {
		pkg = c.Ref.GetPackage()
	} else {
		return nil, errors.New("spec or ref must be specified")
	}

	var states []*pluginv1.ProviderState
	segments := strings.Split(pkg, string(filepath.Separator))
	if len(segments) == 0 || len(segments) > 1 && segments[0] != "" {
		// make sure to always probe root
		segments = slices.Insert(segments, 0, "")
	}
	for i := range segments {
		for _, p := range e.Providers {
			res, err := p.Probe(ctx, connect.NewRequest(&pluginv1.ProbeRequest{
				Package: strings.Join(segments[:i], string(filepath.Separator)),
			}))
			if err != nil {
				return nil, err
			}

			states = append(states, res.Msg.GetStates()...)
		}
	}

	// TODO: errgroup to parallelize probing

	return e.ResolveSpec(ctx, states, c, rc)
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
	memSpecs hsingleflight.GroupMem[[]*pluginv1.TargetSpec]
	memRef   hsingleflight.GroupMem[*pluginv1.TargetSpec]
}

func (e *Engine) GetDef(ctx context.Context, c DefContainer, rc *ResolveCache) (*pluginv1.TargetDef, error) {
	// put back when we have custom ids
	// step, ctx := hstep.New(ctx, "Getting definition...")
	// defer step.Done()

	if c.Def != nil {
		return c.Def, nil
	}

	spec, err := e.GetSpec(ctx, SpecContainer{
		Ref:  c.Ref,
		Spec: c.Spec,
	}, rc)
	if err != nil {
		return nil, err
	}

	driver, ok := e.DriversByName[spec.GetRef().GetDriver()]
	if !ok {
		return nil, fmt.Errorf("driver %q doesnt exist", spec.GetRef().GetDriver())
	}

	res, err := driver.Parse(ctx, connect.NewRequest(&pluginv1.ParseRequest{
		Spec: spec,
	}))
	if err != nil {
		return nil, err
	}

	return res.Msg.GetTarget(), nil
}

type LinkedTarget struct {
	*pluginv1.TargetDef
	Deps []*LinkedTarget
}

func refWithOutputToRef(ref *pluginv1.TargetRefWithOutput) *pluginv1.TargetRef {
	return &pluginv1.TargetRef{
		Package: ref.GetPackage(),
		Name:    ref.GetName(),
		Driver:  ref.GetDriver(),
		Args:    ref.GetArgs(),
	}
}

type LightLinkedTargetDep struct {
	*pluginv1.TargetDef
	Outputs []string
	DefDep  *pluginv1.TargetDef_Dep
}

type LightLinkedTarget struct {
	*pluginv1.TargetDef
	Deps []*LightLinkedTargetDep
}

func (e *Engine) LightLink(ctx context.Context, c DefContainer) (*LightLinkedTarget, error) {
	step, ctx := hstep.New(ctx, "Linking...")
	defer step.Done()

	rc := &ResolveCache{}

	def, err := e.GetDef(ctx, c, rc)
	if err != nil {
		return nil, err
	}

	lt := &LightLinkedTarget{
		TargetDef: def,
	}

	dedupOutputs := map[string]int{}

	for _, dep := range def.GetDeps() {
		ref := dep.GetRef()

		getOutputIndex, setOutputIndex := hmaps.GetSet(dedupOutputs, ref.String())

		i, ok := getOutputIndex()

		if i == -1 {
			continue
		}

		linkeddep, err := e.GetDef(ctx, DefContainer{Ref: refWithOutputToRef(ref)}, rc)
		if err != nil {
			return nil, err
		}

		var outputs []string
		var allset bool
		if ok {
			outputs = lt.Deps[i].Outputs
		}
		if ref.Output == nil {
			allset = true

			outputs = linkeddep.GetOutputs()
		} else {
			outputs = append(outputs, ref.GetOutput())
			slices.Sort(outputs)
			outputs = slices.Compact(outputs)
		}

		if ok {
			lt.Deps[i].Outputs = outputs
		} else {
			if allset {
				setOutputIndex(-1)
			} else {
				setOutputIndex(len(lt.Deps))
			}
			lt.Deps = append(lt.Deps, &LightLinkedTargetDep{
				DefDep:    dep,
				TargetDef: linkeddep,
				Outputs:   outputs,
			})
		}
	}

	slices.SortFunc(lt.Deps, func(a, b *LightLinkedTargetDep) int {
		return tref.CompareOut(a.DefDep.Ref, a.DefDep.Ref)

	})

	return lt, nil
}
