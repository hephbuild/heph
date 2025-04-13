package hlog

import (
	"context"
	"fmt"
	"log/slog"
	"sync"

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

	var attrs []*corev1.CreateRequest_Attr
	appendAttr := func(attrs []*corev1.CreateRequest_Attr, attr slog.Attr) []*corev1.CreateRequest_Attr {
		rpcAttr := &corev1.CreateRequest_Attr{
			Key: attr.Key,
		}

		switch attr.Value.Kind() {
		case slog.KindBool:
			rpcAttr.Value = &corev1.CreateRequest_Attr_ValueBool{ValueBool: attr.Value.Bool()}
		case slog.KindString:
			rpcAttr.Value = &corev1.CreateRequest_Attr_ValueStr{ValueStr: attr.Value.String()}
		case slog.KindInt64:
			rpcAttr.Value = &corev1.CreateRequest_Attr_ValueInt{ValueInt: attr.Value.Int64()}
		case slog.KindFloat64:
			rpcAttr.Value = &corev1.CreateRequest_Attr_ValueFloat{ValueFloat: attr.Value.Float64()}
		default:
			rpcAttr.Value = &corev1.CreateRequest_Attr_ValueStr{ValueStr: fmt.Sprint(attr.Value.Any())}
		}

		attrs = append(attrs, rpcAttr)

		return attrs
	}

	for _, attr := range l.attrs {
		attrs = appendAttr(attrs, attr)
	}
	record.Attrs(func(attr slog.Attr) bool {
		attrs = appendAttr(attrs, attr)
		return true
	})

	_, err := l.client.Create(ctx, connect.NewRequest(&corev1.CreateRequest{
		Level:   level,
		Message: record.Message,
		Attrs:   attrs,
	}))
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

func NewRPCLogger(client corev1connect.LogServiceClient) *slog.Logger {
	return NewLogger(&slogRPCHandler{
		innerSlogRPCHandler: &innerSlogRPCHandler{
			client: client,
		},
	})
}
