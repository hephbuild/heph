package pluginfs

import (
	"context"
	"errors"
	"fmt"
	"path/filepath"
	"strings"

	"connectrpc.com/connect"
	"github.com/hephbuild/heph/lib/engine"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/gen/heph/plugin/v1/pluginv1connect"
	"github.com/hephbuild/heph/plugin/tref"
	"google.golang.org/protobuf/types/known/structpb"
)

var _ pluginv1connect.ProviderHandler = (*Provider)(nil)
var _ engine.PluginIniter = (*Provider)(nil)

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

func (p *Provider) Config(ctx context.Context, c *connect.Request[pluginv1.ProviderConfigRequest]) (*connect.Response[pluginv1.ProviderConfigResponse], error) {
	return connect.NewResponse(&pluginv1.ProviderConfigResponse{
		Name: "fs",
	}), nil
}

func (p *Provider) Probe(ctx context.Context, c *connect.Request[pluginv1.ProbeRequest]) (*connect.Response[pluginv1.ProbeResponse], error) {
	return connect.NewResponse(&pluginv1.ProbeResponse{}), nil
}

func (p *Provider) GetSpecs(ctx context.Context, req *connect.Request[pluginv1.GetSpecsRequest], res *connect.ServerStream[pluginv1.GetSpecsResponse]) error {
	return connect.NewError(connect.CodeUnimplemented, errors.New("not implemented"))
}

func (p *Provider) List(ctx context.Context, req *connect.Request[pluginv1.ListRequest], res *connect.ServerStream[pluginv1.ListResponse]) error {
	return nil
}

func (p *Provider) Get(ctx context.Context, req *connect.Request[pluginv1.GetRequest]) (*connect.Response[pluginv1.GetResponse], error) {
	rest, ok := strings.CutPrefix(req.Msg.GetRef().GetPackage(), "@heph/file/")
	if !ok {
		return nil, connect.NewError(connect.CodeNotFound, errors.New("not found"))
	}

	return connect.NewResponse(&pluginv1.GetResponse{
		Spec: &pluginv1.TargetSpec{
			Ref: tref.WithDriver(req.Msg.GetRef(), "sh"),
			Config: map[string]*structpb.Value{
				"run": structpb.NewStringValue(fmt.Sprintf("cp $ROOTDIR/%v $OUT", rest)),
				"out": structpb.NewStringValue(filepath.Base(rest)),
			},
		},
	}), nil
}
