package hlog

import (
	"connectrpc.com/connect"
	"context"
	corev1 "github.com/hephbuild/hephv2/plugin/gen/heph/core/v1"
	"github.com/hephbuild/hephv2/plugin/gen/heph/core/v1/corev1connect"
	"log/slog"
	"sync"
)

type slogRpcHandler struct {
	*innerSlogRpcHandler
	attrs []slog.Attr
}

type innerSlogRpcHandler struct {
	m      sync.Mutex
	client corev1connect.LogServiceClient
}

func (l slogRpcHandler) Enabled(ctx context.Context, level slog.Level) bool {
	return true
}

func (l slogRpcHandler) Handle(ctx context.Context, record slog.Record) error {
	l.m.Lock()
	defer l.m.Unlock()

	var level corev1.CreateRequest_Level
	switch record.Level {
	case slog.LevelDebug:
		level = corev1.CreateRequest_LEVEL_TRACE
	case slog.LevelInfo:
		level = corev1.CreateRequest_LEVEL_INFO
	case slog.LevelWarn:
		level = corev1.CreateRequest_LEVEL_WARN
	case slog.LevelError:
		level = corev1.CreateRequest_LEVEL_ERROR
	default:
		level = corev1.CreateRequest_LEVEL_INFO
	}

	_, err := l.client.Create(ctx, connect.NewRequest(&corev1.CreateRequest{
		Level:   level,
		Message: record.Message,
	}))
	if err != nil {
		return err
	}

	return nil
}

func (l slogRpcHandler) WithAttrs(attrs []slog.Attr) slog.Handler {
	l.attrs = append(l.attrs, attrs...)

	return l
}

func (l slogRpcHandler) WithGroup(name string) slog.Handler {
	return l
}

func NewRPCLogger(client corev1connect.LogServiceClient) *slog.Logger {
	return NewLogger(&slogRpcHandler{
		innerSlogRpcHandler: &innerSlogRpcHandler{
			client: client,
		},
	})
}
