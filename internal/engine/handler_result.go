package engine

import (
	"context"

	"connectrpc.com/connect"
	corev1 "github.com/hephbuild/heph/plugin/gen/heph/core/v1"
	"github.com/hephbuild/heph/plugin/gen/heph/core/v1/corev1connect"
)

func (e *Engine) ResultHandler() corev1connect.ResultServiceHandler {
	return &resultServiceHandler{Engine: e}
}

type resultServiceHandler struct {
	*Engine
}

func (r resultServiceHandler) Get(ctx context.Context, req *connect.Request[corev1.ResultRequest]) (*connect.Response[corev1.ResultResponse], error) {
	res, err := r.Resulter().Get(ctx, req.Msg)
	if err != nil {
		return nil, err
	}

	return connect.NewResponse(res), nil
}
