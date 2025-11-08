package pluginsdk

import (
	"context"

	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

type Driver interface {
	Config(context.Context, *pluginv1.ConfigRequest) (*pluginv1.ConfigResponse, error)
	Parse(context.Context, *pluginv1.ParseRequest) (*pluginv1.ParseResponse, error)
	ApplyTransitive(context.Context, *pluginv1.ApplyTransitiveRequest) (*pluginv1.ApplyTransitiveResponse, error)
	Run(context.Context, *pluginv1.RunRequest) (*pluginv1.RunResponse, error)
	Pipe(context.Context, *pluginv1.PipeRequest) (*pluginv1.PipeResponse, error)
}
