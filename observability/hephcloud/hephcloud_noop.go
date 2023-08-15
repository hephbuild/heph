//go:build nocloud

package hephcloud

import (
	"context"
	"github.com/Khan/genqlient/graphql"
	"github.com/hephbuild/heph/observability"
	"github.com/hephbuild/heph/scheduler"
)

type Hook struct {
	observability.BaseHook
	Config    scheduler.Config
	Client    graphql.Client
	ProjectID string
}

func (h *Hook) Start(ctx context.Context) func() {
	return func() {}
}

func (h *Hook) GetFlowID() string {
	return ""
}
