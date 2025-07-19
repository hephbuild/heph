package engine

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"sync"

	"connectrpc.com/connect"
	"connectrpc.com/otelconnect"
	"github.com/google/uuid"
	"github.com/hephbuild/heph/internal/hcore"
	"github.com/hephbuild/heph/lib/pluginsdk"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	sync_map "github.com/zolstein/sync-map"
	"go.opentelemetry.io/otel/attribute"
	semconv "go.opentelemetry.io/otel/semconv/v1.21.0"
	"go.opentelemetry.io/otel/trace"

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
	pluginsdk.Engine
}

type Engine struct {
	Root     hfs.OS
	Home     hfs.OS
	Cache    hfs.OS
	Sandbox  hfs.OS
	RootSpan trace.Span

	CoreHandle EngineHandle

	Providers     []EngineProvider
	Drivers       []pluginsdk.Driver
	DriversHandle map[pluginsdk.Driver]PluginHandle
	DriversByName map[string]pluginsdk.Driver
	DriversConfig map[string]*pluginv1.ConfigResponse
	Caches        []CacheHandle

	requestState sync_map.Map[string, *RequestState]
}

type EngineProvider struct {
	Name string
	pluginsdk.Provider
}

func New(ctx context.Context, root string, cfg Config) (*Engine, error) {
	if cfg.HomeDir == "" {
		cfg.HomeDir = ".heph"
	}

	rootfs := hfs.NewOS(root)
	homefs := hfs.At(rootfs, cfg.HomeDir)
	cachefs := hfs.At(homefs, "cache")
	sandboxfs := hfs.At(homefs, "sandbox")

	e := &Engine{
		Root:     rootfs,
		Home:     homefs,
		Cache:    cachefs,
		Sandbox:  sandboxfs,
		RootSpan: trace.SpanFromContext(ctx),
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

	srvh.Mux.Handle(corev1connect.NewLogServiceHandler(hlog.NewLoggerHandler(hlog.From(ctx))))
	srvh.Mux.Handle(corev1connect.NewStepServiceHandler(hstep.NewHandler(hstep.HandlerFromContext(ctx)), handlerOpts...))
	srvh.Mux.Handle(corev1connect.NewResultServiceHandler(e.ResultHandler(), handlerOpts...))

	e.CoreHandle = EngineHandle{
		ServerHandle: srvh,
		Engine: pluginsdk.Engine{
			LogClient:    corev1connect.NewLogServiceClient(srvh.HTTPClient(), srvh.GetBaseURL()),
			StepClient:   corev1connect.NewStepServiceClient(srvh.HTTPClient(), srvh.GetBaseURL(), clientOpts...),
			ResultClient: e.Resulter(),
		},
	}

	return e, nil
}

func (e *Engine) NewRequestState() (*RequestState, func()) {
	id := uuid.New().String()

	s := &RequestState{
		ID:               id,
		RequestStateData: &RequestStateData{},
	}

	e.requestState.Store(id, s)

	return s, func() {
		e.requestState.Delete(id)
	}
}

func (e *Engine) StoreRequestState(rs *RequestState) func() {
	id := rs.ID

	e.requestState.Store(id, rs)

	return func() {
		e.requestState.Delete(id)
	}
}

func (e *Engine) GetRequestState(id string) (*RequestState, error) {
	s, ok := e.requestState.Load(id)
	if !ok {
		return nil, fmt.Errorf("request state not found for id %q", id)
	}

	return s, nil
}
