package hcore

import (
	"context"

	"connectrpc.com/connect"
	"github.com/hephbuild/heph/internal/hcore/hlog"
	"github.com/hephbuild/heph/internal/hcore/hstep"
	"github.com/hephbuild/heph/internal/hpanic"
	"github.com/hephbuild/heph/plugin/gen/heph/core/v1/corev1connect"
)

func NewRecoveryInterceptor() connect.UnaryInterceptorFunc {
	return func(next connect.UnaryFunc) connect.UnaryFunc {
		return func(ctx context.Context, req connect.AnyRequest) (connect.AnyResponse, error) {
			return hpanic.RecoverV(func() (connect.AnyResponse, error) {
				return next(ctx, req)
			}, hpanic.Wrap(func(err error) error {
				return connect.NewError(connect.CodeInternal, err)
			}))
		}
	}
}

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
