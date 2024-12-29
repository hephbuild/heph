package hstepconnect

import (
	"context"

	"connectrpc.com/connect"
	"github.com/hephbuild/hephv2/internal/hcore/hstep"
)

const stepParentIDHeaderKey = "heph-step-parent-id"

func Interceptor() connect.Interceptor {
	return connect.UnaryInterceptorFunc(func(next connect.UnaryFunc) connect.UnaryFunc {
		return func(ctx context.Context, request connect.AnyRequest) (connect.AnyResponse, error) {
			if request.Spec().IsClient {
				step := hstep.From(ctx)
				if step.GetID() != "" {
					request.Header().Set(stepParentIDHeaderKey, step.GetID())
				}
			} else {
				ctx = hstep.ContextWithParentID(ctx, request.Header().Get(stepParentIDHeaderKey))
			}

			return next(ctx, request)
		}
	})
}
