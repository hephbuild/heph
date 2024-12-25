package engine

import (
	"connectrpc.com/connect"
	"context"
	"errors"
	"fmt"
	"github.com/hephbuild/hephv2/hmaps"
	"github.com/hephbuild/hephv2/plugin/c2"
	pluginv1 "github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1/pluginv1connect"
	"net"
	"net/http"
	"slices"
)

type RegisterMuxFunc = func(mux *http.ServeMux)

type PluginHandle struct {
	HttpClient *http.Client
	BaseURL    string
}

func (e *Engine) RegisterPlugin(ctx context.Context, register RegisterMuxFunc) (PluginHandle, error) {
	mux := http.NewServeMux()

	register(mux)

	l, err := net.Listen("tcp", "127.0.0.1:")
	if err != nil {
		return PluginHandle{}, err
	}
	// TODO: close listener

	go func() {
		if err := http.Serve(l, mux); err != nil {
			panic(err)
		}
	}()

	addr := "http://" + l.Addr().String()
	httpClient := http.DefaultClient

	ph := PluginHandle{HttpClient: httpClient, BaseURL: addr}

	return ph, nil
}

type ProviderHandle struct {
	PluginHandle
	Client pluginv1connect.ProviderClient
}

func (e *Engine) RegisterProvider(ctx context.Context, handler pluginv1connect.ProviderHandler) (ProviderHandle, error) {
	path, h := pluginv1connect.NewProviderHandler(handler, connect.WithInterceptors(c2.NewInterceptor()))

	pluginh, err := e.RegisterPlugin(ctx, func(mux *http.ServeMux) {
		mux.Handle(path, h)
	})
	if err != nil {
		return ProviderHandle{}, err
	}

	client := pluginv1connect.NewProviderClient(pluginh.HttpClient, pluginh.BaseURL, connect.WithInterceptors(c2.NewInterceptor()))

	e.Providers = append(e.Providers, client)

	return ProviderHandle{
		PluginHandle: pluginh,
		Client:       client,
	}, nil
}

type DriverHandle struct {
	PluginHandle
	Client pluginv1connect.DriverClient
}

func (e *Engine) RegisterDriver(ctx context.Context, handler pluginv1connect.DriverHandler, register RegisterMuxFunc) (DriverHandle, error) {
	path, h := pluginv1connect.NewDriverHandler(handler, connect.WithInterceptors(c2.NewInterceptor()))

	pluginh, err := e.RegisterPlugin(ctx, func(mux *http.ServeMux) {
		mux.Handle(path, h)
		if register != nil {
			register(mux)
		}
	})
	if err != nil {
		return DriverHandle{}, err
	}

	client := pluginv1connect.NewDriverClient(pluginh.HttpClient, pluginh.BaseURL, connect.WithInterceptors(c2.NewInterceptor()))

	res, err := client.Config(ctx, connect.NewRequest(&pluginv1.ConfigRequest{}))
	if err != nil {
		return DriverHandle{}, err
	}

	if e.DriversByName == nil {
		e.DriversByName = map[string]pluginv1connect.DriverClient{}
	}
	if e.DriversHandle == nil {
		e.DriversHandle = map[pluginv1connect.DriverClient]PluginHandle{}
	}

	e.Drivers = append(e.Drivers, client)
	e.DriversByName[res.Msg.Name] = client
	e.DriversHandle[client] = pluginh

	return DriverHandle{
		PluginHandle: pluginh,
		Client:       client,
	}, nil
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
		return nil, fmt.Errorf("driver %q doesnt exist", spec.Ref.Driver)
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
