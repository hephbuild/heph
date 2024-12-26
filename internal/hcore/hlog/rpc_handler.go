package hlog

import (
	"connectrpc.com/connect"
	"context"
	"github.com/hephbuild/hephv2/plugin/gen/heph/core/v1"
	"github.com/hephbuild/hephv2/plugin/gen/heph/core/v1/corev1connect"
	"log/slog"
)

type rpcHandler struct {
	logger *slog.Logger
}

func (r rpcHandler) Create(ctx context.Context, req *connect.Request[corev1.CreateRequest]) (*connect.Response[corev1.CreateResponse], error) {
	switch req.Msg.Level {
	case corev1.CreateRequest_LEVEL_TRACE:
		r.logger.Debug(req.Msg.Message)
	case corev1.CreateRequest_LEVEL_INFO:
		r.logger.Info(req.Msg.Message)
	case corev1.CreateRequest_LEVEL_WARN:
		r.logger.Warn(req.Msg.Message)
	case corev1.CreateRequest_LEVEL_ERROR:
		r.logger.Error(req.Msg.Message)
	default:
		r.logger.Info(req.Msg.Message)
	}

	return connect.NewResponse(&corev1.CreateResponse{}), nil
}

func NewLoggerHandler(logger *slog.Logger) corev1connect.LogServiceHandler {
	return rpcHandler{logger: logger}
}
