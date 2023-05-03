package queue

import (
	"github.com/hephbuild/heph/log/log"
	"sync"
)

type Queue[T any] struct {
	Max int
	vs  []T
	m   sync.Mutex
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
	for {
		vs := q.Dequeue(chunkSize)

		if len(vs) == 0 {
			return nil
		}
		err := f(vs)
		if err != nil {
			// Requeue...
			q.m.Lock()
			q.vs = append(vs, q.vs...)
			q.m.Unlock()
			return err
		}
	}
}

func (q *Queue[T]) Dequeue(chunkSize int) []T {
	q.m.Lock()
	vs := q.vs
	if len(vs) > chunkSize {
		vs = vs[:chunkSize]
	}
	q.vs = q.vs[len(vs):]
	q.m.Unlock()

	return vs
}
