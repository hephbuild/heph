package engine

import (
	"connectrpc.com/connect"
	"context"
	"errors"
	"fmt"
	"github.com/hephbuild/hephv2/hmaps"
	pluginv1 "github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1/pluginv1connect"
	"slices"
)

func (e *Engine) RegisterProvider(ctx context.Context, client pluginv1connect.ProviderClient) error {
	e.Providers = append(e.Providers, client)

	return nil
}

func (e *Engine) RegisterDriver(ctx context.Context, client pluginv1connect.DriverClient) error {
	res, err := client.Config(ctx, connect.NewRequest(&pluginv1.ConfigRequest{}))
	if err != nil {
		return err
	}

	if e.DriversByName == nil {
		e.DriversByName = map[string]pluginv1connect.DriverClient{}
	}

	e.DriversByName[res.Msg.Name] = client
	e.Drivers = append(e.Drivers, client)

	return nil
}

func (e *Engine) GetSpec(ctx context.Context, pkg, name string) (*pluginv1.TargetSpec, error) {
	for _, p := range e.Providers {
		res, err := p.Get(ctx, connect.NewRequest(&pluginv1.GetRequest{
			Ref: &pluginv1.TargetRef{
				Package: pkg,
				Name:    name,
			},
		}))
		if err != nil {
			return nil, err
		}

		return res.Msg.Spec, nil
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
	spec, err := e.GetSpec(ctx, pkg, name)
	if err != nil {
		return nil, err
	}

	driver, ok := e.DriversByName[spec.Ref.Driver]
	if !ok {
		return nil, fmt.Errorf("driver doesnt exist: %s", spec.Ref.Driver)
	}

	res, err := driver.Parse(ctx, connect.NewRequest(&pluginv1.ParseRequest{
		Spec: spec,
	}))
	if err != nil {
		return nil, err
	}

	return res.Msg.Target, nil
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

	for _, dep := range def.Deps {
		linkedDep, err := e.linkInner(ctx, dep.Ref.Package, dep.Ref.Name, memo)
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
}

type LightLinkedTarget struct {
	*pluginv1.TargetDef
	Deps []*LightLinkedTargetDep
}

func (e *Engine) LightLink(ctx context.Context, pkg, name string) (*LightLinkedTarget, error) {
	def, err := e.GetDef(ctx, pkg, name)
	if err != nil {
		return nil, err
	}

	lt := &LightLinkedTarget{
		TargetDef: def,
	}

	dedupOutputs := map[string]int{}

	for _, dep := range def.Deps {
		getOutputIndex, setOutputIndex := hmaps.GetSet(dedupOutputs, dep.Ref.String())

		i, ok := getOutputIndex()

		if i == -1 {
			continue
		}

		linkeddep, err := e.GetDefFromRef(ctx, dep.Ref)
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

			outputs = linkeddep.Outputs
		} else {
			outputs = append(outputs, *dep.Ref.Output)
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
				TargetDef: linkeddep,
				Outputs:   outputs,
			})
		}
	}

	return lt, nil
}
