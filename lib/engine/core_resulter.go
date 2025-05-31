package engine

import (
	"context"
	corev1 "github.com/hephbuild/heph/plugin/gen/heph/core/v1"
)

type Resulter interface {
	Get(context.Context, *corev1.ResultRequest) (*corev1.ResultResponse, error)
}
