package engine

import (
	"connectrpc.com/connect"
	"context"
	"errors"
	"github.com/hephbuild/heph/internal/hcore/hlog"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/gen/heph/plugin/v1/pluginv1connect"
)

type Provider interface {
	Config(ctx context.Context, req *pluginv1.ProviderConfigRequest) (*pluginv1.ProviderConfigResponse, error)
	List(ctx context.Context, req *pluginv1.ListRequest) (HandlerStreamReceive[*pluginv1.ListResponse], error)
	Get(ctx context.Context, req *pluginv1.GetRequest) (*pluginv1.GetResponse, error)
	Probe(ctx context.Context, req *pluginv1.ProbeRequest) (*pluginv1.ProbeResponse, error)
}

var _ Provider = (*providerConnectClient)(nil)

func NewProviderConnectClient(client pluginv1connect.ProviderClient) Provider {
	return providerConnectClient{client: client}
}

type providerConnectClient struct {
	client pluginv1connect.ProviderClient
}

func (p providerConnectClient) handleErr(ctx context.Context, err error) error {
	if connect.CodeOf(err) == connect.CodeUnimplemented {
		return ErrNotImplemented
	}
	if connect.CodeOf(err) == connect.CodeNotFound {
		return ErrNotFound
	}

	return err
}

func (p providerConnectClient) Config(ctx context.Context, req *pluginv1.ProviderConfigRequest) (*pluginv1.ProviderConfigResponse, error) {
	res, err := p.client.Config(ctx, connect.NewRequest(req))
	if err != nil {
		return nil, p.handleErr(ctx, err)
	}

	return res.Msg, nil
}

func (p providerConnectClient) List(ctx context.Context, req *pluginv1.ListRequest) (HandlerStreamReceive[*pluginv1.ListResponse], error) {
	res, err := p.client.List(ctx, connect.NewRequest(req))
	if err != nil {
		return nil, p.handleErr(ctx, err)
	}

	return connectServerStream(res), nil
}

func (p providerConnectClient) Get(ctx context.Context, req *pluginv1.GetRequest) (*pluginv1.GetResponse, error) {
	hardCtx, cancelHardCtx := context.WithCancel(context.WithoutCancel(ctx))
	defer cancelHardCtx()

	strm := p.client.Get(hardCtx)
	defer strm.CloseResponse()
	defer strm.CloseRequest()

	go func() {
		select {
		case <-hardCtx.Done():
			return
		case <-ctx.Done():
			err := strm.Send(&pluginv1.GetContainer{Msg: &pluginv1.GetContainer_Cancel{Cancel: true}})
			if err != nil {
				hlog.From(ctx).Error("failed to send cancel message", "err", err)
			}
		}
	}()

	err := strm.Send(&pluginv1.GetContainer{Msg: &pluginv1.GetContainer_Start{Start: req}})
	if err != nil {
		return nil, p.handleErr(ctx, err)
	}

	res, err := strm.Receive()
	if err != nil {
		return nil, p.handleErr(ctx, err)
	}

	return res, nil
}

func (p providerConnectClient) Probe(ctx context.Context, req *pluginv1.ProbeRequest) (*pluginv1.ProbeResponse, error) {
	res, err := p.client.Probe(ctx, connect.NewRequest(req))
	if err != nil {
		return nil, p.handleErr(ctx, err)
	}

	return res.Msg, nil
}

func NewProviderConnectHandler(handler Provider) pluginv1connect.ProviderHandler {
	return providerConnectHandler{handler: handler}
}

type providerConnectHandler struct {
	handler Provider
}

func (p providerConnectHandler) handleErr(ctx context.Context, err error) error {
	if errors.Is(err, ErrNotImplemented) {
		return connect.NewError(connect.CodeUnimplemented, err)
	}
	if errors.Is(err, ErrNotFound) {
		return connect.NewError(connect.CodeNotFound, err)
	}

	return err
}

func (p providerConnectHandler) Config(ctx context.Context, req *connect.Request[pluginv1.ProviderConfigRequest]) (*connect.Response[pluginv1.ProviderConfigResponse], error) {
	res, err := p.handler.Config(ctx, req.Msg)
	if err != nil {
		return nil, p.handleErr(ctx, err)
	}

	return connect.NewResponse(res), nil
}

func (p providerConnectHandler) List(ctx context.Context, req *connect.Request[pluginv1.ListRequest], res *connect.ServerStream[pluginv1.ListResponse]) error {
	strm, err := p.handler.List(ctx, req.Msg)
	if err != nil {
		return p.handleErr(ctx, err)
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

func (p providerConnectHandler) Get(ctx context.Context, strm *connect.BidiStream[pluginv1.GetContainer, pluginv1.GetResponse]) error {
	ctx, cancel := context.WithCancel(ctx)
	defer cancel()

	startCh := make(chan *pluginv1.GetRequest, 1)
	errCh := make(chan error, 1)

	go func() {
		defer cancel()

		for {
			msg, err := strm.Receive()
			if err != nil {
				errCh <- err
				return
			}

			switch msg := msg.Msg.(type) {
			case *pluginv1.GetContainer_Start:
				startCh <- msg.Start
				close(startCh)
			case *pluginv1.GetContainer_Cancel:
				cancel()
			}
		}
	}()

	select {
	case err := <-errCh:
		return err
	case startMsg := <-startCh:
		res, err := p.handler.Get(ctx, startMsg)
		if err != nil {
			return p.handleErr(ctx, err)
		}

		err = strm.Send(res)
		if err != nil {
			return err
		}

		return nil
	}
}

func (p providerConnectHandler) Probe(ctx context.Context, req *connect.Request[pluginv1.ProbeRequest]) (*connect.Response[pluginv1.ProbeResponse], error) {
	res, err := p.handler.Probe(ctx, req.Msg)
	if err != nil {
		return nil, p.handleErr(ctx, err)
	}

	return connect.NewResponse(res), nil
}

var _ pluginv1connect.ProviderHandler = (*providerConnectHandler)(nil)
