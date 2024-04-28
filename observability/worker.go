package observability

import (
	"context"
	"github.com/hephbuild/heph/worker2"
)

type createSpanFunc[S any] func(job worker2.Dep) (context.Context, S)

type workerStageStore[S any] struct {
	span           S
	hasSpan        bool
	createSpanFunc createSpanFunc[S]
}

func (s *workerStageStore[S]) createSpan(job worker2.Dep) (context.Context, S) {
	ctx, span := s.createSpanFunc(job)
	s.span = span
	s.hasSpan = true

	return ctx, span
}

func WorkerStageFactory[S SpanError](f createSpanFunc[S]) worker2.Hook {
	ss := &workerStageStore[S]{createSpanFunc: f}
	return worker2.StageHook{
		OnScheduled: func(job worker2.Dep) context.Context {
			ctx, span := ss.createSpan(job)
			span.SetScheduledTime(job.GetScheduledAt())

			return ctx
		},
		OnQueued: func(job worker2.Dep) context.Context {
			ss.span.SetQueuedTime(job.GetQueuedAt())
			return nil
		},
		OnStart: func(job worker2.Dep) context.Context {
			var ctx context.Context
			if !ss.hasSpan {
				ctx, _ = ss.createSpan(job)
			}

			ss.span.SetStartTime(job.GetStartedAt())
			return ctx
		},
		OnEnd: func(job worker2.Dep) context.Context {
			if err := job.GetErr(); err != nil {
				state := StateFailed
				if job.GetState() == worker2.ExecStateSkipped {
					state = StateSkipped
				}
				ss.span.EndErrorState(err, state)
			} else {
				ss.span.End()
			}
			return nil
		},
	}.Hook()
}
