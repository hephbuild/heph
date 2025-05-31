package engine

import (
	"context"
	"fmt"
	"github.com/hephbuild/heph/lib/engine"
	corev1 "github.com/hephbuild/heph/plugin/gen/heph/core/v1"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"go.opentelemetry.io/otel"
)

func (e *Engine) Resulter() engine.Resulter {
	return &resulterHandler{Engine: e}
}

type resulterHandler struct {
	*Engine
}

var tracer = otel.Tracer("heph/engine")

func (r resulterHandler) Get(ctx context.Context, req *corev1.ResultRequest) (*corev1.ResultResponse, error) {
	rs, err := r.GetRequestState(req.RequestId)
	if err != nil {
		return nil, err
	}

	var res *ExecuteResultLocks
	switch kind := req.GetOf().(type) {
	case *corev1.ResultRequest_Ref:
		res, err = r.ResultFromRef(ctx, kind.Ref, []string{AllOutputs}, ResultOptions{}, rs)
		if err != nil {
			return nil, err
		}
	case *corev1.ResultRequest_Spec:
		res, err = r.ResultFromSpec(ctx, kind.Spec, []string{AllOutputs}, ResultOptions{}, rs)
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

	return &corev1.ResultResponse{
		Artifacts: artifacts,
		Def:       res.Def.TargetDef.TargetDef,
	}, nil
}
