package pluginbuildfile

import (
	"context"

	"go.starlark.net/starlark"
)

const execContextKey = "__heph_ctx"

type onProviderStateFunc = func(ctx context.Context, payload OnProviderStatePayload) error
type onTargetFunc = func(ctx context.Context, payload OnTargetPayload) error

type execContext struct {
	Ctx             context.Context
	Package         string
	OnTarget        onTargetFunc
	OnProviderState onProviderStateFunc
}

func getExecContext(thread *starlark.Thread) execContext {
	return thread.Local(execContextKey).(execContext) //nolint:errcheck
}

func setExecContext(thread *starlark.Thread, ctx execContext) {
	thread.SetLocal(execContextKey, ctx)
}
