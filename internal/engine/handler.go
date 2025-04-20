package engine

import (
	"context"
	"fmt"

	"go.opentelemetry.io/otel"

	"connectrpc.com/connect"
	corev1 "github.com/hephbuild/heph/plugin/gen/heph/core/v1"
	"github.com/hephbuild/heph/plugin/gen/heph/core/v1/corev1connect"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

func (e *Engine) Handler() corev1connect.ResultServiceHandler {
	return &resultServiceHandler{Engine: e}
}

type resultServiceHandler struct {
	*Engine
}

var tracer = otel.Tracer("heph/engine")

func (r resultServiceHandler) Get(ctx context.Context, req *connect.Request[corev1.ResultRequest]) (*connect.Response[corev1.ResultResponse], error) {
	var res *ExecuteChResult
	switch kind := req.Msg.GetOf().(type) {
	case *corev1.ResultRequest_Ref:
		res = r.ResultFromRef(ctx, kind.Ref, []string{AllOutputs}, ResultOptions{})
	case *corev1.ResultRequest_Def:
		res = r.ResultFromDef(ctx, kind.Def, []string{AllOutputs}, ResultOptions{})
	case *corev1.ResultRequest_Spec:
		res = r.ResultFromSpec(ctx, kind.Spec, []string{AllOutputs}, ResultOptions{})
	default:
		return nil, fmt.Errorf("unexpected message type: %T", kind)
	}

	if res.Err != nil {
		return nil, res.Err
	}

	artifacts := make([]*pluginv1.Artifact, 0, len(res.Artifacts))
	for _, artifact := range res.Artifacts {
		artifacts = append(artifacts, artifact.Artifact)
	}

	return connect.NewResponse(&corev1.ResultResponse{
		Artifacts: artifacts,
		Def:       res.Def.TargetDef,
	}), nil
}
