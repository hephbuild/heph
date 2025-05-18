package hsoftcontext

import (
	"connectrpc.com/connect"
	"context"
	"errors"
	"fmt"
	cache "github.com/Code-Hex/go-generics-cache"
	"github.com/google/uuid"
	"github.com/hephbuild/heph/internal/hcore/hlog"
	corev1 "github.com/hephbuild/heph/plugin/gen/heph/core/v1"
	"io"
)

type Handler struct {
	ctxs *cache.Cache[string, *ctxContainer]
}

func (e *Handler) NewContext(ctx context.Context, strm *connect.BidiStream[corev1.NewContextRequest, corev1.NewContextResponse]) error {
	id := uuid.New().String()

	ictx, icancel := WithCancel(context.Background())
	c := &ctxContainer{
		ctx: ictx,
	}
	e.ctxs.Set(id, c)

	defer func() {
		icancel(errors.New("cleanup"))
		e.ctxs.Delete(id)
	}()

	err := strm.Send(&corev1.NewContextResponse{
		Id: id,
	})
	if err != nil {
		return err
	}

	for {
		req, err := strm.Receive()
		if err != nil {
			if errors.Is(err, io.EOF) {
				// client closed
				break
			}

			break
		}

		switch msg := req.Msg.(type) {
		case *corev1.NewContextRequest_Cancel_:
			if msg.Cancel.Cause == "" {
				continue
			}

			icancel(errors.New(msg.Cancel.Cause))
		}
	}

	return nil
}

func (e *Handler) ContextDone(ctx context.Context, req *connect.Request[corev1.ContextDoneRequest], res *connect.ServerStream[corev1.ContextDoneResponse]) error {
	c, ok := e.ctxs.Get(req.Msg.Id)
	if !ok {
		return fmt.Errorf("unknown request id: %q", req.Msg.Id)
	}

	err := res.Send(&corev1.ContextDoneResponse{Err: ""})
	if err != nil {
		hlog.From(ctx).Error("control context: error sending response", "error", err)
	}

	select {
	case <-ctx.Done():
		return nil
	case <-c.ctx.Done():
	}

	err = res.Send(&corev1.ContextDoneResponse{Err: c.ctx.Err().Error()})
	if err != nil {
		hlog.From(ctx).Error("control context: error sending response", "error", err)
	}

	return nil
}

type ctxContainer struct {
	ctx context.Context
}

func NewHandler() *Handler {
	return &Handler{ctxs: cache.New[string, *ctxContainer]()}
}
