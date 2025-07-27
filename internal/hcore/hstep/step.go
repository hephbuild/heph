package hstep

import (
	"context"
	"github.com/hephbuild/heph/internal/htypes"
	"sync/atomic"
	"time"

	"github.com/hephbuild/heph/internal/hproto"

	"connectrpc.com/connect"
	"github.com/google/uuid"
	"github.com/hephbuild/heph/internal/hcore/hlog"
	corev1 "github.com/hephbuild/heph/plugin/gen/heph/core/v1"
	"github.com/hephbuild/heph/plugin/gen/heph/core/v1/corev1connect"
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

	return hproto.Clone(s.pbstep)
}

func (s *Step) SetText(text string) {
	if s.pbstep == nil {
		return
	}

	pbstep := s.getPbStep()
	pbstep.SetText( text )

	s.pbstep = s.handleStep(s.ctx, pbstep)
}

func (s *Step) SetError() {
	if s.pbstep == nil {
		return
	}

	pbstep := s.getPbStep()
	pbstep.SetError(true)

	s.pbstep = s.handleStep(s.ctx, pbstep)
}

func (s *Step) Done() {
	if s.pbstep == nil {
		return
	}

	pbstep := s.getPbStep()
	pbstep.SetStatus(corev1.Step_STATUS_COMPLETED)
	pbstep.CompletedAt = timestamppb.New(time.Now())

	s.pbstep = s.handleStep(s.ctx, pbstep)
}

func (s *Step) GetID() string {
	if s.pbstep == nil {
		return ""
	}

	return s.pbstep.GetId()
}

type ctxStepKey struct{}
type ctxHandlerKey struct{}

type Handler func(ctx context.Context, pbstep *corev1.Step) *corev1.Step

func ContextWithRPCHandler(ctx context.Context, client corev1connect.StepServiceClient) context.Context {
	ch := make(chan *corev1.Step, 1000)
	var running atomic.Bool

	startRoutine := func() {
		if running.Swap(true) {
			return
		}

		ctx := context.WithoutCancel(ctx)
		t := time.NewTimer(time.Second)
		defer t.Stop()

		go func() {
			defer running.Store(false)

			for {
				t.Reset(time.Second)

				select {
				case <-t.C:
					return
				case step := <-ch:
					_, err := client.Create(ctx, connect.NewRequest(&corev1.StepServiceCreateRequest{Step: step}))
					if err != nil {
						hlog.From(ctx).Error(err.Error())
					}
				}
			}
		}()
	}

	return ContextWithHandler(ctx, func(ctx context.Context, pbstep *corev1.Step) *corev1.Step {
		startRoutine()

		ch <- pbstep

		return pbstep
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
		return &Step{}
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
			Id: htypes.Ptr(parentID),
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
			Id:        htypes.Ptr(uuid.New().String()),
			ParentId:  htypes.Ptr(parentID),
			Text:      htypes.Ptr(str),
			Status:    htypes.Ptr(corev1.Step_STATUS_RUNNING),
			StartedAt: timestamppb.New(time.Now()),
		},
	}

	handler(ctx, step.pbstep)

	ctx = context.WithValue(ctx, ctxStepKey{}, step)

	return step, ctx
}
