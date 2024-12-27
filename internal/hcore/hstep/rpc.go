package hstep

import (
	"connectrpc.com/connect"
	"context"
	"github.com/hephbuild/hephv2/plugin/gen/heph/core/v1"
	"github.com/hephbuild/hephv2/plugin/gen/heph/core/v1/corev1connect"
)

type rpcHandler struct {
	handler Handler
}

func (r rpcHandler) Create(ctx context.Context, req *connect.Request[corev1.StepServiceCreateRequest]) (*connect.Response[corev1.StepServiceCreateResponse], error) {
	step := r.handler(ctx, req.Msg.Step)

	return connect.NewResponse(&corev1.StepServiceCreateResponse{Step: step}), nil
}

func NewHandler(handler Handler) corev1connect.StepServiceHandler {
	return rpcHandler{handler: handler}
}
