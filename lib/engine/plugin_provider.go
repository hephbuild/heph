package engine

import (
	"connectrpc.com/connect"
	"context"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/gen/heph/plugin/v1/pluginv1connect"
)

type Provider interface {
	Config(ctx context.Context, req *pluginv1.ProviderConfigRequest) (*pluginv1.ProviderConfigResponse, error)
	List(ctx context.Context, req *pluginv1.ListRequest) (HandlerStreamReceive[*pluginv1.ListResponse], error)
	Get(ctx context.Context, req *pluginv1.GetRequest) (*pluginv1.GetResponse, error)
	GetSpecs(ctx context.Context, req *pluginv1.GetSpecsRequest) (HandlerStreamReceive[*pluginv1.GetSpecsResponse], error)
	Probe(ctx context.Context, req *pluginv1.ProbeRequest) (*pluginv1.ProbeResponse, error)
}

var _ Provider = (*providerConnectClient)(nil)

func NewProviderConnectClient(client pluginv1connect.ProviderClient) Provider {
	return providerConnectClient{client: client}
}

type providerConnectClient struct {
	client pluginv1connect.ProviderClient
}

func (p providerConnectClient) Config(ctx context.Context, req *pluginv1.ProviderConfigRequest) (*pluginv1.ProviderConfigResponse, error) {
	res, err := p.client.Config(ctx, connect.NewRequest(req))
	if err != nil {
		return nil, err
	}

	return res.Msg, nil
}

func (p providerConnectClient) List(ctx context.Context, req *pluginv1.ListRequest) (HandlerStreamReceive[*pluginv1.ListResponse], error) {
	res, err := p.client.List(ctx, connect.NewRequest(req))
	if err != nil {
		return nil, err
	}

	return connectServerStream(res), nil
}

func (p providerConnectClient) Get(ctx context.Context, req *pluginv1.GetRequest) (*pluginv1.GetResponse, error) {
	res, err := p.client.Get(ctx, connect.NewRequest(req))
	if err != nil {
		return nil, err
	}

	return res.Msg, nil
}

func (p providerConnectClient) GetSpecs(ctx context.Context, req *pluginv1.GetSpecsRequest) (HandlerStreamReceive[*pluginv1.GetSpecsResponse], error) {
	res, err := p.client.GetSpecs(ctx, connect.NewRequest(req))
	if err != nil {
		return nil, err
	}

	return connectServerStream(res), nil
}

func (p providerConnectClient) Probe(ctx context.Context, req *pluginv1.ProbeRequest) (*pluginv1.ProbeResponse, error) {
	res, err := p.client.Probe(ctx, connect.NewRequest(req))
	if err != nil {
		return nil, err
	}

	return res.Msg, nil
}

func NewProviderConnectHandler(handler Provider) pluginv1connect.ProviderHandler {
	return providerConnectHandler{handler: handler}
}

type providerConnectHandler struct {
	handler Provider
}

func (p providerConnectHandler) Config(ctx context.Context, req *connect.Request[pluginv1.ProviderConfigRequest]) (*connect.Response[pluginv1.ProviderConfigResponse], error) {
	res, err := p.handler.Config(ctx, req.Msg)
	if err != nil {
		return nil, err
	}

	return connect.NewResponse(res), nil
}

func (p providerConnectHandler) List(ctx context.Context, req *connect.Request[pluginv1.ListRequest], res *connect.ServerStream[pluginv1.ListResponse]) error {
	strm, err := p.handler.List(ctx, req.Msg)
	if err != nil {
		return err
	}
	defer strm.CloseReceive()

	for strm.Receive() {
		msg := strm.Msg()
		err := res.Send(msg)
		if err != nil {
			return err
		}
	}
	if err := strm.Err(); err != nil {
		return err
	}

	return nil
}

func (p providerConnectHandler) Get(ctx context.Context, req *connect.Request[pluginv1.GetRequest]) (*connect.Response[pluginv1.GetResponse], error) {
	res, err := p.handler.Get(ctx, req.Msg)
	if err != nil {
		return nil, err
	}

	return connect.NewResponse(res), nil
}

func (p providerConnectHandler) GetSpecs(ctx context.Context, req *connect.Request[pluginv1.GetSpecsRequest], res *connect.ServerStream[pluginv1.GetSpecsResponse]) error {
	strm, err := p.handler.GetSpecs(ctx, req.Msg)
	if err != nil {
		return err
	}
	defer strm.CloseReceive()

	for strm.Receive() {
		msg := strm.Msg()
		err := res.Send(msg)
		if err != nil {
			return err
		}
	}

	return nil
}

func (p providerConnectHandler) Probe(ctx context.Context, req *connect.Request[pluginv1.ProbeRequest]) (*connect.Response[pluginv1.ProbeResponse], error) {
	res, err := p.handler.Probe(ctx, req.Msg)
	if err != nil {
		return nil, err
	}

	return connect.NewResponse(res), nil
}

var _ pluginv1connect.ProviderHandler = (*providerConnectHandler)(nil)
