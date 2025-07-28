package engine

import (
	"context"
	"errors"
	"fmt"

	"github.com/hephbuild/heph/lib/pluginsdk"
	corev1 "github.com/hephbuild/heph/plugin/gen/heph/core/v1"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"go.opentelemetry.io/otel"
)

func (e *Engine) Resulter() pluginsdk.Resulter {
	return &resulterHandler{Engine: e}
}

type resulterHandler struct {
	*Engine
}

var tracer = otel.Tracer("heph/engine")

func (r resulterHandler) Get(ctx context.Context, req *corev1.ResultRequest) (*corev1.ResultResponse, error) {
	rs, err := r.GetRequestState(req.GetRequestId())
	if err != nil {
		return nil, err
	}

	var res *ExecuteResultLocks
	switch kind := req.WhichOf(); kind {
	case corev1.ResultRequest_Ref_case:
		res, err = r.ResultFromRef(ctx, rs, req.GetRef(), []string{AllOutputs})
		if err != nil {
			var serr StackRecursionError
			if errors.Is(err, &serr) {
				return nil, pluginsdk.StackRecursionError{Stack: serr.Print()}
			}

			return nil, err
		}
	case corev1.ResultRequest_Spec_case:
		res, err = r.ResultFromSpec(ctx, rs, req.GetSpec(), []string{AllOutputs})
		if err != nil {
			var serr StackRecursionError
			if errors.Is(err, &serr) {
				return nil, pluginsdk.StackRecursionError{Stack: serr.Print()}
			}

			return nil, err
		}
	default:
		return nil, fmt.Errorf("unexpected message type: %v", kind)
	}
	// TODO: this is in the wrong place, the caller should be responsible for releasing the locks
	defer res.Unlock(ctx)

	artifacts := make([]*pluginv1.Artifact, 0, len(res.Artifacts))
	for _, artifact := range res.Artifacts {
		artifacts = append(artifacts, artifact.Artifact)
	}

	return corev1.ResultResponse_builder{
		Artifacts: artifacts,
		Def:       res.Def.TargetDef.TargetDef,
	}.Build(), nil
}
