package queue

import (
	"context"
	"github.com/hephbuild/heph/log/log"
	"sync"
	"time"
)

type Queue[T any] struct {
	Max                 int
	vs                  []T
	m                   sync.Mutex
	DisableRescheduling bool
}

func (q *Queue[T]) Enqueue(vs ...T) {
	q.m.Lock()
	defer q.m.Unlock()

	q.vs = append(q.vs, vs...)

	if q.Max > 0 && len(q.vs) > q.Max {
		offset := len(q.vs) - q.Max
		log.Default().Tracef("discarding %v elements from %T", offset, q)
		q.vs = q.vs[offset:]
	}
}

func (q *Queue[T]) DequeueChunk(chunkSize int, f func(vs []T) error) error {
	return q.DequeueChunkContext(context.Background(), chunkSize, f)
}

func (q *Queue[T]) DequeueChunkContext(ctx context.Context, chunkSize int, f func(vs []T) error) error {
	for {
		select {
		case <-ctx.Done():
			return ctx.Err()
		default:
		}

		vs := q.Dequeue(chunkSize)

		if len(vs) == 0 {
			return nil
		}
		err := f(vs)
		if err != nil {
			// Requeue...
			q.m.Lock()
			if !q.DisableRescheduling {
				q.vs = append(vs, q.vs...)
			}
			q.m.Unlock()
			return err
		}
	}
}

func (q *Queue[T]) Dequeue(chunkSize int) []T {
	q.m.Lock()
	defer q.m.Unlock()

	vs := q.vs
	if len(vs) > chunkSize {
		vs = vs[:chunkSize]
	}
	q.vs = q.vs[len(vs):]

	return vs
}

func (q *Queue[T]) Len() int {
	q.m.Lock()
	defer q.m.Unlock()
	return len(q.vs)
}

func (q *Queue[T]) Empty() <-chan struct{} {
	ch := make(chan struct{})

	go func() {
		t := time.NewTicker(time.Second)
		defer t.Stop()

		for range t.C {
			if q.Len() == 0 {
				close(ch)
				return
			}
		}
	}()

	return ch
}
