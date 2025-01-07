package pluginsmartprovidertest

import (
	"context"
	"fmt"

	"github.com/hephbuild/heph/internal/engine"

	"github.com/hephbuild/heph/internal/hartifact"
	corev1 "github.com/hephbuild/heph/plugin/gen/heph/core/v1"
	"google.golang.org/protobuf/types/known/structpb"

	"connectrpc.com/connect"
	"github.com/hephbuild/heph/plugin/gen/heph/core/v1/corev1connect"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/gen/heph/plugin/v1/pluginv1connect"
)

var _ pluginv1connect.ProviderHandler = (*Provider)(nil)
var _ engine.PluginIniter = (*Provider)(nil)

const ProviderName = "smart_provider_test"

func New() *Provider {
	return &Provider{}
}

type Provider struct {
	resultClient corev1connect.ResultServiceClient
}

func (p *Provider) PluginInit(ctx context.Context, init engine.PluginInit) error {
	p.resultClient = init.CoreHandle.ResultClient

	return nil
}

func (p *Provider) Config(ctx context.Context, req *connect.Request[pluginv1.ProviderConfigRequest]) (*connect.Response[pluginv1.ProviderConfigResponse], error) {
	return connect.NewResponse(&pluginv1.ProviderConfigResponse{
		Name: ProviderName,
	}), nil
}

func (p *Provider) List(ctx context.Context, req *connect.Request[pluginv1.ListRequest], stream *connect.ServerStream[pluginv1.ListResponse]) error {
	return nil
}

func (p *Provider) Get(ctx context.Context, req *connect.Request[pluginv1.GetRequest]) (*connect.Response[pluginv1.GetResponse], error) {
	res, err := p.resultClient.Get(ctx, connect.NewRequest(&corev1.ResultRequest{
		Of: &corev1.ResultRequest_Spec{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: "some/package",
					Name:    "think",
					Driver:  "bash",
				},
				Config: map[string]*structpb.Value{
					"out": structpb.NewStringValue("out"),
					"run": structpb.NewStringValue(`echo hello > $OUT`),
				},
			},
		},
	}))
	if err != nil {
		return nil, err
	}

	artifacts := hartifact.FindOutputs(res.Msg.GetArtifacts(), "")

	b, err := hartifact.ReadAll(ctx, artifacts[0], "some/package/out")
	if err != nil {
		return nil, err
	}

	req.Msg.Ref.Driver = "bash"

	return connect.NewResponse(&pluginv1.GetResponse{
		Spec: &pluginv1.TargetSpec{
			Ref: req.Msg.GetRef(),
			Config: map[string]*structpb.Value{
				"out": structpb.NewStringValue("out"),
				"run": structpb.NewStringValue(fmt.Sprintf(`echo 'parent: %s' > $OUT`, b)),
			},
		},
	}), nil
}

func (p *Provider) Probe(ctx context.Context, req *connect.Request[pluginv1.ProbeRequest]) (*connect.Response[pluginv1.ProbeResponse], error) {
	return connect.NewResponse(&pluginv1.ProbeResponse{}), nil
}
