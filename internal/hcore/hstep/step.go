package hstep

import (
	"connectrpc.com/connect"
	"context"
	"github.com/hephbuild/hephv2/internal/hcore/hlog"
	corev1 "github.com/hephbuild/hephv2/plugin/gen/heph/core/v1"
	"github.com/hephbuild/hephv2/plugin/gen/heph/core/v1/corev1connect"
)

type Step struct {
	ctx        context.Context
	pbstep     *corev1.Step
	handleStep Handler
}

func (s *Step) getPbStep() *corev1.Step {
	if s.pbstep == nil {
		return &corev1.Step{}
	}

	return s.pbstep
}

func (s *Step) SetText(text string) {
	if s.pbstep == nil {
		return
	}

	pbstep := s.getPbStep()
	pbstep.Text = text

	s.pbstep = s.handleStep(s.ctx, pbstep)
}

func (s *Step) SetError() {
	if s.pbstep == nil {
		return
	}

	pbstep := s.getPbStep()
	pbstep.Error = true

	s.pbstep = s.handleStep(s.ctx, pbstep)
}

func (s *Step) Done() {
	if s.pbstep == nil {
		return
	}

	pbstep := s.getPbStep()
	pbstep.Status = corev1.Step_STATUS_COMPLETED

	s.pbstep = s.handleStep(s.ctx, pbstep)
}

type ctxStepKey struct{}
type ctxHandlerKey struct{}

type Handler func(ctx context.Context, pbstep *corev1.Step) *corev1.Step

func ContextWithRPCHandler(ctx context.Context, client corev1connect.StepServiceClient) context.Context {
	return ContextWithHandler(ctx, func(ctx context.Context, pbstep *corev1.Step) *corev1.Step {
		res, err := client.Create(ctx, connect.NewRequest(&corev1.StepServiceCreateRequest{Step: pbstep}))
		if err != nil {
			hlog.From(ctx).Error(err.Error())
			return pbstep
		}

		return res.Msg.Step
	})
}

func ContextWithHandler(ctx context.Context, handler Handler) context.Context {
	return context.WithValue(ctx, ctxHandlerKey{}, handler)
}

func HandlerFromContext(ctx context.Context) Handler {
	handler, ok := ctx.Value(ctxHandlerKey{}).(Handler)
	if !ok {
		handler = func(ctx context.Context, pbstep *corev1.Step) *corev1.Step {
			return pbstep
		}
	}

	return handler
}

func New(ctx context.Context, str string) (*Step, context.Context) {
	handler := HandlerFromContext(ctx)

	var parentId string
	if parent, ok := ctx.Value(ctxStepKey{}).(*Step); ok {
		if parent.pbstep != nil {
			parentId = parent.pbstep.Id
		}
	}

	step := &Step{
		ctx:        context.WithoutCancel(ctx),
		handleStep: handler,
		pbstep: &corev1.Step{
			ParentId: parentId,
			Text:     str,
			Status:   corev1.Step_STATUS_RUNNING,
		},
	}

	handler(ctx, step.pbstep)

	ctx = context.WithValue(ctx, ctxStepKey{}, step)

	return step, ctx
}
