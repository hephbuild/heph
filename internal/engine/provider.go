package engine

import (
	"context"
	"errors"
	engine2 "github.com/hephbuild/heph/lib/engine"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/tref"
)

func (e *Engine) List(ctx context.Context, rs *RequestState, p EngineProvider, pkg string) (engine2.HandlerStreamReceive[*pluginv1.ListResponse], error) {
	rs, err := rs.TraceList(p.Name, pkg)
	if err != nil {
		return nil, err
	}

	clean := e.StoreRequestState(rs)

	strm, err := p.List(ctx, &pluginv1.ListRequest{
		RequestId: rs.ID,
		Package:   pkg,
	})
	if err != nil {
		clean()

		return nil, err
	}

	strm = engine2.WithOnCloseReceive(strm, clean)
	strm = engine2.WithOnErr(strm, handleProviderErr)

	return strm, nil
}

func (e *Engine) Get(ctx context.Context, rs *RequestState, p EngineProvider, ref *pluginv1.TargetRef, states []*pluginv1.ProviderState) (*pluginv1.GetResponse, error) {
	rs, err := rs.Trace("Get", tref.Format(ref))
	if err != nil {
		return nil, err
	}

	clean := e.StoreRequestState(rs)
	defer clean()

	res, err := p.Get(ctx, &pluginv1.GetRequest{
		RequestId: rs.ID,
		Ref:       ref,
		States:    states,
	})
	if err != nil {
		return nil, handleProviderErr(err)
	}

	return res, nil
}

func handleProviderErr(err error) error {
	var serr engine2.ErrStackRecursion
	if errors.As(err, &serr) {
		return ErrStackRecursion{printer: func() string {
			return serr.Stack
		}}
	}

	return err
}
