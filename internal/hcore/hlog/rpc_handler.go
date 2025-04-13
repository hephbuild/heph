package hlog

import (
	"context"
	"fmt"
	"log/slog"

	"connectrpc.com/connect"
	corev1 "github.com/hephbuild/heph/plugin/gen/heph/core/v1"
	"github.com/hephbuild/heph/plugin/gen/heph/core/v1/corev1connect"
)

type rpcHandler struct {
	logger *slog.Logger
}

func (r rpcHandler) Create(ctx context.Context, req *connect.Request[corev1.CreateRequest]) (*connect.Response[corev1.CreateResponse], error) {
	logger := r.logger
	if len(req.Msg.Attrs) > 0 {
		var attrs []any
		for _, attr := range req.Msg.Attrs {
			switch value := attr.Value.(type) {
			case *corev1.CreateRequest_Attr_ValueStr:
				attrs = append(attrs, slog.String(attr.Key, value.ValueStr))
			case *corev1.CreateRequest_Attr_ValueBool:
				attrs = append(attrs, slog.Bool(attr.Key, value.ValueBool))
			case *corev1.CreateRequest_Attr_ValueInt:
				attrs = append(attrs, slog.Int64(attr.Key, value.ValueInt))
			case *corev1.CreateRequest_Attr_ValueFloat:
				attrs = append(attrs, slog.Float64(attr.Key, value.ValueFloat))
			default:
				attrs = append(attrs, slog.String(attr.Key, fmt.Sprintf("%#v", attr.Value)))
			}
		}

		logger = logger.With(attrs...)
	}

	switch req.Msg.GetLevel() {
	case corev1.CreateRequest_LEVEL_TRACE:
		logger.Debug(req.Msg.GetMessage())
	case corev1.CreateRequest_LEVEL_INFO:
		logger.Info(req.Msg.GetMessage())
	case corev1.CreateRequest_LEVEL_WARN:
		logger.Warn(req.Msg.GetMessage())
	case corev1.CreateRequest_LEVEL_ERROR:
		logger.Error(req.Msg.GetMessage())
	case corev1.CreateRequest_LEVEL_UNSPECIFIED:
		fallthrough
	default:
		logger.Info(req.Msg.GetMessage())
	}

	return connect.NewResponse(&corev1.CreateResponse{}), nil
}

func NewLoggerHandler(logger *slog.Logger) corev1connect.LogServiceHandler {
	return rpcHandler{logger: logger}
}
