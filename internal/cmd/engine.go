package cmd

import (
	"context"
	"github.com/hephbuild/heph/internal/engine"
	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/plugin/pluginbuildfile"
	"github.com/hephbuild/heph/plugin/pluginexec"
	"github.com/hephbuild/heph/plugin/pluginfs"
	"github.com/hephbuild/heph/plugin/plugingo"
	"go.opentelemetry.io/contrib/instrumentation/net/http/otelhttp"
	"net/http"
)

func newEngine(ctx context.Context, root string) (*engine.Engine, error) {
	e, err := engine.New(ctx, root, engine.Config{})
	if err != nil {
		return nil, err
	}

	_, err = e.RegisterProvider(ctx, pluginbuildfile.New(hfs.NewOS(root)))
	if err != nil {
		return nil, err
	}

	_, err = e.RegisterProvider(ctx, plugingo.New())
	if err != nil {
		return nil, err
	}

	_, err = e.RegisterProvider(ctx, pluginfs.NewProvider())
	if err != nil {
		return nil, err
	}

	_, err = e.RegisterDriver(ctx, pluginfs.NewDriver(), nil)
	if err != nil {
		return nil, err
	}

	drivers := []*pluginexec.Plugin{
		pluginexec.New(),
		pluginexec.NewSh(),
		pluginexec.NewBash(),
		pluginexec.NewInteractiveBash(),
	}

	for _, p := range drivers {
		_, err = e.RegisterDriver(ctx, p, func(mux *http.ServeMux) {
			path, h := p.PipesHandler()

			h = otelhttp.NewHandler(h, "Pipe")

			mux.Handle(path, h)
		})
		if err != nil {
			return nil, err
		}
	}

	return e, nil
}
