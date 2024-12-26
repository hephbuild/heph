package engine

import (
	"connectrpc.com/connect"
	"context"
	"github.com/hephbuild/hephv2/plugin/c2"
	pluginv1 "github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1/pluginv1connect"
	"net"
	"net/http"
)

type ServerHandle struct {
	Listener net.Listener
	Mux      *http.ServeMux
}

func (h ServerHandle) BaseURL() string {
	return "http://" + h.Listener.Addr().String()
}

func (h ServerHandle) HttpClient() *http.Client {
	return http.DefaultClient
}

func (e *Engine) newServer() (ServerHandle, error) {
	l, err := net.Listen("tcp", "127.0.0.1:")
	if err != nil {
		return ServerHandle{}, err
	}
	// TODO: close listener

	mux := http.NewServeMux()

	go func() {
		if err := http.Serve(l, mux); err != nil {
			panic(err)
		}
	}()

	return ServerHandle{
		Listener: l,
		Mux:      mux,
	}, nil
}

type RegisterMuxFunc = func(mux *http.ServeMux)

type PluginHandle struct {
	ServerHandle
}

func (e *Engine) RegisterPlugin(ctx context.Context, register RegisterMuxFunc) (PluginHandle, error) {
	sh, err := e.newServer()
	if err != nil {
		return PluginHandle{}, err
	}

	register(sh.Mux)

	return PluginHandle{ServerHandle: sh}, nil
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

	client := pluginv1connect.NewProviderClient(pluginh.HttpClient(), pluginh.BaseURL(), connect.WithInterceptors(c2.NewInterceptor()))

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

	client := pluginv1connect.NewDriverClient(pluginh.HttpClient(), pluginh.BaseURL(), connect.WithInterceptors(c2.NewInterceptor()))

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
