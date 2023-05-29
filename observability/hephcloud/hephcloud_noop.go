//go:build nocloud

package hephcloud

import (
	"context"
	"github.com/Khan/genqlient/graphql"
	"github.com/hephbuild/heph/engine"
	"github.com/hephbuild/heph/observability"
)

type Hook struct {
	observability.BaseHook
	Config    engine.Config
	Client    graphql.Client
	ProjectID string
}

func (h *Hook) Start(ctx context.Context) func() {
	return func() {}
}

func (h *Hook) GetFlowID() string {
	return ""
}
