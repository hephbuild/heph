package pluginsmartprovidertest

import (
	"context"
	"fmt"
	"io"

	"github.com/hephbuild/heph/internal/htypes"
	"github.com/hephbuild/heph/lib/tref"

	"github.com/hephbuild/heph/lib/pluginsdk"

	"github.com/hephbuild/heph/internal/engine"

	"github.com/hephbuild/heph/internal/hartifact"
	corev1 "github.com/hephbuild/heph/plugin/gen/heph/core/v1"
	"google.golang.org/protobuf/types/known/structpb"

	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

var _ pluginsdk.Provider = (*Provider)(nil)
var _ pluginsdk.Initer = (*Provider)(nil)

const ProviderName = "smart_provider_test"

func New() *Provider {
	return &Provider{}
}

type Provider struct {
	resultClient pluginsdk.Engine
}

func (p *Provider) PluginInit(ctx context.Context, init engine.PluginInit) error {
	p.resultClient = init.Engine

	return nil
}

func (p *Provider) Config(ctx context.Context, req *pluginv1.ProviderConfigRequest) (*pluginv1.ProviderConfigResponse, error) {
	return pluginv1.ProviderConfigResponse_builder{
		Name: htypes.Ptr(ProviderName),
	}.Build(), nil
}

func (p *Provider) List(ctx context.Context, req *pluginv1.ListRequest) (pluginsdk.HandlerStreamReceive[*pluginv1.ListResponse], error) {
	return pluginsdk.NewNoopChanHandlerStream[*pluginv1.ListResponse](), nil
}

func (p *Provider) Get(ctx context.Context, req *pluginv1.GetRequest) (*pluginv1.GetResponse, error) {
	res, err := p.resultClient.ResultClient.Get(ctx, corev1.ResultRequest_builder{
		RequestId: htypes.Ptr(req.GetRequestId()),
		Spec: pluginv1.TargetSpec_builder{
			Ref: tref.New("some/package", "think", nil),
			Driver: htypes.Ptr("bash"),
			Config: map[string]*structpb.Value{
				"out": structpb.NewStringValue("out"),
				"run": structpb.NewStringValue(`echo hello > $OUT`),
			},
		}.Build(),
	}.Build())
	if err != nil {
		return nil, err
	}

	artifacts := hartifact.FindOutputs(res.GetArtifacts(), "")

	r, err := hartifact.FileReader(ctx, artifacts[0])
	if err != nil {
		return nil, err
	}
	defer r.Close()

	b, err := io.ReadAll(r)
	if err != nil {
		return nil, err
	}

	return pluginv1.GetResponse_builder{
		Spec: pluginv1.TargetSpec_builder{
			Ref:    req.GetRef(),
			Driver: htypes.Ptr("bash"),
			Config: map[string]*structpb.Value{
				"out": structpb.NewStringValue("out"),
				"run": structpb.NewStringValue(fmt.Sprintf(`echo 'parent: %s' > $OUT`, b)),
			},
		}.Build(),
	}.Build(), nil
}

func (p *Provider) Probe(ctx context.Context, req *pluginv1.ProbeRequest) (*pluginv1.ProbeResponse, error) {
	return &pluginv1.ProbeResponse{}, nil
}
