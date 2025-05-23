package engine

import (
	"connectrpc.com/connect"
	"context"
	"errors"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/gen/heph/plugin/v1/pluginv1connect"
)

type Driver interface {
	Config(context.Context, *pluginv1.ConfigRequest) (*pluginv1.ConfigResponse, error)
	Parse(context.Context, *pluginv1.ParseRequest) (*pluginv1.ParseResponse, error)
	Run(context.Context, *pluginv1.RunRequest) (*pluginv1.RunResponse, error)
	Pipe(context.Context, *pluginv1.PipeRequest) (*pluginv1.PipeResponse, error)
}

var _ Driver = (*driverConnectClient)(nil)

func NewDriverConnectClient(client pluginv1connect.DriverClient) Driver {
	return driverConnectClient{client: client}
}

type driverConnectClient struct {
	client pluginv1connect.DriverClient
}

func (p driverConnectClient) Config(ctx context.Context, req *pluginv1.ConfigRequest) (*pluginv1.ConfigResponse, error) {
	res, err := p.client.Config(ctx, connect.NewRequest(req))
	if err != nil {
		return nil, p.handleErr(ctx, err)
	}

	return res.Msg, nil
}

func (p driverConnectClient) Parse(ctx context.Context, req *pluginv1.ParseRequest) (*pluginv1.ParseResponse, error) {
	res, err := p.client.Parse(ctx, connect.NewRequest(req))
	if err != nil {
		return nil, p.handleErr(ctx, err)
	}

	return res.Msg, nil

}

func (p driverConnectClient) Run(ctx context.Context, req *pluginv1.RunRequest) (*pluginv1.RunResponse, error) {
	res, err := p.client.Run(ctx, connect.NewRequest(req))
	if err != nil {
		return nil, p.handleErr(ctx, err)
	}

	return res.Msg, nil

}

func (p driverConnectClient) Pipe(ctx context.Context, req *pluginv1.PipeRequest) (*pluginv1.PipeResponse, error) {
	res, err := p.client.Pipe(ctx, connect.NewRequest(req))
	if err != nil {
		return nil, p.handleErr(ctx, err)
	}

	return res.Msg, nil
}

func (p driverConnectClient) handleErr(ctx context.Context, err error) error {
	if connect.CodeOf(err) == connect.CodeUnimplemented {
		return ErrNotImplemented
	}
	if connect.CodeOf(err) == connect.CodeNotFound {
		return ErrNotFound
	}

	return err
}

func NewDriverConnectHandler(handler Driver) pluginv1connect.DriverHandler {
	return driverConnectHandler{handler: handler}
}

type driverConnectHandler struct {
	handler Driver
}

func (p driverConnectHandler) Config(ctx context.Context, req *connect.Request[pluginv1.ConfigRequest]) (*connect.Response[pluginv1.ConfigResponse], error) {
	res, err := p.handler.Config(ctx, req.Msg)
	if err != nil {
		return nil, p.handleErr(ctx, err)
	}

	return connect.NewResponse(res), nil
}

func (p driverConnectHandler) Parse(ctx context.Context, req *connect.Request[pluginv1.ParseRequest]) (*connect.Response[pluginv1.ParseResponse], error) {
	res, err := p.handler.Parse(ctx, req.Msg)
	if err != nil {
		return nil, p.handleErr(ctx, err)
	}

	return connect.NewResponse(res), nil
}

func (p driverConnectHandler) Run(ctx context.Context, req *connect.Request[pluginv1.RunRequest]) (*connect.Response[pluginv1.RunResponse], error) {
	res, err := p.handler.Run(ctx, req.Msg)
	if err != nil {
		return nil, p.handleErr(ctx, err)
	}

	return connect.NewResponse(res), nil
}

func (p driverConnectHandler) Pipe(ctx context.Context, req *connect.Request[pluginv1.PipeRequest]) (*connect.Response[pluginv1.PipeResponse], error) {
	res, err := p.handler.Pipe(ctx, req.Msg)
	if err != nil {
		return nil, p.handleErr(ctx, err)
	}

	return connect.NewResponse(res), nil
}

func (p driverConnectHandler) handleErr(ctx context.Context, err error) error {
	if errors.Is(err, ErrNotImplemented) {
		return connect.NewError(connect.CodeUnimplemented, err)
	}
	if errors.Is(err, ErrNotFound) {
		return connect.NewError(connect.CodeNotFound, err)
	}

	return err
}

var _ pluginv1connect.DriverHandler = (*driverConnectHandler)(nil)
