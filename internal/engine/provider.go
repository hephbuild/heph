package engine

import (
	"context"
	"errors"

	"github.com/hephbuild/heph/internal/htypes"
	"github.com/hephbuild/heph/lib/pluginsdk"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

func (e *Engine) List(ctx context.Context, rs *RequestState, p EngineProvider, pkg string) (pluginsdk.HandlerStreamReceive[*pluginv1.ListResponse], error) {
	strm, err := p.List(ctx, pluginv1.ListRequest_builder{
		RequestId: htypes.Ptr(rs.ID),
		Package:   htypes.Ptr(pkg),
	}.Build())
	if err != nil {

		return nil, err
	}

	strm = pluginsdk.WithOnErr(strm, handleProviderErr)

	return strm, nil
}

func (e *Engine) Get(ctx context.Context, rs *RequestState, p EngineProvider, ref *pluginv1.TargetRef, states []*pluginv1.ProviderState) (*pluginv1.GetResponse, error) {
	res, err := p.Get(ctx, pluginv1.GetRequest_builder{
		RequestId: htypes.Ptr(rs.ID),
		Ref:       ref,
		States:    states,
	}.Build())
	if err != nil {
		return nil, handleProviderErr(err)
	}

	return res, nil
}

func handleProviderErr(err error) error {
	var serr pluginsdk.StackRecursionError
	if errors.As(err, &serr) {
		return StackRecursionError{printer: func() string {
			return serr.Stack
		}}
	}

	return err
}
