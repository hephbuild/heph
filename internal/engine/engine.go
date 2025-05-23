package engine

import (
	"connectrpc.com/connect"
	"connectrpc.com/otelconnect"
	"context"
	"errors"
	"github.com/hephbuild/heph/internal/hcore"
	"github.com/hephbuild/heph/internal/hsoftcontext"
	engine2 "github.com/hephbuild/heph/lib/engine"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"go.opentelemetry.io/otel/attribute"
	semconv "go.opentelemetry.io/otel/semconv/v1.21.0"
	"go.opentelemetry.io/otel/trace"
	"os"
	"path/filepath"
	"strings"
	"sync"

	"github.com/hephbuild/heph/internal/hcore/hlog"
	"github.com/hephbuild/heph/internal/hcore/hstep"
	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/plugin/gen/heph/core/v1/corev1connect"
)

var getCwd = sync.OnceValues(func() (string, error) {
	cwd := os.Getenv("HEPH_CWD")

	if cwd == "" {
		var err error
		cwd, err = os.Getwd()
		if err != nil {
			return "", err
		}
	}

	cwd, err := filepath.Abs(cwd)
	if err != nil {
		return "", err
	}

	return cwd, nil
})

func Cwd() (string, error) {
	return getCwd()
}

const ConfigFileName = ".hephconfig2"

func Root() (string, error) {
	return getRoot()
}

var getRoot = sync.OnceValues(func() (string, error) {
	cwd, err := Cwd()
	if err != nil {
		return "", err
	}

	parts := strings.Split(cwd, string(filepath.Separator))
	for len(parts) > 0 {
		p := string(filepath.Separator) + filepath.Join(parts...)

		if _, err := os.Stat(filepath.Join(p, ConfigFileName)); err == nil {
			return p, nil
		}

		parts = parts[:len(parts)-1]
	}

	return "", errors.New("root not found, are you running this command in the repo directory?")
})

type EngineHandle struct {
	ServerHandle
	engine2.EngineHandle
}

type Engine struct {
	Root     hfs.OS
	Home     hfs.OS
	Cache    hfs.OS
	Sandbox  hfs.OS
	RootSpan trace.Span

	CoreHandle EngineHandle

	Providers     []EngineProvider
	Drivers       []engine2.Driver
	DriversHandle map[engine2.Driver]PluginHandle
	DriversByName map[string]engine2.Driver
	DriversConfig map[string]*pluginv1.ConfigResponse

	SoftCancel *hsoftcontext.Handler
}

type EngineProvider struct {
	Name string
	engine2.Provider
}

func New(ctx context.Context, root string, cfg Config) (*Engine, error) {
	rootfs := hfs.NewOS(root)
	homefs := hfs.At(rootfs, ".heph")
	cachefs := hfs.At(homefs, "cache")
	sandboxfs := hfs.At(homefs, "sandbox")

	e := &Engine{
		Root:       rootfs,
		Home:       homefs,
		Cache:      cachefs,
		Sandbox:    sandboxfs,
		RootSpan:   trace.SpanFromContext(ctx),
		SoftCancel: hsoftcontext.NewHandler(),
	}

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

	interceptors := []connect.Interceptor{
		otelInterceptor,
	}

	srvh, err := e.newServer(ctx)
	if err != nil {
		return nil, err
	}

	handlerOpts := []connect.HandlerOption{
		connect.WithInterceptors(interceptors...),
		hcore.WithRecovery(),
	}

	clientOpts := []connect.ClientOption{
		connect.WithInterceptors(interceptors...),
	}

	srvh.Mux.Handle(corev1connect.NewControlServiceHandler(e.SoftCancel, handlerOpts...))
	controlClient := corev1connect.NewControlServiceClient(srvh.HTTPClient(), srvh.GetBaseURL(), clientOpts...)

	srvh.Mux.Handle(corev1connect.NewLogServiceHandler(hlog.NewLoggerHandler(hlog.From(ctx))))
	srvh.Mux.Handle(corev1connect.NewStepServiceHandler(hstep.NewHandler(hstep.HandlerFromContext(ctx)), handlerOpts...))
	srvh.Mux.Handle(corev1connect.NewResultServiceHandler(e.ResultHandler(), append(handlerOpts, connect.WithInterceptors(hsoftcontext.Interceptor(controlClient)))...))

	e.CoreHandle = EngineHandle{
		ServerHandle: srvh,
		EngineHandle: engine2.EngineHandle{
			LogClient:     corev1connect.NewLogServiceClient(srvh.HTTPClient(), srvh.GetBaseURL()),
			StepClient:    corev1connect.NewStepServiceClient(srvh.HTTPClient(), srvh.GetBaseURL(), clientOpts...),
			ResultClient:  e.Resulter(),
			ControlClient: controlClient,
		},
	}

	return e, nil
}
