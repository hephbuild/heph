package pluginfs

import (
	"context"

	"github.com/hephbuild/heph/internal/hproto/hstructpb"
	"github.com/hephbuild/heph/internal/htypes"

	"github.com/hephbuild/heph/lib/tref"

	"github.com/hephbuild/heph/lib/pluginsdk"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"google.golang.org/protobuf/types/known/structpb"
)

var _ pluginsdk.Initer = (*Provider)(nil)
var _ pluginsdk.Provider = (*Provider)(nil)

type Provider struct {
	resultClient pluginsdk.Engine
}

func (p *Provider) PluginInit(ctx context.Context, init pluginsdk.InitPayload) error {
	p.resultClient = init.Engine

	return nil
}

func NewProvider() *Provider {
	return &Provider{}
}

const NameProvider = "fs"

func (p *Provider) Config(ctx context.Context, c *pluginv1.ProviderConfigRequest) (*pluginv1.ProviderConfigResponse, error) {
	return pluginv1.ProviderConfigResponse_builder{
		Name: htypes.Ptr(NameProvider),
	}.Build(), nil
}

func (p *Provider) Probe(ctx context.Context, c *pluginv1.ProbeRequest) (*pluginv1.ProbeResponse, error) {
	return &pluginv1.ProbeResponse{}, nil
}

func (p *Provider) List(ctx context.Context, req *pluginv1.ListRequest) (pluginsdk.HandlerStreamReceive[*pluginv1.ListResponse], error) {
	return pluginsdk.NewNoopChanHandlerStream[*pluginv1.ListResponse](), nil
}

func (p *Provider) Get(ctx context.Context, req *pluginv1.GetRequest) (*pluginv1.GetResponse, error) {
	if path, ok := tref.ParseFile(req.GetRef()); ok {
		return pluginv1.GetResponse_builder{
			Spec: pluginv1.TargetSpec_builder{
				Ref:    req.GetRef(),
				Driver: htypes.Ptr(DriverName),
				Config: map[string]*structpb.Value{
					"file": structpb.NewStringValue(path),
				},
			}.Build(),
		}.Build(), nil
	}

	if path, exclude, ok := tref.ParseGlob(req.GetRef()); ok {
		return pluginv1.GetResponse_builder{
			Spec: pluginv1.TargetSpec_builder{
				Ref:    req.GetRef(),
				Driver: htypes.Ptr(DriverName),
				Config: map[string]*structpb.Value{
					"glob":    structpb.NewStringValue(path),
					"exclude": hstructpb.NewStringsValue(exclude),
				},
			}.Build(),
		}.Build(), nil
	}

	return nil, pluginsdk.ErrNotFound
}
