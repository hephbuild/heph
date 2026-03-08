package pluginsdk

import (
	"context"

	corev1 "github.com/hephbuild/heph/plugin/gen/heph/core/v1"
	"github.com/hephbuild/heph/plugin/gen/heph/core/v1/corev1connect"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

type GetResult struct {
	Release   func()
	Artifacts []Artifact
	Def       *pluginv1.TargetDef
}

type EngineResulter interface {
	Get(context.Context, *corev1.ResultRequest) (*GetResult, error)
}

type Resulter struct {
	Engine EngineResulter
}

func (r *Resulter) Get(ctx context.Context, req *corev1.ResultRequest) (*GetResult, error) {
	res, err := r.Engine.Get(ctx, req)
	if err != nil {
		return nil, err
	}

	return res, nil
}

type InitPayload struct {
	Engine Engine
	Root   string
}

type Initer interface {
	PluginInit(context.Context, InitPayload) error
}

type Engine struct {
	LogClient    corev1connect.LogServiceClient
	StepClient   corev1connect.StepServiceClient
	ResultClient Resulter
}
