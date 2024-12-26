package engine

import (
	"connectrpc.com/connect"
	"context"
	"errors"
	"github.com/hephbuild/hephv2/internal/hcore/hlog"
	"github.com/hephbuild/hephv2/internal/hfs"
	"github.com/hephbuild/hephv2/plugin/gen/heph/core/v1/corev1connect"
	"github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1/pluginv1connect"
	"os"
	"path/filepath"
	"strings"
)

func Root() (string, error) {
	var err error
	cwd := os.Getenv("HEPH_CWD")

	if cwd == "" {
		cwd, err = os.Getwd()
		if err != nil {
			return "", err
		}
	}

	cwd, err = filepath.Abs(cwd)
	if err != nil {
		return "", err
	}

	parts := strings.Split(cwd, string(filepath.Separator))
	for len(parts) > 0 {
		p := string(filepath.Separator) + filepath.Join(parts...)

		if _, err := os.Stat(filepath.Join(p, ".hephconfig")); err == nil {
			return p, nil
		}

		parts = parts[:len(parts)-1]
	}

	return "", errors.New("root not found, are you running this command in the repo directory?")
}

type Config struct {
}

type EngineHandle struct {
	ServerHandle
	Client interface {
		corev1connect.LogServiceClient
	}
}

type Engine struct {
	Root    hfs.OS
	Home    hfs.OS
	Cache   hfs.OS
	Sandbox hfs.OS

	CoreHandle EngineHandle

	Providers     []pluginv1connect.ProviderClient
	Drivers       []pluginv1connect.DriverClient
	DriversHandle map[pluginv1connect.DriverClient]PluginHandle
	DriversByName map[string]pluginv1connect.DriverClient
}

func New(ctx context.Context, root string, cfg Config) (*Engine, error) {
	rootfs := hfs.NewOS(root)
	homefs := hfs.At(rootfs, ".heph")
	cachefs := hfs.At(homefs, "cache")
	sandboxfs := hfs.At(homefs, "sandbox")

	e := &Engine{
		Root:    rootfs,
		Home:    homefs,
		Cache:   cachefs,
		Sandbox: sandboxfs,
	}

	srvh, err := e.newServer()
	if err != nil {
		return nil, err
	}

	{
		path, handler := corev1connect.NewLogServiceHandler(hlog.NewLoggerHandler(hlog.From(ctx)))

		srvh.Mux.Handle(path, handler)
	}

	e.CoreHandle = EngineHandle{
		ServerHandle: srvh,
		Client:       corev1connect.NewLogServiceClient(srvh.HttpClient(), srvh.BaseURL(), connect.WithInterceptors()),
	}

	return e, nil
}
