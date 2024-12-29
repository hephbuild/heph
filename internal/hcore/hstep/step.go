package hstep

import (
	"context"
	"time"

	"connectrpc.com/connect"
	"github.com/google/uuid"
	"github.com/hephbuild/hephv2/internal/hcore/hlog"
	corev1 "github.com/hephbuild/hephv2/plugin/gen/heph/core/v1"
	"github.com/hephbuild/hephv2/plugin/gen/heph/core/v1/corev1connect"
	"google.golang.org/protobuf/types/known/timestamppb"
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
	pbstep.CompletedAt = timestamppb.New(time.Now())

	s.pbstep = s.handleStep(s.ctx, pbstep)
}

func (s *Step) GetID() string {
	return s.pbstep.GetId()
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

		return res.Msg.GetStep()
	})
}

func ContextWithHandler(ctx context.Context, handler Handler) context.Context {
	return context.WithValue(ctx, ctxHandlerKey{}, handler)
}

func HandlerFromContext(ctx context.Context) Handler {
	handler, ok := ctx.Value(ctxHandlerKey{}).(Handler)
	if !ok {
		handler = func(ctx context.Context, step *corev1.Step) *corev1.Step {
			return step
		}
	}

	return handler
}

func From(ctx context.Context) *Step {
	parent, ok := ctx.Value(ctxStepKey{}).(*Step)
	if !ok {
		return &Step{pbstep: &corev1.Step{}}
	}

	return parent
}

func WithoutParent(ctx context.Context) context.Context {
	return context.WithValue(ctx, ctxStepKey{}, nil)
}

func ContextWithParentID(ctx context.Context, parentID string) context.Context {
	handler := HandlerFromContext(ctx)

	step := &Step{
		ctx:        context.WithoutCancel(ctx),
		handleStep: handler,
		pbstep: &corev1.Step{
			Id: parentID,
		},
	}

	ctx = context.WithValue(ctx, ctxStepKey{}, step)

	return ctx
}

func New(ctx context.Context, str string) (*Step, context.Context) {
	handler := HandlerFromContext(ctx)

	var parentID string
	if parent := From(ctx); parent.GetID() != "" {
		parentID = parent.GetID()
	}

	step := &Step{
		ctx:        context.WithoutCancel(ctx),
		handleStep: handler,
		pbstep: &corev1.Step{
			Id:        uuid.New().String(),
			ParentId:  parentID,
			Text:      str,
			Status:    corev1.Step_STATUS_RUNNING,
			StartedAt: timestamppb.New(time.Now()),
		},
	}

	handler(ctx, step.pbstep)

	ctx = context.WithValue(ctx, ctxStepKey{}, step)

	return step, ctx
}
