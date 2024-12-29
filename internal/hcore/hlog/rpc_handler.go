package hlog

import (
	"context"
	"log/slog"

	"connectrpc.com/connect"
	corev1 "github.com/hephbuild/heph/plugin/gen/heph/core/v1"
	"github.com/hephbuild/heph/plugin/gen/heph/core/v1/corev1connect"
)

type rpcHandler struct {
	logger *slog.Logger
}

func (r rpcHandler) Create(ctx context.Context, req *connect.Request[corev1.CreateRequest]) (*connect.Response[corev1.CreateResponse], error) {
	switch req.Msg.GetLevel() {
	case corev1.CreateRequest_LEVEL_TRACE:
		r.logger.Debug(req.Msg.GetMessage())
	case corev1.CreateRequest_LEVEL_INFO:
		r.logger.Info(req.Msg.GetMessage())
	case corev1.CreateRequest_LEVEL_WARN:
		r.logger.Warn(req.Msg.GetMessage())
	case corev1.CreateRequest_LEVEL_ERROR:
		r.logger.Error(req.Msg.GetMessage())
	case corev1.CreateRequest_LEVEL_UNSPECIFIED:
		fallthrough
	default:
		r.logger.Info(req.Msg.GetMessage())
	}

	return connect.NewResponse(&corev1.CreateResponse{}), nil
}

func NewLoggerHandler(logger *slog.Logger) corev1connect.LogServiceHandler {
	return rpcHandler{logger: logger}
}
