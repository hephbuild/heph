package engine

import (
	"context"
	"errors"
	"fmt"

	"github.com/google/uuid"
	"github.com/hephbuild/heph/lib/pluginsdk"
	corev1 "github.com/hephbuild/heph/plugin/gen/heph/core/v1"
	sync_map "github.com/zolstein/sync-map"
	"go.opentelemetry.io/otel"
)

func (e *Engine) Resulter() pluginsdk.EngineResulter {
	return &resulterHandler{Engine: e}
}

type resulterHandler struct {
	*Engine

	m sync_map.Map[string, *ExecuteResultLocks]
}

var tracer = otel.Tracer("heph/engine")

func (r *resulterHandler) Get(ctx context.Context, req *corev1.ResultRequest) (*pluginsdk.GetResult, error) {
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

	id := uuid.New().String()
	r.m.Store(id, res)

	artifacts := make([]pluginsdk.Artifact, 0, len(res.Artifacts))
	for _, artifact := range res.Artifacts {
		artifacts = append(artifacts, artifact)
	}

	return &pluginsdk.GetResult{
		Release: func() {
			res.Unlock(ctx)
		},
		Artifacts: artifacts,
		Def:       res.Def.TargetDef.TargetDef,
	}, nil
}
