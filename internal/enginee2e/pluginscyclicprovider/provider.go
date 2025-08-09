package plugincyclicprovider

import (
	"context"
	"github.com/hephbuild/heph/internal/htypes"
	"github.com/hephbuild/heph/lib/tref"

	"github.com/hephbuild/heph/lib/pluginsdk"

	"github.com/hephbuild/heph/internal/engine"

	corev1 "github.com/hephbuild/heph/plugin/gen/heph/core/v1"
	"google.golang.org/protobuf/types/known/structpb"

	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

var _ pluginsdk.Provider = (*Provider)(nil)
var _ pluginsdk.Initer = (*Provider)(nil)

const ProviderName = "cyclic_provider_test"

func New() *Provider {
	return &Provider{}
}

type Provider struct {
	resultClient pluginsdk.Engine
}

func (p *Provider) targets() []*pluginv1.TargetSpec {
	return []*pluginv1.TargetSpec{
		pluginv1.TargetSpec_builder{
			Ref: pluginv1.TargetRef_builder{
				Package: htypes.Ptr("some/package"),
				Name:    htypes.Ptr("t1"),
			}.Build(),
			Driver: htypes.Ptr("sh"),
			Config: map[string]*structpb.Value{
				"out": structpb.NewStringValue("t1"),
				"run": structpb.NewStringValue(`echo t1 > $OUT`),
			},
		}.Build(),
		pluginv1.TargetSpec_builder{
			Ref: pluginv1.TargetRef_builder{
				Package: htypes.Ptr("some/package"),
				Name:    htypes.Ptr("t2"),
			}.Build(),
			Driver: htypes.Ptr("sh"),
			Config: map[string]*structpb.Value{
				"out":     structpb.NewStringValue("t2"),
				"run":     structpb.NewStringValue(`echo t2 > $OUT`),
				"codegen": structpb.NewStringValue("copy"),
			},
			Labels: []string{"gen"},
		}.Build(),
	}
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
	return pluginsdk.NewChanHandlerStreamFunc(func(send func(*pluginv1.ListResponse) error) error {
		for _, spec := range p.targets() {
			if spec.GetRef().GetPackage() != req.GetPackage() {
				continue
			}

			err := send(pluginv1.ListResponse_builder{
				Spec: spec,
			}.Build())
			if err != nil {
				return err
			}
		}

		return nil
	}), nil
}

func (p *Provider) Get(ctx context.Context, req *pluginv1.GetRequest) (*pluginv1.GetResponse, error) {
	if req.GetRef().GetPackage() != "some/package" {
		return nil, pluginsdk.ErrNotFound
	}

	_, err := p.resultClient.ResultClient.Get(ctx, corev1.ResultRequest_builder{
		RequestId: htypes.Ptr(req.GetRequestId()),
		Spec: pluginv1.TargetSpec_builder{
			Ref: pluginv1.TargetRef_builder{
				Package: htypes.Ptr("some/package"),
				Name:    htypes.Ptr("think"),
			}.Build(),
			Driver: htypes.Ptr("sh"),
			Config: map[string]*structpb.Value{
				"deps": structpb.NewStringValue("//@heph/query:query@label=gen,tree_output_to=some/package"),
			},
		}.Build(),
	}.Build())
	if err != nil {
		return nil, err
	}

	for _, spec := range p.targets() {
		if tref.Equal(spec.GetRef(), req.GetRef()) {
			return pluginv1.GetResponse_builder{
				Spec: spec,
			}.Build(), nil
		}
	}

	return nil, pluginsdk.ErrNotFound
}

func (p *Provider) Probe(ctx context.Context, req *pluginv1.ProbeRequest) (*pluginv1.ProbeResponse, error) {
	return &pluginv1.ProbeResponse{}, nil
}
