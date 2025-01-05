package engine

import (
	"context"
	"crypto/tls"
	"net"
	"net/http"
	"time"

	"connectrpc.com/connect"
	"github.com/hephbuild/heph/internal/hcore"
	"github.com/hephbuild/heph/internal/hcore/hstep/hstepconnect"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/gen/heph/plugin/v1/pluginv1connect"
	"golang.org/x/net/http2"
	"golang.org/x/net/http2/h2c"
)

type ServerHandle struct {
	Listener net.Listener
	Mux      *http.ServeMux

	client *http.Client
}

func (h ServerHandle) BaseURL() string {
	return "http://" + h.Listener.Addr().String()
}

func (h ServerHandle) HTTPClient() *http.Client {
	return h.client
}

var httpClient = &http.Client{
	Transport: &http2.Transport{
		AllowHTTP: true,
		DialTLSContext: func(ctx context.Context, network, addr string, cfg *tls.Config) (net.Conn, error) {
			return net.Dial(network, addr)
		},
	},
}

func (e *Engine) newServer(ctx context.Context) (ServerHandle, error) {
	l, err := net.Listen("tcp", "127.0.0.1:")
	if err != nil {
		return ServerHandle{}, err
	}
	// TODO: close listener

	mux := http.NewServeMux()

	go func() {
		h2s := &http2.Server{}
		srv := &http.Server{
			Handler: h2c.NewHandler(mux, h2s),
			BaseContext: func(listener net.Listener) context.Context {
				return ctx
			},
			ReadHeaderTimeout: 5 * time.Second,
		}

		if err := srv.Serve(l); err != nil {
			panic(err)
		}
	}()

	h := ServerHandle{
		Listener: l,
		Mux:      mux,
		client:   httpClient,
	}

	// warmup the client
	go h.client.Get(h.BaseURL()) //nolint:errcheck,noctx

	return h, nil
}

type RegisterMuxFunc = func(mux *http.ServeMux)

type PluginHandle struct {
	ServerHandle
}

func (e *Engine) RegisterPlugin(ctx context.Context, register RegisterMuxFunc) (PluginHandle, error) {
	sh, err := e.newServer(ctx)
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

type PluginInit struct {
	CoreHandle EngineHandle
}

type PluginIniter interface {
	PluginInit(context.Context, PluginInit) error
}

func (e *Engine) initPlugin(ctx context.Context, handler any) error {
	if pi, ok := handler.(PluginIniter); ok {
		err := pi.PluginInit(ctx, PluginInit{
			CoreHandle: e.CoreHandle,
		})

		return err
	}

	return nil
}

func (e *Engine) pluginInterceptor() connect.Option {
	return connect.WithInterceptors(
		hcore.NewRecoveryInterceptor(),
		hcore.NewInterceptor(e.CoreHandle.LogClient, e.CoreHandle.StepClient),
		hstepconnect.Interceptor(),
	)
}

func (e *Engine) RegisterProvider(ctx context.Context, handler pluginv1connect.ProviderHandler) (ProviderHandle, error) {
	path, h := pluginv1connect.NewProviderHandler(handler, e.pluginInterceptor())

	pluginh, err := e.RegisterPlugin(ctx, func(mux *http.ServeMux) {
		mux.Handle(path, h)
	})
	if err != nil {
		return ProviderHandle{}, err
	}

	client := pluginv1connect.NewProviderClient(pluginh.HTTPClient(), pluginh.BaseURL(), e.pluginInterceptor())

	res, err := client.Config(ctx, connect.NewRequest(&pluginv1.ProviderConfigRequest{}))
	if err != nil {
		return ProviderHandle{}, err
	}

	e.Providers = append(e.Providers, Provider{
		Name:           res.Msg.GetName(),
		ProviderClient: client,
	})

	err = e.initPlugin(ctx, handler)
	if err != nil {
		return ProviderHandle{}, err
	}

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
	path, h := pluginv1connect.NewDriverHandler(handler, e.pluginInterceptor())

	pluginh, err := e.RegisterPlugin(ctx, func(mux *http.ServeMux) {
		mux.Handle(path, h)
		if register != nil {
			register(mux)
		}
	})
	if err != nil {
		return DriverHandle{}, err
	}

	client := pluginv1connect.NewDriverClient(pluginh.HTTPClient(), pluginh.BaseURL(), e.pluginInterceptor())

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
	if e.DriversConfig == nil {
		e.DriversConfig = map[string]*pluginv1.ConfigResponse{}
	}

	e.Drivers = append(e.Drivers, client)
	e.DriversByName[res.Msg.GetName()] = client
	e.DriversHandle[client] = pluginh
	e.DriversConfig[res.Msg.GetName()] = res.Msg

	err = e.initPlugin(ctx, handler)
	if err != nil {
		return DriverHandle{}, err
	}

	return DriverHandle{
		PluginHandle: pluginh,
		Client:       client,
	}, nil
}
