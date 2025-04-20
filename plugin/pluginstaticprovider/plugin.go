package pluginstaticprovider

import (
	"context"
	"errors"
	"strings"

	"github.com/hephbuild/heph/plugin/tref"

	"github.com/hephbuild/heph/plugin/gen/heph/plugin/v1/pluginv1connect"

	"connectrpc.com/connect"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

type Target struct {
	Spec *pluginv1.TargetSpec
}

var _ pluginv1connect.ProviderHandler = (*Plugin)(nil)

type Plugin struct {
	f func() []Target
}

func (p *Plugin) Config(ctx context.Context, c *connect.Request[pluginv1.ProviderConfigRequest]) (*connect.Response[pluginv1.ProviderConfigResponse], error) {
	return connect.NewResponse(&pluginv1.ProviderConfigResponse{
		Name: "static",
	}), nil
}

func (p *Plugin) Probe(ctx context.Context, c *connect.Request[pluginv1.ProbeRequest]) (*connect.Response[pluginv1.ProbeResponse], error) {
	return connect.NewResponse(&pluginv1.ProbeResponse{}), nil
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

func (p *Plugin) GetSpecs(ctx context.Context, req *connect.Request[pluginv1.GetSpecsRequest], res *connect.ServerStream[pluginv1.GetSpecsResponse]) error {
	return connect.NewError(connect.CodeUnimplemented, errors.New("not implemented"))
}

func (p *Plugin) List(ctx context.Context, req *connect.Request[pluginv1.ListRequest], res *connect.ServerStream[pluginv1.ListResponse]) error {
	for _, target := range p.f() {
		if req.Msg.GetPackage() != "" {
			if req.Msg.GetDeep() {
				if !strings.HasPrefix(target.Spec.GetRef().GetPackage(), req.Msg.GetPackage()) {
					continue
				}
			} else {
				if target.Spec.GetRef().GetPackage() != req.Msg.GetPackage() {
					continue
				}
			}
		}

		err := res.Send(&pluginv1.ListResponse{
			Of: &pluginv1.ListResponse_Ref{
				Ref: target.Spec.GetRef(),
			},
		})
		if err != nil {
			return err
		}
	}

	return nil
}

func (p *Plugin) Get(ctx context.Context, req *connect.Request[pluginv1.GetRequest]) (*connect.Response[pluginv1.GetResponse], error) {
	for _, target := range p.f() {
		if tref.Equal(target.Spec.GetRef(), req.Msg.GetRef()) {
			continue
		}

		return connect.NewResponse(&pluginv1.GetResponse{
			Spec: target.Spec,
		}), nil
	}

	return nil, connect.NewError(connect.CodeNotFound, errors.New("not found"))
}
