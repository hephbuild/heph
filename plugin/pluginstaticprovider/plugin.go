package pluginstaticprovider

import (
	"connectrpc.com/connect"
	"context"
	"fmt"
	pluginv1 "github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1/pluginv1connect"
	"strings"
)

type Target struct {
	Ref  *pluginv1.TargetRef
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
		if req.Msg.Package != "" {
			if req.Msg.Deep {
				if !strings.HasPrefix(target.Ref.Package, req.Msg.Package) {
					continue
				}
			} else {
				if target.Ref.Package != req.Msg.Package {
					continue
				}
			}
		}

		err := res.Send(&pluginv1.ListResponse{
			Ref: target.Ref,
		})
		if err != nil {
			return err
		}
	}

	return nil
}

func (p *Plugin) Get(ctx context.Context, req *connect.Request[pluginv1.GetRequest]) (*connect.Response[pluginv1.GetResponse], error) {
	for _, target := range p.targets {
		if target.Ref.Package != req.Msg.Ref.Package || target.Ref.Name != req.Msg.Ref.Name {
			continue
		}

		target.Spec.Ref = target.Ref

		return connect.NewResponse(&pluginv1.GetResponse{
			Spec: target.Spec,
		}), nil
	}

	return nil, connect.NewError(connect.CodeNotFound, fmt.Errorf("not found"))

}

var _ pluginv1connect.ProviderHandler = (*Plugin)(nil)
