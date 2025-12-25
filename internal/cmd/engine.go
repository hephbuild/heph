package cmd

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"os"
	"path/filepath"
	"runtime"
	"strings"

	"github.com/go-viper/mapstructure/v2"
	"github.com/hephbuild/heph/internal/engine"
	"github.com/hephbuild/heph/internal/hbbt/hbbtexec"
	"github.com/hephbuild/heph/internal/hcore/hlog"
	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/internal/remotecache"
	"github.com/hephbuild/heph/internal/termui"
	"github.com/hephbuild/heph/lib/pluginsdk"
	"github.com/hephbuild/heph/plugin/pluginbin"
	"github.com/hephbuild/heph/plugin/pluginbuildfile"
	"github.com/hephbuild/heph/plugin/pluginexec"
	execv1 "github.com/hephbuild/heph/plugin/pluginexec/gen/heph/plugin/exec/v1"
	"github.com/hephbuild/heph/plugin/pluginfs"
	"github.com/hephbuild/heph/plugin/plugingo"
	"github.com/hephbuild/heph/plugin/plugingroup"
	"github.com/hephbuild/heph/plugin/pluginnix"
	nixv1 "github.com/hephbuild/heph/plugin/pluginnix/gen/heph/plugin/nix/v1"
	"github.com/hephbuild/heph/plugin/plugintextfile"
	"go.opentelemetry.io/contrib/instrumentation/net/http/otelhttp"
)

