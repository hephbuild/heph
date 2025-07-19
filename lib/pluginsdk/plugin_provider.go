package pluginsdk

import (
	"context"
	"fmt"

	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

type Provider interface {
	Config(ctx context.Context, req *pluginv1.ProviderConfigRequest) (*pluginv1.ProviderConfigResponse, error)
	List(ctx context.Context, req *pluginv1.ListRequest) (HandlerStreamReceive[*pluginv1.ListResponse], error)
	Get(ctx context.Context, req *pluginv1.GetRequest) (*pluginv1.GetResponse, error)
	Probe(ctx context.Context, req *pluginv1.ProbeRequest) (*pluginv1.ProbeResponse, error)
}

type StackRecursionError struct {
	Stack string
}

func (e StackRecursionError) Error() string {
	return fmt.Sprintf("stack recursion detected: %v", e.Stack)
}

func (e StackRecursionError) Is(err error) bool {
	_, ok := err.(StackRecursionError)

	return ok
}
