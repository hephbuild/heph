package hstepconnect

import (
	"connectrpc.com/connect"
	"context"
	"github.com/hephbuild/hephv2/internal/hcore/hstep"
)

const stepParentIdHeaderKey = "heph-step-parent-id"

func Interceptor() connect.Interceptor {
	return connect.UnaryInterceptorFunc(func(next connect.UnaryFunc) connect.UnaryFunc {
		return func(ctx context.Context, request connect.AnyRequest) (connect.AnyResponse, error) {
			if request.Spec().IsClient {
				step, ok := hstep.ParentFromContext(ctx)
				if ok {
					request.Header().Set(stepParentIdHeaderKey, step.Id)
				}
			} else {
				ctx = hstep.ContextWithParentId(ctx, request.Header().Get(stepParentIdHeaderKey))
			}

			return next(ctx, request)
		}
	})
}
