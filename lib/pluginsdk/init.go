package pluginsdk

import (
	"context"
	corev1 "github.com/hephbuild/heph/plugin/gen/heph/core/v1"
	"github.com/hephbuild/heph/plugin/gen/heph/core/v1/corev1connect"
)

type Resulter interface {
	Get(context.Context, *corev1.ResultRequest) (*corev1.ResultResponse, error)
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