func parseConfig(ctx context.Context, root string) (engine.Config, error) {
	cfg := engine.Config{
		Version:    "dev", // TODO
		HomeDir:    ".heph",
		LockDriver: "fs",
	}

	cfg.Providers = append(cfg.Providers, engine.ConfigProvider{
		Name:    pluginfs.NameProvider,
		Enabled: true,
	})

	cfg.Providers = append(cfg.Providers, engine.ConfigProvider{
		Name:    pluginnix.NameProvider,
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
		Name:    pluginfs.NameProvider,
		Enabled: true,
	})

	cfg.Drivers = append(cfg.Drivers, engine.ConfigDriver{
		Name:    plugintextfile.Name,
		Enabled: true,
	})

	cfg.Drivers = append(cfg.Drivers, engine.ConfigDriver{
		Name:    plugingroup.Name,
		Enabled: true,
	})

	cfg.Drivers = append(cfg.Drivers, engine.ConfigDriver{
		Name:    pluginbin.Name,
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
		Name:    pluginexec.NameShShell,
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

	cfg.Drivers = append(cfg.Drivers, engine.ConfigDriver{
		Name:    pluginnix.NameBash,
		Enabled: true,
	})

	cfg.Drivers = append(cfg.Drivers, engine.ConfigDriver{
		Name:    pluginnix.NameBashShell,
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

var nameToProvider = map[string]func(ctx context.Context, root string, options map[string]any) pluginsdk.Provider{
	pluginbuildfile.Name: func(ctx context.Context, root string, options map[string]any) pluginsdk.Provider {
		var cfg pluginbuildfile.Options
		err := mapstructure.Decode(options, &cfg)
		if err != nil {
			panic(err)
		}

		return pluginbuildfile.New(hfs.NewOS(root), cfg)
	},
	plugingo.Name: func(ctx context.Context, root string, options map[string]any) pluginsdk.Provider {
		var cfg plugingo.Options
		err := mapstructure.Decode(options, &cfg)
		if err != nil {
			panic(err)
		}

		return plugingo.New(cfg)
	},
	pluginfs.NameProvider: func(ctx context.Context, root string, options map[string]any) pluginsdk.Provider {
		return pluginfs.NewProvider()
	},
	pluginnix.NameProvider: func(ctx context.Context, root string, options map[string]any) pluginsdk.Provider {
		return pluginnix.NewProvider()
	},
}

func pluginExecFactory(factory func(options ...pluginexec.Option[*execv1.Target]) *pluginexec.Plugin[*execv1.Target], options map[string]any) (pluginsdk.Driver, func(mux *http.ServeMux)) {
	var cfg struct {
		PATH []string `mapstructure:"PATH"`
	}
	cfg.PATH = []string{
		"/nix/var/nix/profiles/default/bin", // TODO: figure out how to make plugins provide that
		"/usr/local/bin",
		"/usr/bin",
		"/bin",
	}
	err := mapstructure.Decode(options, &cfg)
	if err != nil {
		panic(err)
	}

	d := factory(pluginexec.WithPath[*execv1.Target](cfg.PATH))

	return d, func(mux *http.ServeMux) {
		path, h := d.PipesHandler()

		h = otelhttp.NewHandler(h, "Pipe")

		mux.Handle(path, h)
	}
}

func pluginNixFactory(factory func(options ...pluginexec.Option[*nixv1.Target]) *pluginexec.Plugin[*nixv1.Target], options map[string]any) (pluginsdk.Driver, func(mux *http.ServeMux)) {
	d := factory()

	return d, func(mux *http.ServeMux) {
		path, h := d.PipesHandler()

		h = otelhttp.NewHandler(h, "Pipe")

		mux.Handle(path, h)
	}
}

var nameToDriver = map[string]func(ctx context.Context, root string, options map[string]any) (pluginsdk.Driver, func(mux *http.ServeMux)){
	pluginfs.NameProvider: func(ctx context.Context, root string, options map[string]any) (pluginsdk.Driver, func(mux *http.ServeMux)) {
		return pluginfs.NewDriver(), nil
	},
	plugintextfile.Name: func(ctx context.Context, root string, options map[string]any) (pluginsdk.Driver, func(mux *http.ServeMux)) {
		return plugintextfile.New(), nil
	},
	plugingroup.Name: func(ctx context.Context, root string, options map[string]any) (pluginsdk.Driver, func(mux *http.ServeMux)) {
		return plugingroup.New(), nil
	},
	pluginbin.Name: func(ctx context.Context, root string, options map[string]any) (pluginsdk.Driver, func(mux *http.ServeMux)) {
		return pluginbin.New(), nil
	},
	pluginexec.NameExec: func(ctx context.Context, root string, options map[string]any) (pluginsdk.Driver, func(mux *http.ServeMux)) {
		return pluginExecFactory(pluginexec.NewExec, options)
	},
	pluginexec.NameSh: func(ctx context.Context, root string, options map[string]any) (pluginsdk.Driver, func(mux *http.ServeMux)) {
		return pluginExecFactory(pluginexec.NewSh, options)
	},
	pluginexec.NameShShell: func(ctx context.Context, root string, options map[string]any) (pluginsdk.Driver, func(mux *http.ServeMux)) {
		return pluginExecFactory(pluginexec.NewInteractiveSh, options)
	},
	pluginexec.NameBash: func(ctx context.Context, root string, options map[string]any) (pluginsdk.Driver, func(mux *http.ServeMux)) {
		return pluginExecFactory(pluginexec.NewBash, options)
	},
	pluginexec.NameBashShell: func(ctx context.Context, root string, options map[string]any) (pluginsdk.Driver, func(mux *http.ServeMux)) {
		return pluginExecFactory(pluginexec.NewInteractiveBash, options)
	},
	pluginnix.NameBash: func(ctx context.Context, root string, options map[string]any) (pluginsdk.Driver, func(mux *http.ServeMux)) {
		return pluginNixFactory(pluginnix.NewBash, options)
	},
	pluginnix.NameBashShell: func(ctx context.Context, root string, options map[string]any) (pluginsdk.Driver, func(mux *http.ServeMux)) {
		return pluginNixFactory(pluginnix.NewInteractiveBash, options)
	},
}

var nameToCache = map[string]func(ctx context.Context, options map[string]any) (pluginsdk.Cache, error){
	remotecache.DriverNameGCS: func(ctx context.Context, options map[string]any) (pluginsdk.Cache, error) {
		return remotecache.NewGCS(ctx, fmt.Sprint(options["bucket"]))
	},
	remotecache.DriverNameExec: func(ctx context.Context, options map[string]any) (pluginsdk.Cache, error) {
		var optionss struct {
			Args []string `mapstructure:"args"`
		}
		err := mapstructure.Decode(options, &optionss)
		if err != nil {
			return nil, err
		}

		return remotecache.NewExec(optionss.Args)
	},
	remotecache.DriverNameSh: func(ctx context.Context, options map[string]any) (pluginsdk.Cache, error) {
		return remotecache.NewSh(fmt.Sprint(options["cmd"]))
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

	for _, cache := range cfg.Caches {
		if !cache.Read && !cache.Write {
			continue
		}

		factory, ok := nameToCache[cache.Driver]
		if !ok {
			return nil, fmt.Errorf("unknown cache driver %s: %q", cache.Name, cache.Driver)
		}

		c, err := factory(ctx, cache.Options)
		if err != nil {
			return nil, err
		}

		_, err = e.RegisterCache(cache.Name, c, cache.Read, cache.Write)
		if err != nil {
			return nil, err
		}
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
