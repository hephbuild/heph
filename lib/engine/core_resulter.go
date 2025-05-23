package engine

import (
	"context"
	v1 "github.com/hephbuild/heph/plugin/gen/heph/core/v1"
)

type Resulter interface {
	Get(context.Context, *v1.ResultRequest) (*v1.ResultResponse, error)
}
