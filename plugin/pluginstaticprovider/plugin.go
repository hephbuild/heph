package pluginstaticprovider

import (
	"context"
	"errors"
	"strings"

	"connectrpc.com/connect"
	pluginv1 "github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1/pluginv1connect"
)

type Target struct {
	Spec *pluginv1.TargetSpec
}

type Plugin struct {
	targets []Target
}

func New(targets []Target) *Plugin {
	return &Plugin{
		targets: targets,
	}
}

func (p *Plugin) List(ctx context.Context, req *connect.Request[pluginv1.ListRequest], res *connect.ServerStream[pluginv1.ListResponse]) error {
	for _, target := range p.targets {
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
			Ref: target.Spec.GetRef(),
		})
		if err != nil {
			return err
		}
	}

	return nil
}

func (p *Plugin) Get(ctx context.Context, req *connect.Request[pluginv1.GetRequest]) (*connect.Response[pluginv1.GetResponse], error) {
	for _, target := range p.targets {
		if target.Spec.GetRef().GetPackage() != req.Msg.GetRef().GetPackage() || target.Spec.GetRef().GetName() != req.Msg.GetRef().GetName() {
			continue
		}

		return connect.NewResponse(&pluginv1.GetResponse{
			Spec: target.Spec,
		}), nil
	}

	return nil, connect.NewError(connect.CodeNotFound, errors.New("not found"))
}

var _ pluginv1connect.ProviderHandler = (*Plugin)(nil)
