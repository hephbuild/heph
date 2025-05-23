package cmd

import (
	"context"
	"errors"
	"fmt"
	"github.com/go-viper/mapstructure/v2"
	"github.com/hephbuild/heph/internal/engine"
	"github.com/hephbuild/heph/internal/hbbt/hbbtexec"
	"github.com/hephbuild/heph/internal/hcore/hlog"
	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/internal/termui"
	engine2 "github.com/hephbuild/heph/lib/engine"
	"github.com/hephbuild/heph/plugin/gen/heph/plugin/v1/pluginv1connect"
	"github.com/hephbuild/heph/plugin/pluginbuildfile"
	"github.com/hephbuild/heph/plugin/pluginexec"
	"github.com/hephbuild/heph/plugin/pluginfs"
	"github.com/hephbuild/heph/plugin/plugingo"
	"go.opentelemetry.io/contrib/instrumentation/net/http/otelhttp"
	"net/http"
	"os"
	"path/filepath"
	"runtime"
	"strings"
)

func parseConfig(ctx context.Context, root string) (engine.Config, error) {
	cfg := engine.Config{
		Version: "dev", // TODO
	}

	cfg.Providers = append(cfg.Providers, engine.ConfigProvider{
		Name:    pluginfs.Name,
		Enabled: true,
	})

	cfg.Providers = append(cfg.Providers, engine.ConfigProvider{
		Name:    pluginbuildfile.Name,
		Enabled: true,
		Options: map[string]any{
			"patterns": []string{"BUILD", "*.BUILD"},
		},
	})

	cfg.Providers = append(cfg.Providers, engine.ConfigProvider{
		Name:    plugingo.Name,
		Enabled: true,
		Options: map[string]any{
			"version": strings.TrimPrefix(runtime.Version(), "go"),
		},
	})

	cfg.Drivers = append(cfg.Drivers, engine.ConfigDriver{
		Name:    pluginfs.Name,
		Enabled: true,
	})

	cfg.Drivers = append(cfg.Drivers, engine.ConfigDriver{
		Name:    pluginexec.NameExec,
		Enabled: true,
	})

	cfg.Drivers = append(cfg.Drivers, engine.ConfigDriver{
		Name:    pluginexec.NameSh,
		Enabled: true,
	})

	cfg.Drivers = append(cfg.Drivers, engine.ConfigDriver{
		Name:    pluginexec.NameBash,
		Enabled: true,
	})

	cfg.Drivers = append(cfg.Drivers, engine.ConfigDriver{
		Name:    pluginexec.NameBashShell,
		Enabled: true,
	})

	for _, p := range []string{engine.ConfigFileName, engine.ConfigFileName + ".local"} {
		yamlCfg, err := engine.ParseYAMLConfig(filepath.Join(root, p))
		if err != nil {
			if errors.Is(err, os.ErrNotExist) {
				continue
			}

			return cfg, err
		}

		cfg, err = engine.ApplyYAMLConfig(cfg, yamlCfg)
		if err != nil {
			return cfg, err
		}
	}

	return cfg, nil
}

var nameToProvider = map[string]func(ctx context.Context, root string, options map[string]any) pluginv1connect.ProviderHandler{
	pluginbuildfile.Name: func(ctx context.Context, root string, options map[string]any) pluginv1connect.ProviderHandler {
		var cfg pluginbuildfile.Options
		err := mapstructure.Decode(options, &cfg)
		if err != nil {
			panic(err)
		}

		return engine2.NewProviderConnectHandler(pluginbuildfile.New(hfs.NewOS(root), cfg))
	},
	plugingo.Name: func(ctx context.Context, root string, options map[string]any) pluginv1connect.ProviderHandler {
		return plugingo.New()
	},
	pluginfs.Name: func(ctx context.Context, root string, options map[string]any) pluginv1connect.ProviderHandler {
		return pluginfs.NewProvider()
	},
}

func pluginExecFactory(d *pluginexec.Plugin) (pluginv1connect.DriverHandler, func(mux *http.ServeMux)) {
	return d, func(mux *http.ServeMux) {
		path, h := d.PipesHandler()

		h = otelhttp.NewHandler(h, "Pipe")

		mux.Handle(path, h)
	}
}

var nameToDriver = map[string]func(ctx context.Context, root string, options map[string]any) (pluginv1connect.DriverHandler, func(mux *http.ServeMux)){
	pluginfs.Name: func(ctx context.Context, root string, options map[string]any) (pluginv1connect.DriverHandler, func(mux *http.ServeMux)) {
		return pluginfs.NewDriver(), nil
	},
	pluginexec.NameExec: func(ctx context.Context, root string, options map[string]any) (pluginv1connect.DriverHandler, func(mux *http.ServeMux)) {
		d := pluginexec.New()

		return pluginExecFactory(d)
	},
	pluginexec.NameSh: func(ctx context.Context, root string, options map[string]any) (pluginv1connect.DriverHandler, func(mux *http.ServeMux)) {
		d := pluginexec.NewSh()

		return pluginExecFactory(d)
	},
	pluginexec.NameBash: func(ctx context.Context, root string, options map[string]any) (pluginv1connect.DriverHandler, func(mux *http.ServeMux)) {
		d := pluginexec.NewBash()

		return pluginExecFactory(d)
	},
	pluginexec.NameBashShell: func(ctx context.Context, root string, options map[string]any) (pluginv1connect.DriverHandler, func(mux *http.ServeMux)) {
		d := pluginexec.NewInteractiveBash()

		return pluginExecFactory(d)
	},
}

func newEngine(ctx context.Context, root string) (*engine.Engine, error) {
	cfg, err := parseConfig(ctx, root)
	if err != nil {
		return nil, err
	}

	e, err := engine.New(ctx, root, cfg)
	if err != nil {
		return nil, err
	}

	for _, plugin := range cfg.Providers {
		if !plugin.Enabled {
			continue
		}

		factory, ok := nameToProvider[plugin.Name]
		if !ok {
			return nil, fmt.Errorf("unknown provider %q", plugin.Name)
		}

		p := factory(ctx, root, plugin.Options)

		_, err = e.RegisterProvider(ctx, p)
		if err != nil {
			return nil, err
		}
	}

	for _, plugin := range cfg.Drivers {
		if !plugin.Enabled {
			continue
		}

		factory, ok := nameToDriver[plugin.Name]
		if !ok {
			return nil, fmt.Errorf("unknown driver %q", plugin.Name)
		}

		d, register := factory(ctx, root, plugin.Options)

		_, err = e.RegisterDriver(ctx, d, register)
		if err != nil {
			return nil, err
		}
	}

	return e, nil
}

func newTermui(ctx context.Context, f termui.RunFunc) error {
	wrappedF := func(ctx context.Context, execFunc func(f hbbtexec.ExecFunc) error) error {
		go func() {
			<-ctx.Done()
			hlog.From(ctx).Warn("interrupt, gracefully stopping...")
		}()

		return f(ctx, execFunc)
	}

	if !plain && isTerm() {
		return termui.NewInteractive(ctx, wrappedF)
	} else {
		return termui.NewNonInteractive(ctx, wrappedF)
	}
}
