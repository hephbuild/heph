package engine

import (
	"context"
	engine2 "github.com/hephbuild/heph/lib/engine"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/tref"
)

func (e *Engine) List(ctx context.Context, p EngineProvider, pkg string, rs *RequestState) (engine2.HandlerStreamReceive[*pluginv1.ListResponse], error) {
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

	return engine2.WithOnCloseReceive(strm, clean), nil
}

func (e *Engine) Get(ctx context.Context, p EngineProvider, ref *pluginv1.TargetRef, states []*pluginv1.ProviderState, rs *RequestState) (*pluginv1.GetResponse, error) {
	rs, err := rs.Trace("Get", tref.Format(ref))
	if err != nil {
		return nil, err
	}

	clean := e.StoreRequestState(rs)
	defer clean()

	return p.Get(ctx, &pluginv1.GetRequest{
		RequestId: rs.ID,
		Ref:       ref,
		States:    states,
	})
}
