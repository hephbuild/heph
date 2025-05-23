package engine

import (
	"context"
	"crypto/tls"
	"encoding/base64"
	"fmt"
	"github.com/google/uuid"
	"github.com/hephbuild/heph/internal/hsoftcontext"
	engine2 "github.com/hephbuild/heph/lib/engine"
	"go.opentelemetry.io/contrib/instrumentation/net/http/otelhttp"
	"go.opentelemetry.io/otel/attribute"
	semconv "go.opentelemetry.io/otel/semconv/v1.21.0"
	"go.opentelemetry.io/otel/trace"
	"net"
	"net/http"
	"os"
	"strings"
	"time"

	"connectrpc.com/connect"
	"connectrpc.com/otelconnect"
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
	BaseURL  string
}

func (h ServerHandle) GetBaseURL() string {
	return h.BaseURL
}

func (h ServerHandle) HTTPClient() *http.Client {
	return httpClient
}

func (h ServerHandle) HTTPClientWithOtel() *http.Client {
	return httpClientWithOtel
}

const unixPathPrefix = "unix_"

var httpClient = &http.Client{
	Transport: &http2.Transport{
		AllowHTTP: true,
		DialTLSContext: func(ctx context.Context, network, addr string, cfg *tls.Config) (net.Conn, error) {
			if rest, ok := strings.CutPrefix(addr, unixPathPrefix); ok {
				b64host, _, err := net.SplitHostPort(rest)
				if err != nil {
					return nil, err
				}

				host, err := base64.URLEncoding.DecodeString(b64host)
				if err != nil {
					return nil, fmt.Errorf("%w: %q", err, rest)
				}

				addr = string(host)

				return net.Dial("unix", addr)
			}

			return net.Dial(network, addr)
		},
	},
}

var httpClientWithOtel = &http.Client{
	Transport: otelhttp.NewTransport(httpClient.Transport),
}

func (e *Engine) newListener(ctx context.Context) (net.Listener, string, func(), error) {
	if false {
		dir := e.Home.At("socks")
		err := dir.MkdirAll("", os.ModePerm)
		if err != nil {
			return nil, "", nil, err
		}

		path := dir.At(fmt.Sprintf("%v_%v", os.Getpid(), uuid.New().String())).Path()

		l, err := net.Listen("unix", path)
		if err != nil {
			return nil, "", nil, err
		}
		cleanup := func() {
			_ = l.Close()
			_ = os.Remove(path)
		}

		b64Path := base64.URLEncoding.EncodeToString([]byte(path))

		return l, "http://" + unixPathPrefix + b64Path, cleanup, nil
	} else {
		l, err := net.Listen("tcp", "127.0.0.1:")
		if err != nil {
			return nil, "", nil, err
		}

		return l, "http://" + l.Addr().String(), func() {}, nil
	}
}

func (e *Engine) newServer(ctx context.Context) (ServerHandle, error) {
	l, baseUrl, cleanup, err := e.newListener(ctx)
	if err != nil {
		return ServerHandle{}, err
	}
	mux := http.NewServeMux()

	go func() {
		defer cleanup()
		h2s := &http2.Server{
			MaxReadFrameSize:     1 << 20,
			MaxConcurrentStreams: 500,
		}
		srv := &http.Server{
			Handler: h2c.NewHandler(mux, h2s),
			BaseContext: func(listener net.Listener) context.Context {
				// to prevent inheriting from the root span, make a new noop span
				ctx := trace.ContextWithSpan(context.WithoutCancel(ctx), trace.SpanFromContext(context.Background()))

				return ctx
			},
			ReadHeaderTimeout: 5 * time.Second,
		}

		if err := srv.Serve(l); err != nil {
			panic(err)
		}
	}()

	// TODO: call a clean srv.Shutdown() before exiting the bin
	h := ServerHandle{
		Listener: l,
		Mux:      mux,
		BaseURL:  baseUrl,
	}

	// warmup the client
	go h.HTTPClient().Get(h.GetBaseURL()) //nolint:errcheck,noctx

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
	Client engine2.Provider
}

type PluginInit = engine2.PluginInit

type PluginIniter = engine2.PluginIniter

func (e *Engine) initPlugin(ctx context.Context, handler any) error {
	if pi, ok := handler.(PluginIniter); ok {
		err := pi.PluginInit(ctx, PluginInit{
			CoreHandle: e.CoreHandle.EngineHandle,
			Root:       e.Root.Path(),
		})

		return err
	}

	return nil
}

type pluginSpanDecorator struct {
	pluginType, pluginName string
}

