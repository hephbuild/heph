package pluginfs

import (
	"context"
	"errors"
	"github.com/hephbuild/heph/internal/htypes"
	"path/filepath"

	"github.com/hephbuild/heph/lib/tref"

	"connectrpc.com/connect"
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

const Name = "fs"

func (p *Provider) Config(ctx context.Context, c *pluginv1.ProviderConfigRequest) (*pluginv1.ProviderConfigResponse, error) {
	return pluginv1.ProviderConfigResponse_builder{
		Name: htypes.Ptr(Name),
	}.Build(), nil
}

func (p *Provider) Probe(ctx context.Context, c *pluginv1.ProbeRequest) (*pluginv1.ProbeResponse, error) {
	return &pluginv1.ProbeResponse{}, nil
}

func (p *Provider) List(ctx context.Context, req *pluginv1.ListRequest) (pluginsdk.HandlerStreamReceive[*pluginv1.ListResponse], error) {
	return pluginsdk.NewNoopChanHandlerStream[*pluginv1.ListResponse](), nil
}

func (p *Provider) Get(ctx context.Context, req *pluginv1.GetRequest) (*pluginv1.GetResponse, error) {
	rest, ok := tref.CutPackagePrefix(req.GetRef().GetPackage(), "@heph/file")
	if !ok {
		return nil, pluginsdk.ErrNotFound
	}

	f := req.GetRef().GetArgs()["f"]
	if f == "" {
		return nil, connect.NewError(connect.CodeInvalidArgument, errors.New("missing f argument"))
	}

	return pluginv1.GetResponse_builder{
		Spec: pluginv1.TargetSpec_builder{
			Ref:    req.GetRef(),
			Driver: htypes.Ptr("fs_driver"),
			Config: map[string]*structpb.Value{
				"file": structpb.NewStringValue(filepath.Join(rest, f)),
			},
		}.Build(),
	}.Build(), nil
}
