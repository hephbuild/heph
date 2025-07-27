package pluginsdkconnect

import (
	"context"
	"errors"

	"connectrpc.com/connect"
	"github.com/hephbuild/heph/internal/hcore/hlog"
	"github.com/hephbuild/heph/lib/pluginsdk"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/gen/heph/plugin/v1/pluginv1connect"
	"google.golang.org/protobuf/proto"
)

var _ pluginsdk.Provider = (*providerConnectClient)(nil)

func NewProviderConnectClient(client pluginv1connect.ProviderClient) pluginsdk.Provider {
	return providerConnectClient{client: client}
}

type providerConnectClient struct {
	client pluginv1connect.ProviderClient
}

func (p providerConnectClient) handleErr(err error) error {
	if connect.CodeOf(err) == connect.CodeUnimplemented {
		return pluginsdk.ErrNotImplemented
	}

	if connect.CodeOf(err) == connect.CodeNotFound {
		return pluginsdk.ErrNotFound
	}

	if serr, ok := AsStackRecursionError(err); ok {
		return pluginsdk.StackRecursionError{Stack: serr.GetStack()}
	}

	return err
}

func (p providerConnectClient) Config(ctx context.Context, req *pluginv1.ProviderConfigRequest) (*pluginv1.ProviderConfigResponse, error) {
	res, err := p.client.Config(ctx, connect.NewRequest(req))
	if err != nil {
		return nil, p.handleErr(err)
	}

	return res.Msg, nil
}

func (p providerConnectClient) List(ctx context.Context, req *pluginv1.ListRequest) (pluginsdk.HandlerStreamReceive[*pluginv1.ListResponse], error) {
	res, err := p.client.List(ctx, connect.NewRequest(req))
	if err != nil {
		return nil, p.handleErr(err)
	}

	strm := connectServerStream(res)
	strm = pluginsdk.WithOnErr(strm, p.handleErr)

	return strm, nil
}

func (p providerConnectClient) Get(ctx context.Context, req *pluginv1.GetRequest) (*pluginv1.GetResponse, error) {
	hardCtx, cancelHardCtx := context.WithCancel(context.WithoutCancel(ctx))
	defer cancelHardCtx()

	strm := p.client.Get(hardCtx)
	defer strm.CloseResponse() //nolint:errcheck
	defer strm.CloseRequest()  //nolint:errcheck

	go func() {
		select {
		case <-hardCtx.Done():
			return
		case <-ctx.Done():
			err := strm.Send(pluginv1.GetContainer_builder{Cancel: proto.Bool(true)}.Build())
			if err != nil {
				hlog.From(ctx).Error("failed to send cancel message", "err", err)
			}
		}
	}()

	err := strm.Send(pluginv1.GetContainer_builder{Start: proto.ValueOrDefault(req)}.Build())
	if err != nil {
		return nil, p.handleErr(err)
	}

	res, err := strm.Receive()
	if err != nil {
		return nil, p.handleErr(err)
	}

	return res, nil
}

func (p providerConnectClient) Probe(ctx context.Context, req *pluginv1.ProbeRequest) (*pluginv1.ProbeResponse, error) {
	res, err := p.client.Probe(ctx, connect.NewRequest(req))
	if err != nil {
		return nil, p.handleErr(err)
	}

	return res.Msg, nil
}

func NewProviderConnectHandler(handler pluginsdk.Provider) pluginv1connect.ProviderHandler {
	return providerConnectHandler{handler: handler}
}

type providerConnectHandler struct {
	handler pluginsdk.Provider
}

func (p providerConnectHandler) handleErr(err error) error {
	if errors.Is(err, pluginsdk.ErrNotImplemented) {
		return connect.NewError(connect.CodeUnimplemented, err)
	}
	if errors.Is(err, pluginsdk.ErrNotFound) {
		return connect.NewError(connect.CodeNotFound, err)
	}
	var serr pluginsdk.StackRecursionError
	if errors.Is(err, &serr) {
		return NewStackRecursionError(serr.Stack)
	}

	return connect.NewError(connect.CodeInternal, err)
}

func (p providerConnectHandler) Config(
	ctx context.Context,
	req *connect.Request[pluginv1.ProviderConfigRequest],
) (*connect.Response[pluginv1.ProviderConfigResponse], error) {
	res, err := p.handler.Config(ctx, req.Msg)
	if err != nil {
		return nil, p.handleErr(err)
	}

	return connect.NewResponse(res), nil
}

func (p providerConnectHandler) List(ctx context.Context, req *connect.Request[pluginv1.ListRequest], res *connect.ServerStream[pluginv1.ListResponse]) error {
	strm, err := p.handler.List(ctx, req.Msg)
	if err != nil {
		return p.handleErr(err)
	}
	defer strm.CloseReceive() //nolint:errcheck

	for strm.Receive() {
		msg := strm.Msg()
		err := res.Send(msg)
		if err != nil {
			return err
		}
	}
	if err := strm.Err(); err != nil {
		return p.handleErr(err)
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

			switch msg.WhichMsg() {
			case pluginv1.GetContainer_Start_case:
				startCh <- msg.GetStart()
				close(startCh)
			case pluginv1.GetContainer_Cancel_case:
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
			return p.handleErr(err)
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
		return nil, p.handleErr(err)
	}

	return connect.NewResponse(res), nil
}

var _ pluginv1connect.ProviderHandler = (*providerConnectHandler)(nil)
