package pluginstaticprovider

import (
	"context"
	"errors"

	"github.com/hephbuild/heph/lib/tref"

	"connectrpc.com/connect"
	"github.com/hephbuild/heph/lib/pluginsdk"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

type Target struct {
	Spec *pluginv1.TargetSpec
}

var _ pluginsdk.Provider = (*Plugin)(nil)

type Plugin struct {
	f func() []Target
}

func (p *Plugin) Config(ctx context.Context, c *pluginv1.ProviderConfigRequest) (*pluginv1.ProviderConfigResponse, error) {
	return &pluginv1.ProviderConfigResponse{
		Name: "static",
	}, nil
}

func (p *Plugin) Probe(ctx context.Context, c *pluginv1.ProbeRequest) (*pluginv1.ProbeResponse, error) {
	return &pluginv1.ProbeResponse{}, nil
}

func New(targets []Target) *Plugin {
	return NewFunc(func() []Target {
		return targets
	})
}

func NewFunc(f func() []Target) *Plugin {
	return &Plugin{
		f: f,
	}
}

func (p *Plugin) List(ctx context.Context, req *pluginv1.ListRequest) (pluginsdk.HandlerStreamReceive[*pluginv1.ListResponse], error) {
	return pluginsdk.NewChanHandlerStreamFunc(func(send func(*pluginv1.ListResponse) error) error {
		for _, target := range p.f() {
			if req.GetPackage() != "" {
				if target.Spec.GetRef().GetPackage() != req.GetPackage() {
					continue
				}
			}

			err := send(&pluginv1.ListResponse{
				Of: &pluginv1.ListResponse_Ref{
					Ref: target.Spec.GetRef(),
				},
			})
			if err != nil {
				return err
			}
		}

		return nil
	}), nil
}

func (p *Plugin) Get(ctx context.Context, req *pluginv1.GetRequest) (*pluginv1.GetResponse, error) {
	for _, target := range p.f() {
		if !tref.Equal(target.Spec.GetRef(), req.GetRef()) {
			continue
		}

		return &pluginv1.GetResponse{
			Spec: target.Spec,
		}, nil
	}

	return nil, connect.NewError(connect.CodeNotFound, errors.New("not found"))
}
