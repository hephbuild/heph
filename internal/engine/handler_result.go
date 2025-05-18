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

func (e *Engine) ResultHandler() corev1connect.ResultServiceHandler {
	return &resultServiceHandler{Engine: e}
}

type resultServiceHandler struct {
	*Engine
}

var tracer = otel.Tracer("heph/engine")

var GlobalResolveCache = &ResolveCache{}

func (r resultServiceHandler) Get(ctx context.Context, req *connect.Request[corev1.ResultRequest]) (*connect.Response[corev1.ResultResponse], error) {
	rc := GlobalResolveCache

	var res *ExecuteResultLocks
	var err error
	switch kind := req.Msg.GetOf().(type) {
	case *corev1.ResultRequest_Ref:
		res, err = r.ResultFromRef(ctx, kind.Ref, []string{AllOutputs}, ResultOptions{}, rc)
		if err != nil {
			return nil, err
		}
	case *corev1.ResultRequest_Spec:
		res, err = r.ResultFromSpec(ctx, kind.Spec, []string{AllOutputs}, ResultOptions{}, rc)
		if err != nil {
			return nil, err
		}
	default:
		return nil, fmt.Errorf("unexpected message type: %T", kind)
	}
	// TODO: this is in the wrong place, the caller should be responsible for releasing the locks
	defer res.Unlock(ctx)

	artifacts := make([]*pluginv1.Artifact, 0, len(res.Artifacts))
	for _, artifact := range res.Artifacts {
		artifacts = append(artifacts, artifact.Artifact)
	}

	return connect.NewResponse(&corev1.ResultResponse{
		Artifacts: artifacts,
		Def:       res.Def.TargetDef.TargetDef,
	}), nil
}
