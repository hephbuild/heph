package hcore

import (
	"connectrpc.com/connect"
	"context"
	"github.com/hephbuild/hephv2/internal/hcore/hlog"
	"github.com/hephbuild/hephv2/internal/hcore/hstep"
	"github.com/hephbuild/hephv2/plugin/gen/heph/core/v1/corev1connect"
)

func NewInterceptor(
	logClient corev1connect.LogServiceClient,
	stepClient corev1connect.StepServiceClient,
) *Interceptor {
	return &Interceptor{
		logClient:  logClient,
		stepClient: stepClient,
	}
}

var _ connect.Interceptor = (*Interceptor)(nil)

type Interceptor struct {
	logClient  corev1connect.LogServiceClient
	stepClient corev1connect.StepServiceClient
}

func (i Interceptor) handlerSide(ctx context.Context) context.Context {
	ctx = hlog.ContextWithLogger(ctx, hlog.NewRPCLogger(i.logClient))
	ctx = hstep.ContextWithRPCHandler(ctx, i.stepClient)

	return ctx
}

func (i Interceptor) WrapUnary(next connect.UnaryFunc) connect.UnaryFunc {
	return func(ctx context.Context, req connect.AnyRequest) (connect.AnyResponse, error) {
		if !req.Spec().IsClient {
			ctx = i.handlerSide(ctx)
		}

		return next(ctx, req)
	}
}

func (i Interceptor) WrapStreamingClient(next connect.StreamingClientFunc) connect.StreamingClientFunc {
	return func(ctx context.Context, spec connect.Spec) connect.StreamingClientConn {
		conn := next(ctx, spec)

		return conn
	}
}

func (i Interceptor) WrapStreamingHandler(next connect.StreamingHandlerFunc) connect.StreamingHandlerFunc {
	return func(ctx context.Context, conn connect.StreamingHandlerConn) error {
		ctx = i.handlerSide(ctx)

		return next(ctx, conn)
	}
}
