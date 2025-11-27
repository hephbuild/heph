package hlog

import (
	"context"
	"fmt"
	"log/slog"
	"sync"

	"github.com/hephbuild/heph/internal/htypes"

	"connectrpc.com/connect"
	corev1 "github.com/hephbuild/heph/plugin/gen/heph/core/v1"
	"github.com/hephbuild/heph/plugin/gen/heph/core/v1/corev1connect"
)

type slogRPCHandler struct {
	*innerSlogRPCHandler
	attrs []slog.Attr
}

type innerSlogRPCHandler struct {
	m      sync.Mutex
	client corev1connect.LogServiceClient
}

func (l slogRPCHandler) Enabled(ctx context.Context, level slog.Level) bool {
	return true
}

func (l slogRPCHandler) Handle(ctx context.Context, record slog.Record) error {
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

	buildAttr := func(attr slog.Attr) *corev1.CreateRequest_Attr {
		rpcAttr := corev1.CreateRequest_Attr_builder{
			Key: htypes.Ptr(attr.Key),
		}.Build()

		switch attr.Value.Kind() {
		case slog.KindBool:
			rpcAttr.SetValueBool(attr.Value.Bool())
		case slog.KindString:
			rpcAttr.SetValueStr(attr.Value.String())
		case slog.KindInt64:
			rpcAttr.SetValueInt(attr.Value.Int64())
		case slog.KindFloat64:
			rpcAttr.SetValueFloat(attr.Value.Float64())
		default:
			rpcAttr.SetValueStr(fmt.Sprint(attr.Value.Any()))
		}

		return rpcAttr
	}

	attrs := make([]*corev1.CreateRequest_Attr, 0, len(l.attrs)+record.NumAttrs())
	for _, attr := range l.attrs {
		attrs = append(attrs, buildAttr(attr))
	}
	record.Attrs(func(attr slog.Attr) bool {
		attrs = append(attrs, buildAttr(attr))
		return true
	})

	_, err := l.client.Create(ctx, connect.NewRequest(corev1.CreateRequest_builder{
		Level:   htypes.Ptr(level),
		Message: htypes.Ptr(record.Message),
		Attrs:   attrs,
	}.Build()))
	if err != nil {
		return err
	}

	return nil
}

func (l slogRPCHandler) WithAttrs(attrs []slog.Attr) slog.Handler {
	l.attrs = append(l.attrs, attrs...)

	return l
}

func (l slogRPCHandler) WithGroup(name string) slog.Handler {
	return l
}

func NewRCPHandler(client corev1connect.LogServiceClient) slog.Handler {
	return &slogRPCHandler{
		innerSlogRPCHandler: &innerSlogRPCHandler{
			client: client,
		},
	}
}

func NewRPCLogger(client corev1connect.LogServiceClient) *slog.Logger {
	return NewLogger(NewRCPHandler(client))
}
