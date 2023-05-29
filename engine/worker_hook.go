package engine

import (
	"context"
	"github.com/hephbuild/heph/observability"
	"github.com/hephbuild/heph/worker"
)

type createSpanFunc[S any] func(job *worker.Job) (context.Context, S)

type workerStageStore[S any] struct {
	span           S
	hasSpan        bool
	createSpanFunc createSpanFunc[S]
}

func (s *workerStageStore[S]) createSpan(job *worker.Job) (context.Context, S) {
	ctx, span := s.createSpanFunc(job)
	s.span = span
	s.hasSpan = true

	return ctx, span
}

func WorkerStageFactory[S observability.SpanError](f createSpanFunc[S]) worker.Hook {
	ss := &workerStageStore[S]{createSpanFunc: f}
	return worker.StageHook{
		OnScheduled: func(job *worker.Job) context.Context {
			ctx, span := ss.createSpan(job)
			span.SetScheduledTime(job.TimeScheduled)

			return ctx
		},
		OnQueued: func(job *worker.Job) context.Context {
			ss.span.SetQueuedTime(job.TimeQueued)
			return nil
		},
		OnStart: func(job *worker.Job) context.Context {
			var ctx context.Context
			if !ss.hasSpan {
				ctx, _ = ss.createSpan(job)
			}

			ss.span.SetStartTime(job.TimeStart)
			return ctx
		},
		OnEnd: func(job *worker.Job) context.Context {
			if err := job.Err(); err != nil {
				state := observability.StateFailed
				if job.State == worker.StateSkipped {
					state = observability.StateSkipped
				}
				ss.span.EndErrorState(err, state)
			} else {
				ss.span.End()
			}
			return nil
		},
	}
}
