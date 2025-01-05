package engine

import (
	"context"
	"errors"
	"fmt"
	"slices"
	"strings"

	"golang.org/x/sync/errgroup"

	"connectrpc.com/connect"
	"github.com/hephbuild/heph/internal/hcore/hstep"
	"github.com/hephbuild/heph/internal/hmaps"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

func (e *Engine) GetSpec(ctx context.Context, pkg, name string) (*pluginv1.TargetSpec, error) {
	var eg errgroup.Group

	var states []*pluginv1.ProviderState

	segments := strings.Split(pkg, "/")
	segments = slices.Insert(segments, 0, "")
	for i := range segments {
		for _, p := range e.Providers {
			res, err := p.Probe(ctx, connect.NewRequest(&pluginv1.ProbeRequest{
				Package: strings.Join(segments[:i], "/"),
			}))
			if err != nil {
				return nil, err
			}

			states = append(states, res.Msg.GetStates()...)
		}
	}

	err := eg.Wait()
	if err != nil {
		return nil, err
	}

	for _, p := range e.Providers {
		var providerStates []*pluginv1.ProviderState
		for _, state := range states {
			if state.GetProvider() == p.Name {
				providerStates = append(providerStates, state)
			}
		}

		res, err := p.Get(ctx, connect.NewRequest(&pluginv1.GetRequest{
			Ref: &pluginv1.TargetRef{
				Package: pkg,
				Name:    name,
			},
			States: providerStates,
		}))
		if err != nil {
			if connect.CodeOf(err) == connect.CodeNotFound {
				continue
			}

			return nil, err
		}

		return res.Msg.GetSpec(), nil
	}

	return nil, errors.New("target not found")
}

type Refish interface {
	GetPackage() string
	GetName() string
}

func (e *Engine) GetDefFromRef(ctx context.Context, ref Refish) (*pluginv1.TargetDef, error) {
	return e.GetDef(ctx, ref.GetPackage(), ref.GetName())
}

func (e *Engine) GetDef(ctx context.Context, pkg, name string) (*pluginv1.TargetDef, error) {
	// put back when we have custom ids
	// step, ctx := hstep.New(ctx, "Getting definition...")
	// defer step.Done()

	spec, err := e.GetSpec(ctx, pkg, name)
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

type LinkMemo struct {
	m map[string]*LinkedTarget
}

func (e *Engine) LinkDeep(ctx context.Context, pkg, name string) (*LinkedTarget, error) {
	return e.linkInner(ctx, pkg, name, &LinkMemo{m: map[string]*LinkedTarget{}})
}

func (e *Engine) linkInner(ctx context.Context, pkg, name string, memo *LinkMemo) (*LinkedTarget, error) {
	if lt, ok := memo.m[pkg+name]; ok {
		return lt, nil
	}

	def, err := e.GetDef(ctx, pkg, name)
	if err != nil {
		return nil, err
	}

	lt := &LinkedTarget{
		TargetDef: def,
	}

	memo.m[pkg+name] = lt

	for _, dep := range def.GetDeps() {
		linkedDep, err := e.linkInner(ctx, dep.GetRef().GetPackage(), dep.GetRef().GetName(), memo)
		if err != nil {
			return nil, err
		}

		lt.Deps = append(lt.Deps, linkedDep)
	}

	return lt, nil
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

func (e *Engine) LightLink(ctx context.Context, pkg, name string) (*LightLinkedTarget, error) {
	step, ctx := hstep.New(ctx, "Linking...")
	defer step.Done()

	def, err := e.GetDef(ctx, pkg, name)
	if err != nil {
		return nil, err
	}

	lt := &LightLinkedTarget{
		TargetDef: def,
	}

	dedupOutputs := map[string]int{}

	for _, dep := range def.GetDeps() {
		getOutputIndex, setOutputIndex := hmaps.GetSet(dedupOutputs, dep.GetRef().String())

		i, ok := getOutputIndex()

		if i == -1 {
			continue
		}

		linkeddep, err := e.GetDefFromRef(ctx, dep.GetRef())
		if err != nil {
			return nil, err
		}

		var outputs []string
		var allset bool
		if ok {
			outputs = lt.Deps[i].Outputs
		}
		if dep.Ref.Output == nil {
			allset = true

			outputs = linkeddep.GetOutputs()
		} else {
			outputs = append(outputs, dep.GetRef().GetOutput())
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

	return lt, nil
}
