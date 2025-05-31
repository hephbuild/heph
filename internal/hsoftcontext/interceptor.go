package hsoftcontext

import (
	"connectrpc.com/connect"
	"context"
	"errors"
	"github.com/hephbuild/heph/internal/hcore/hlog"
	corev1 "github.com/hephbuild/heph/plugin/gen/heph/core/v1"
	"github.com/hephbuild/heph/plugin/gen/heph/core/v1/corev1connect"
	"github.com/hephbuild/heph/plugin/gen/heph/plugin/v1/pluginv1connect"
	"sync/atomic"
)

func Interceptor(client corev1connect.ControlServiceClient) connect.Interceptor {
	return interceptor{client: client}
}

type interceptor struct {
	client corev1connect.ControlServiceClient
}

type streamingClientInterceptor struct {
	connect.StreamingClientConn

	onClose func()
}

func (s *streamingClientInterceptor) CloseResponse() error {
	err := s.StreamingClientConn.CloseResponse()
	s.onClose()
	return err
}

func (i interceptor) clientSide(ctx context.Context) (string, context.Context, func(), error) {
	lctx, lcancel := WithCancel(context.WithoutCancel(ctx))
	strm := i.client.NewContext(lctx)

	ictx, icancel := WithCancel(context.WithoutCancel(ctx))

	var cleanupCalled atomic.Bool
	cleanup := func() {
		cleanupCalled.Store(true)

		_ = strm.CloseRequest()
		_ = strm.CloseResponse()

		lcancel(errors.New("cleanup"))
		icancel(errors.New("cleanup"))
	}

	conn, err := strm.Conn()
	if err != nil {
		cleanup()

		return "", ctx, func() {}, err
	}

	err = conn.Send(nil) // Without that the handler never gets actually called ?
	if err != nil {
		cleanup()

		return "", ctx, func() {}, err
	}

	res, err := strm.Receive()
	if err != nil {
		cleanup()

		return "", ctx, func() {}, err
	}

	go func() {
		select {
		case <-ctx.Done():
			if cleanupCalled.Load() {
				return
			}

			ctxErr := ctx.Err()
			err := strm.Send(&corev1.NewContextRequest{Msg: &corev1.NewContextRequest_Cancel_{
				Cancel: &corev1.NewContextRequest_Cancel{Cause: ctxErr.Error()},
			}})
			if err != nil && !errors.Is(err, context.Canceled) {
				hlog.From(ctx).Error("control context: error sending request", "error", err)
			}
		}
	}()

	return res.Id, ictx, cleanup, nil
}

func (i interceptor) handlerSide(ctx context.Context, id string) (context.Context, func(), error) {
	nctx, cancel := WithCancel(ctx)

	strm, err := i.client.ContextDone(context.WithoutCancel(ctx), connect.NewRequest(&corev1.ContextDoneRequest{Id: id}))
	if err != nil {
		return ctx, nil, err
	}

	go func() {
		defer strm.Close()

		for strm.Receive() {
			msg := strm.Msg()

			if msg.Err == "" {
				continue
			}

			cancel(errors.New(msg.Err))
			break
		}
		if err := strm.Err(); err != nil {
			hlog.From(ctx).Error("soft context: error receiving from stream", "error", err)
		}
	}()

	return nctx, func() {
		cancel(nil)
	}, nil
}

var ignores = map[string]struct{}{
	pluginv1connect.DriverRunProcedure: {},
}

const header = "x-heph-context-boundary-id"

func (i interceptor) WrapUnary(next connect.UnaryFunc) connect.UnaryFunc {
	return func(ctx context.Context, req connect.AnyRequest) (connect.AnyResponse, error) {
		if _, ok := ignores[req.Spec().Procedure]; ok {
			return next(ctx, req)
		}

		if req.Spec().IsClient {
			id, nctx, cancel, err := i.clientSide(ctx)
			if err != nil {
				return nil, err
			}
			req.Header().Set(header, id)
			defer cancel()
			ctx = nctx
		} else {
			nctx, cancel, err := i.handlerSide(ctx, req.Header().Get(header))
			if err != nil {
				return nil, err
			}
			defer cancel()
			ctx = nctx
		}

		return next(ctx, req)
	}
}

func (i interceptor) WrapStreamingClient(next connect.StreamingClientFunc) connect.StreamingClientFunc {
	return func(ctx context.Context, spec connect.Spec) connect.StreamingClientConn {
		if _, ok := ignores[spec.Procedure]; ok {
			return next(ctx, spec)
		}

		id, ctx, cancel, err := i.clientSide(ctx)
		if err != nil {
			hlog.From(ctx).Error("streaming client side", "error", err)

			return next(ctx, spec)
		}

		conn := next(ctx, spec)
		conn.RequestHeader().Set(header, id)

		return &streamingClientInterceptor{
			StreamingClientConn: conn,
			onClose:             cancel,
		}
	}
}

func (i interceptor) WrapStreamingHandler(next connect.StreamingHandlerFunc) connect.StreamingHandlerFunc {
	return func(ctx context.Context, conn connect.StreamingHandlerConn) error {
		if _, ok := ignores[conn.Spec().Procedure]; ok {
			return next(ctx, conn)
		}

		ctx, cancel, err := i.handlerSide(ctx, conn.RequestHeader().Get(header))
		if err != nil {
			return err
		}
		defer cancel()

		return next(ctx, conn)
	}
}