func (f pluginSpanDecorator) decorate(ctx context.Context) {
	span := trace.SpanFromContext(ctx)
	span.SetAttributes(attribute.String("heph.plugin.type", f.pluginType))
	span.SetAttributes(attribute.String("heph.plugin.name", f.pluginName))
}

func (f pluginSpanDecorator) WrapUnary(next connect.UnaryFunc) connect.UnaryFunc {
	return func(ctx context.Context, req connect.AnyRequest) (connect.AnyResponse, error) {
		f.decorate(ctx)

		return next(ctx, req)
	}
}

func (f pluginSpanDecorator) WrapStreamingClient(next connect.StreamingClientFunc) connect.StreamingClientFunc {
	return func(ctx context.Context, spec connect.Spec) connect.StreamingClientConn {
		f.decorate(ctx)

		return next(ctx, spec)
	}
}

func (f pluginSpanDecorator) WrapStreamingHandler(next connect.StreamingHandlerFunc) connect.StreamingHandlerFunc {
	return func(ctx context.Context, conn connect.StreamingHandlerConn) error {
		f.decorate(ctx)

		return next(ctx, conn)
	}
}

func (e *Engine) pluginInterceptor(pluginType, pluginName string) connect.Option {
	otelInterceptor, err := otelconnect.NewInterceptor(
		otelconnect.WithTrustRemote(),
		otelconnect.WithAttributeFilter(func(spec connect.Spec, value attribute.KeyValue) bool {
			if value.Key == semconv.NetPeerPortKey {
				return false
			}
			if value.Key == semconv.NetPeerNameKey {
				return false
			}
			return true
		}),
	)
	if err != nil {
		panic(err)
	}

	return connect.WithInterceptors(
		otelInterceptor,
		pluginSpanDecorator{
			pluginType: pluginType,
			pluginName: pluginName,
		},
		hcore.NewInterceptor(e.CoreHandle.LogClient, e.CoreHandle.StepClient),
		hstepconnect.Interceptor(),
		hsoftcontext.Interceptor(e.CoreHandle.ControlClient),
	)
}

var connectCompressOption = connect.WithCompression("gzip", nil, nil)
var connectAcceptCompressOption = connect.WithAcceptCompression("gzip", nil, nil)

func (e *Engine) RegisterProvider(ctx context.Context, provider engine2.Provider) (ProviderHandle, error) {
	res, err := provider.Config(ctx, &pluginv1.ProviderConfigRequest{})
	if err != nil {
		return ProviderHandle{}, err
	}

	pluginName := res.GetName()

	provider2 := EngineProvider{
		Name:     pluginName,
		Provider: provider,
	}

	e.Providers = append(e.Providers, provider2)

	err = e.initPlugin(ctx, provider)
	if err != nil {
		return ProviderHandle{}, err
	}

	return ProviderHandle{
		Client: provider,
	}, nil
}

type DriverHandle struct {
	PluginHandle
	Client engine2.Driver
}

func (e *Engine) RegisterDriver(ctx context.Context, driver engine2.Driver, register RegisterMuxFunc) (DriverHandle, error) {
	res, err := driver.Config(ctx, &pluginv1.ConfigRequest{})
	if err != nil {
		return DriverHandle{}, err
	}

	pluginName := res.GetName()

	handler := engine2.NewDriverConnectHandler(driver)

	path, h := pluginv1connect.NewDriverHandler(handler, e.pluginInterceptor("driver", pluginName), connectCompressOption)

	pluginh, err := e.RegisterPlugin(ctx, func(mux *http.ServeMux) {
		mux.Handle(path, h)
		if register != nil {
			register(mux)
		}
	})
	if err != nil {
		return DriverHandle{}, err
	}

	cclient := pluginv1connect.NewDriverClient(pluginh.HTTPClient(), pluginh.GetBaseURL(), e.pluginInterceptor("driver", pluginName), connectAcceptCompressOption)
	client := engine2.NewDriverConnectClient(cclient)

	if e.DriversByName == nil {
		e.DriversByName = map[string]engine2.Driver{}
	}
	if e.DriversHandle == nil {
		e.DriversHandle = map[engine2.Driver]PluginHandle{}
	}
	if e.DriversConfig == nil {
		e.DriversConfig = map[string]*pluginv1.ConfigResponse{}
	}

	e.Drivers = append(e.Drivers, client)
	e.DriversByName[pluginName] = client
	e.DriversHandle[client] = pluginh
	e.DriversConfig[pluginName] = res

	err = e.initPlugin(ctx, handler)
	if err != nil {
		return DriverHandle{}, err
	}

	return DriverHandle{
		PluginHandle: pluginh,
		Client:       client,
	}, nil
}
