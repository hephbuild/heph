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

var _ pluginsdk.Driver = (*driverConnectClient)(nil)

func NewDriverConnectClient(client pluginv1connect.DriverClient) pluginsdk.Driver {
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
	hardCtx, cancelHardCtx := context.WithCancel(context.WithoutCancel(ctx))
	defer cancelHardCtx()

	strm := p.client.Run(hardCtx)
	defer strm.CloseResponse() //nolint:errcheck
	defer strm.CloseRequest()  //nolint:errcheck

	go func() {
		select {
		case <-hardCtx.Done():
			return
		case <-ctx.Done():
			err := strm.Send(pluginv1.RunContainer_builder{Cancel: proto.Bool(true)}.Build())
			if err != nil {
				hlog.From(ctx).Error("failed to send cancel message", "err", err)
			}
		}
	}()

	err := strm.Send(pluginv1.RunContainer_builder{Start: proto.ValueOrDefault(req)}.Build())
	if err != nil {
		return nil, p.handleErr(ctx, err)
	}

	res, err := strm.Receive()
	if err != nil {
		return nil, p.handleErr(ctx, err)
	}

	return res, nil
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
		return pluginsdk.ErrNotImplemented
	}
	if connect.CodeOf(err) == connect.CodeNotFound {
		return pluginsdk.ErrNotFound
	}

	return connect.NewError(connect.CodeInternal, err)
}

func NewDriverConnectHandler(handler pluginsdk.Driver) pluginv1connect.DriverHandler {
	return driverConnectHandler{handler: handler}
}

type driverConnectHandler struct {
	handler pluginsdk.Driver
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

func (p driverConnectHandler) Run(ctx context.Context, strm *connect.BidiStream[pluginv1.RunContainer, pluginv1.RunResponse]) error {
	ctx, cancel := context.WithCancel(ctx)
	defer cancel()

	startCh := make(chan *pluginv1.RunRequest, 1)
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
			case pluginv1.RunContainer_Start_case:
				startCh <- msg.GetStart()
				close(startCh)
			case pluginv1.RunContainer_Cancel_case:
				cancel()
			}
		}
	}()

	select {
	case err := <-errCh:
		return err
	case startMsg := <-startCh:
		res, err := p.handler.Run(ctx, startMsg)
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

func (p driverConnectHandler) Pipe(ctx context.Context, req *connect.Request[pluginv1.PipeRequest]) (*connect.Response[pluginv1.PipeResponse], error) {
	res, err := p.handler.Pipe(ctx, req.Msg)
	if err != nil {
		return nil, p.handleErr(ctx, err)
	}

	return connect.NewResponse(res), nil
}

func (p driverConnectHandler) handleErr(ctx context.Context, err error) error {
	if errors.Is(err, pluginsdk.ErrNotImplemented) {
		return connect.NewError(connect.CodeUnimplemented, err)
	}
	if errors.Is(err, pluginsdk.ErrNotFound) {
		return connect.NewError(connect.CodeNotFound, err)
	}

	return err
}

var _ pluginv1connect.DriverHandler = (*driverConnectHandler)(nil)
