package engine

import (
	"context"
	"github.com/hephbuild/heph/plugin/gen/heph/core/v1/corev1connect"
)

type PluginInit struct {
	CoreHandle EngineHandle
	Root       string
}

type PluginIniter interface {
	PluginInit(context.Context, PluginInit) error
}

type EngineHandle struct {
	LogClient     corev1connect.LogServiceClient
	StepClient    corev1connect.StepServiceClient
	ResultClient  corev1connect.ResultServiceClient
	ControlClient corev1connect.ControlServiceClient
}
