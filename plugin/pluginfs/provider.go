package pluginfs

import (
	"connectrpc.com/connect"
	"context"
	"errors"
	"github.com/hephbuild/heph/lib/engine"
	engine2 "github.com/hephbuild/heph/lib/engine"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/tref"
	"google.golang.org/protobuf/types/known/structpb"
	"path/filepath"
)

var _ engine2.PluginIniter = (*Provider)(nil)
var _ engine2.Provider = (*Provider)(nil)

type Provider struct {
	resultClient engine.EngineHandle
}

func (p *Provider) PluginInit(ctx context.Context, init engine.PluginInit) error {
	p.resultClient = init.CoreHandle

	return nil
}

func NewProvider() *Provider {
	return &Provider{}
}

const Name = "fs"

func (p *Provider) Config(ctx context.Context, c *pluginv1.ProviderConfigRequest) (*pluginv1.ProviderConfigResponse, error) {
	return &pluginv1.ProviderConfigResponse{
		Name: Name,
	}, nil
}

func (p *Provider) Probe(ctx context.Context, c *pluginv1.ProbeRequest) (*pluginv1.ProbeResponse, error) {
	return &pluginv1.ProbeResponse{}, nil
}

func (p *Provider) List(ctx context.Context, req *pluginv1.ListRequest) (engine2.HandlerStreamReceive[*pluginv1.ListResponse], error) {
	return engine2.NewChanHandlerStreamFunc(func(send func(*pluginv1.ListResponse) error) error {
		return nil
	}), nil
}

func (p *Provider) Get(ctx context.Context, req *pluginv1.GetRequest) (*pluginv1.GetResponse, error) {
	rest, ok := tref.CutPackagePrefix(req.GetRef().GetPackage(), "@heph/file")
	if !ok {
		return nil, engine2.ErrNotFound
	}

	f := req.Ref.Args["f"]
	if f == "" {
		return nil, connect.NewError(connect.CodeInvalidArgument, errors.New("missing f argument"))
	}

	return &pluginv1.GetResponse{
		Spec: &pluginv1.TargetSpec{
			Ref:    req.GetRef(),
			Driver: "fs_driver",
			Config: map[string]*structpb.Value{
				"file": structpb.NewStringValue(filepath.Join(rest, f)),
			},
		},
	}, nil
}
